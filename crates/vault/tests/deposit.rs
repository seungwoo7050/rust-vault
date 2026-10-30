//! 불변조건 표 밖의 입금 스캐너 테스트(`--features db`). 정지 유지, 되감기 중 조회 실패, 입금이 아닌 tx,
//! 고아였던 블록의 재등장.

#[path = "support/scan.rs"]
mod scan;

use vault::chain::BlockNumber;
use vault::deposit::{ScanError, Step};
use vault::ledger::AccountKey;
use vault_sim::chain::{Call, Fault, failed_transfer, transfer};

use scan::Env;

/// (가) 확정된 입금이 빠지는 reorg로 정지한 뒤, 체인이 예전 분기로 돌아와 더 자라도 정지가 유지된다.
/// `Scanner`를 새로 만들어도(재시작) 정지는 DB에 남아 있고, 원장·커서·헤더·사고 기록이 바뀌지 않으며 체인을 묻지도 않는다.
#[tokio::test]
async fn halt_survives_restart_and_changes_nothing() {
    let env = Env::new().await;
    let chain = &env.chain;
    let g = chain.genesis().hash;
    let dep = chain.mine_canonical(g, vec![transfer(1, scan::addr(1), 100)]);
    let old_tip = scan::extend(chain, dep, (scan::N - 1) as usize);
    let mut scanner = env.scanner().await;
    scanner.run_until_idle().await.unwrap();

    // 깊이 N reorg → 정지
    scan::extend(chain, g, (scan::N + 1) as usize);
    let got = scanner.run_until_idle().await;
    assert!(matches!(got, Err(ScanError::Halted(_))), "{got:?}");
    let keys = env.keys().await;
    let balances = env.balances().await;
    let cursor = env.cursor().await;
    let headers = env.headers().await;
    assert_eq!(env.violations().await.len(), 1);

    // 체인이 예전 분기로 돌아와, 커서 다음에 입금 블록이 붙는다. 정지를 무시하면 이 입금을 처리한다
    chain.set_canonical(old_tip);
    chain.mine_canonical(old_tip, vec![transfer(2, scan::addr(1), 7)]);
    for _ in 0..2 {
        let mut restarted = env.scanner().await;
        let calls = chain.calls();
        let step = restarted.step().await;
        assert!(
            matches!(step, Err(ScanError::Halted(_))),
            "재시작 뒤 step: {step:?}"
        );
        let run = restarted.run_until_idle().await;
        assert!(
            matches!(run, Err(ScanError::Halted(_))),
            "재시작 뒤 run: {run:?}"
        );
        assert_eq!(chain.calls(), calls, "정지 상태에서 체인을 물었다");
    }
    assert!(env.halted().await.is_some());
    assert_eq!(env.keys().await, keys, "분개가 바뀌었다");
    assert_eq!(env.balances().await, balances, "잔액이 바뀌었다");
    assert_eq!(env.cursor().await, cursor, "커서가 바뀌었다");
    assert_eq!(env.headers().await, headers, "헤더가 바뀌었다");
    assert_eq!(env.violations().await.len(), 1, "사고 기록이 늘었다");
    env.db.drop_db().await;
}

/// (나) 되감기 도중 조회가 `Indeterminate`면 헤더·원장·커서가 하나도 바뀌지 않는다.
/// 회복 뒤에는 장애 없이 되감은 실행과 같은 결과가 된다.
#[tokio::test]
async fn indeterminate_during_rewind_writes_nothing() {
    async fn scenario(fault: bool) -> (Env, Vec<Step>) {
        let env = Env::new().await;
        let chain = &env.chain;
        // genesis → a1 → b2(입금) → b3까지 스캔
        let a1 = chain.mine_canonical(chain.genesis().hash, vec![]);
        let b2 = chain.mine_canonical(a1, vec![transfer(1, scan::addr(1), 100)]);
        chain.mine_canonical(b2, vec![]);
        let mut scanner = env.scanner().await;
        scanner.run_until_idle().await.unwrap();
        // a1에서 갈라진 c2 → c3 → c4. 되감기는 3, 2, 1번을 차례로 조회한다
        scan::extend(chain, a1, 3);
        let mut steps = vec![];
        if fault {
            let headers = env.headers().await;
            let keys = env.keys().await;
            let balances = env.balances().await;
            let cursor = env.cursor().await;
            // 2번 조회가 타임아웃
            chain.fail_next(Call::BlockAt(BlockNumber(2)), 1, Fault::Indeterminate);
            let got = scanner.step().await;
            assert!(
                matches!(got, Err(ScanError::Indeterminate(_))),
                "되감기 중 타임아웃: {got:?}"
            );
            assert_eq!(env.headers().await, headers, "헤더가 바뀌었다");
            assert_eq!(env.keys().await, keys, "분개가 바뀌었다");
            assert_eq!(env.balances().await, balances, "잔액이 바뀌었다");
            assert_eq!(env.cursor().await, cursor, "커서가 바뀌었다");
        }
        loop {
            match scanner.step().await.unwrap() {
                Step::Idle => break,
                s => steps.push(s),
            }
        }
        (env, steps)
    }

    let (clean, clean_steps) = scenario(false).await;
    let (faulty, faulty_steps) = scenario(true).await;
    assert_eq!(faulty_steps, clean_steps, "회복 뒤 step 열");
    assert!(matches!(
        faulty_steps[0],
        Step::Rewound {
            from: BlockNumber(3),
            to: BlockNumber(1)
        }
    ));
    assert_eq!(faulty.headers().await, clean.headers().await, "헤더");
    assert_eq!(faulty.keys().await, clean.keys().await, "분개");
    assert_eq!(faulty.balances().await, clean.balances().await, "잔액");
    assert_eq!(faulty.cursor().await, clean.cursor().await, "커서");
    assert_eq!(faulty.entries("dep:revert:").await, 1);
    clean.db.drop_db().await;
    faulty.db.drop_db().await;
}

/// (다) value 0 tx, 실패한 tx, 등록하지 않은 주소로 온 tx는 입금이 아니다.
#[tokio::test]
async fn zero_value_and_failed_txs_are_not_deposits() {
    let env = Env::new().await;
    let chain = &env.chain;
    let to = scan::addr(1);
    let b1 = chain.mine_canonical(
        chain.genesis().hash,
        vec![
            transfer(1, to, 0),
            failed_transfer(2, to, 100),
            transfer(3, scan::stranger(), 100),
            transfer(4, to, 5),
        ],
    );
    scan::extend(chain, b1, scan::N as usize);
    let mut scanner = env.scanner().await;
    scanner.run_until_idle().await.unwrap();

    let deposits: Vec<String> = sqlx::query_scalar("SELECT tx_hash FROM deposits")
        .fetch_all(&mut *env.conn().await)
        .await
        .unwrap();
    assert_eq!(deposits, vec![transfer(4, to, 5).hash.to_string()]);
    assert_eq!(env.entries("dep:").await, 2, "잠정 1 + 확정 1");
    assert_eq!(env.balance(&AccountKey::user(1)).await, 5);
    assert_eq!(env.balance(&AccountKey::pending_deposit(1)).await, 0);
    assert_eq!(env.balance(&AccountKey::hot_wallet(to)).await, 5);
    env.db.drop_db().await;
}

/// (라) 한 번 고아가 된 블록이 다시 canonical이 되면 정지한다. 그 블록의 잠정 입금은 이미 역분개됐고
/// 같은 멱등 키로는 다시 기록할 수 없어서, 넘어가면 canonical 입금을 조용히 잃는다(ADR-002 결정 10).
#[tokio::test]
async fn orphaned_block_coming_back_halts() {
    let env = Env::new().await;
    let chain = &env.chain;
    let g = chain.genesis().hash;
    // A: g → a1(입금) 스캔, B: g → b1 → b2로 바꿔 a1을 고아로
    let a1 = chain.mine_canonical(g, vec![transfer(1, scan::addr(1), 100)]);
    let mut scanner = env.scanner().await;
    scanner.run_until_idle().await.unwrap();
    scan::extend(chain, g, 2);
    scanner.run_until_idle().await.unwrap();
    assert_eq!(env.entries("dep:revert:").await, 1);
    // 다시 A: g → a1 → a2 → a3
    scan::extend(chain, a1, 2);
    let got = scanner.run_until_idle().await;
    assert!(matches!(got, Err(ScanError::Halted(_))), "{got:?}");
    assert!(env.halted().await.unwrap().contains("canonical again"));
    assert_eq!(env.balance(&AccountKey::pending_deposit(1)).await, 0);
    assert_eq!(env.trial_balance().await, 0);
    env.db.drop_db().await;
}
