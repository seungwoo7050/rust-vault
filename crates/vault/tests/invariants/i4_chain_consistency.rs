//! I4 블록체인 정합성: 입금 확정 여부는 현재 canonical chain과 확정 정책에 따라 결정한다

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};
use vault::chain::{Hash32, Notification};
use vault::deposit::{ScanError, Step};
use vault::ledger::AccountKey;
use vault_sim::chain::{shuffle, transfer};

use crate::model;
use crate::scan::{self, Env};

// T4.1 실패해야 할 변형: reorg 감지에서 부모 해시 비교를 빼고 블록 번호만 봄
#[tokio::test]
async fn reorg_before_confirmation_reverses_pending() {
    for depth in 1..scan::N {
        let env = Env::new().await;
        let chain = &env.chain;
        let pending = AccountKey::pending_deposit(1);
        // genesis → a1 → [입금 블록 → 빈 블록 depth−1개]. 커서가 tip이면 입금의 확정 수는 depth(< N)다
        let a1 = chain.mine_canonical(chain.genesis().hash, vec![]);
        let tx = transfer(1, scan::addr(1), 100);
        let dep_block = chain.mine_canonical(a1, vec![tx]);
        scan::extend(chain, dep_block, (depth - 1) as usize);
        let mut scanner = env.scanner().await;
        scanner.run_until_idle().await.unwrap();
        assert_eq!(
            env.balance(&pending).await,
            100,
            "깊이 {depth}: reorg 전 잠정 입금"
        );
        let user_before = env.balance(&AccountKey::user(1)).await;

        // a1에서 갈라진 depth + 1개짜리 새 분기(그 tx 없음)
        let new_tip = scan::extend(chain, a1, (depth + 1) as usize);
        scanner.run_until_idle().await.unwrap();

        let revert_key = format!("dep:revert:{}:{}:{}", scan::CHAIN_ID, tx.hash, dep_block);
        assert_eq!(
            env.entries(&revert_key).await,
            1,
            "깊이 {depth}: 그 입금의 역분개 수"
        );
        assert_eq!(
            env.entries("dep:revert:").await,
            1,
            "깊이 {depth}: 전체 역분개 수"
        );
        assert_eq!(
            env.balance(&pending).await,
            0,
            "깊이 {depth}: pending_deposit:1"
        );
        assert_eq!(
            env.balance(&AccountKey::user(1)).await,
            user_before,
            "깊이 {depth}: user:1이 바뀌었다"
        );
        assert_eq!(
            env.cursor().await.hash,
            new_tip,
            "깊이 {depth}: 커서가 새 분기 head가 아니다"
        );
        env.db.drop_db().await;
    }
}

// T4.2 실패해야 할 변형: 확정된 입금도 역분개
#[tokio::test]
async fn reorg_beyond_confirmation_keeps_ledger_and_halts() {
    let env = Env::new().await;
    let chain = &env.chain;
    // genesis → [입금 블록] → 빈 블록 N−1개: 입금 확정
    let tx = transfer(1, scan::addr(1), 100);
    let dep_block = chain.mine_canonical(chain.genesis().hash, vec![tx]);
    scan::extend(chain, dep_block, (scan::N - 1) as usize);
    let mut scanner = env.scanner().await;
    scanner.run_until_idle().await.unwrap();
    assert_eq!(
        env.balance(&AccountKey::user(1)).await,
        100,
        "reorg 전 확정"
    );

    let keys = env.keys().await;
    let balances = env.balances().await;
    let cursor = env.cursor().await;
    let headers = env.headers().await;

    // genesis에서 갈라진 N + 1개짜리 분기: 깊이 N의 reorg가 확정된 입금 블록을 뺀다
    scan::extend(chain, chain.genesis().hash, (scan::N + 1) as usize);
    let got = scanner.run_until_idle().await;
    assert!(
        matches!(got, Err(ScanError::Halted(_))),
        "정지해야 한다, 받은 것 {got:?}"
    );

    assert_eq!(env.keys().await, keys, "분개가 바뀌었다");
    assert_eq!(env.balances().await, balances, "잔액이 바뀌었다");
    assert_eq!(env.cursor().await, cursor, "커서가 바뀌었다");
    assert_eq!(env.headers().await, headers, "헤더가 바뀌었다");
    assert_eq!(
        env.violations().await,
        vec![(
            "deposit".to_owned(),
            dep_block.to_string(),
            tx.hash.to_string(),
            "100".to_owned()
        )],
        "finality_violations"
    );
    assert!(env.halted().await.is_some(), "정지 상태가 DB에 없다");
    let again = scanner.step().await;
    assert!(
        matches!(again, Err(ScanError::Halted(_))),
        "정지 뒤 step: {again:?}"
    );
    assert_eq!(
        env.violations().await.len(),
        1,
        "정지 뒤 사고 기록이 늘었다"
    );
    env.db.drop_db().await;
}

// T4.3 실패해야 할 변형: 잠정 입금 키에서 블록 해시를 뺌
#[tokio::test]
async fn reincluded_tx_confirms_once() {
    let env = Env::new().await;
    let chain = &env.chain;
    let addr = scan::addr(1);
    // genesis → a1 → b2(입금 tx). 스캔하면 잠정 입금
    let a1 = chain.mine_canonical(chain.genesis().hash, vec![]);
    let tx = transfer(1, addr, 100);
    chain.mine_canonical(a1, vec![tx]);
    let mut scanner = env.scanner().await;
    scanner.run_until_idle().await.unwrap();
    assert_eq!(env.balance(&AccountKey::pending_deposit(1)).await, 100);

    // a1에서 갈라진 분기: c2(빈 블록) → c3(같은 tx 재포함) → 빈 블록 N−1개(c3의 확정 수 N)
    let c2 = chain.mine_canonical(a1, vec![]);
    let c3 = chain.mine_canonical(c2, vec![tx]);
    scan::extend(chain, c3, (scan::N - 1) as usize);
    scanner
        .run_until_idle()
        .await
        .expect("스캔이 오류 없이 head까지 가야 한다");

    assert_eq!(
        env.cursor().await.hash,
        chain.tip().hash,
        "커서가 head가 아니다"
    );
    assert_eq!(
        env.balance(&AccountKey::user(1)).await,
        100,
        "user:1은 한 번만 늘어야 한다"
    );
    assert_eq!(
        env.balance(&AccountKey::pending_deposit(1)).await,
        0,
        "pending_deposit:1"
    );
    assert_eq!(
        env.balance(&AccountKey::hot_wallet(addr)).await,
        scan::deposits_to(&chain.canonical_blocks(), addr),
        "hot_wallet = canonical 입금 합"
    );
    assert_eq!(env.entries("dep:confirm:").await, 1, "확정 분개 수");
    env.db.drop_db().await;
}

// T4.4 실패해야 할 변형: 확정 수를 받은 블록 개수(고아 포함)로 셈
#[tokio::test]
async fn confirmation_counts_canonical_depth() {
    let env = Env::new().await;
    let chain = &env.chain;
    // genesis → x1까지 스캔한 뒤, genesis에서 갈라진 b1(입금) → b2로 바꾼다.
    // 되감기로 x1이 고아가 되고, 번호 1 이상의 헤더는 x1(고아), b1, b2 셋이다
    let g = chain.genesis().hash;
    chain.mine_canonical(g, vec![]);
    let mut scanner = env.scanner().await;
    scanner.run_until_idle().await.unwrap();
    let b1 = chain.mine_canonical(g, vec![transfer(1, scan::addr(1), 100)]);
    chain.mine_canonical(b1, vec![]);
    scanner.run_until_idle().await.unwrap();
    let orphans = env.headers().await.iter().filter(|h| h.3).count();
    assert_eq!(orphans, 1, "고아 헤더 수");
    // canonical 확정 수 2 = N − 1
    assert_eq!(env.cursor().await.number.0, scan::N - 1);
    assert_eq!(
        env.entries("dep:confirm:").await,
        0,
        "확정 수 N−1에서 확정 분개"
    );
    assert_eq!(env.balance(&AccountKey::user(1)).await, 0);

    // 블록 하나 더: 확정 수 N
    scan::extend(chain, chain.tip().hash, 1);
    scanner.run_until_idle().await.unwrap();
    assert_eq!(
        env.entries("dep:confirm:").await,
        1,
        "확정 수 N에서 확정 분개"
    );
    assert_eq!(env.balance(&AccountKey::user(1)).await, 100);
    env.db.drop_db().await;
}

// T4.5 실패해야 할 변형: 알림으로 받은 블록을 "부모 해시 = 커서 해시" 검사 없이 바로 처리(끌어오기 대신 밀어넣기)
#[test]
fn event_order_does_not_change_final_ledger() {
    let rt = model::runtime();
    // 같은 블록 집합을 순서대로 전달한 실행이 기준이다
    let reference = rt.block_on(async {
        let in_order: Vec<usize> = (0..BLOCKS).collect();
        run_delivery(&in_order).await.expect("순서대로 전달한 실행")
    });
    let mut runner = TestRunner::new(model::config(12));
    // 알림 순서(섞기 씨앗)와 중복으로 더 보낼 알림들
    let input = (any::<u64>(), vec(0..BLOCKS, 0..6));
    runner
        .run(&input, |(seed, dups)| {
            let mut order: Vec<usize> = (0..BLOCKS).chain(dups).collect();
            shuffle(&mut order, seed);
            let got = rt
                .block_on(run_delivery(&order))
                .map_err(TestCaseError::fail)?;
            prop_assert_eq!(
                &got,
                &reference,
                "최종 원장이 순서대로 전달한 실행과 다르다"
            );
            Ok(())
        })
        .unwrap();
}

/// 블록 수와 블록마다 넣는 입금 (사용자 순번, 금액).
const BLOCKS: usize = 7;
fn block_deposits(i: usize) -> Vec<(usize, u128)> {
    match i {
        0 => vec![(0, 100)],
        1 => vec![(0, 5), (1, 7)],
        3 => vec![(1, 40)],
        4 => vec![(0, 3)],
        6 => vec![(1, 9)],
        _ => vec![],
    }
}

/// 최종 원장: (멱등 키 목록, 계정 잔액).
type Ledger = (Vec<String>, Vec<(String, i128)>);

/// 한 줄짜리 체인을 미리 캐 두고, `order` 순서로 그 블록들의 새 head 알림을 전달한다.
/// 알림이 오기 전에는 그 블록이 체인에 안 보이므로, 알림마다 canonical tip을 지금까지 알린 가장 높은 블록으로 둔다.
async fn run_delivery(order: &[usize]) -> Result<Ledger, String> {
    let env = Env::new().await;
    let result = delivery(&env, order).await;
    env.db.drop_db().await;
    result
}

async fn delivery(env: &Env, order: &[usize]) -> Result<Ledger, String> {
    let chain = &env.chain;
    let mut hashes: Vec<Hash32> = vec![];
    let mut confirm_at: Vec<Vec<String>> = vec![];
    let mut parent = chain.genesis().hash;
    let mut next_id = 0;
    for i in 0..BLOCKS {
        let txs: Vec<_> = block_deposits(i)
            .into_iter()
            .map(|(u, v)| {
                next_id += 1;
                transfer(next_id, scan::addr(scan::USERS[u]), v)
            })
            .collect();
        confirm_at.push(
            txs.iter()
                .map(|t| format!("dep:confirm:{}:{}", scan::CHAIN_ID, t.hash))
                .collect(),
        );
        parent = chain.mine(parent, txs);
        hashes.push(parent);
    }

    let mut scanner = env.scanner().await;
    let mut shown = 0usize; // 체인에 보이는 가장 높은 블록 번호
    for (k, &i) in order.iter().enumerate() {
        let number = i + 1;
        if number > shown {
            shown = number;
            chain.set_canonical(hashes[i]);
        }
        chain.notifications();
        let note = Notification::NewHead(chain.header(hashes[i]));
        scanner
            .on_notification(note)
            .await
            .map_err(|e| format!("{k}번째 알림(블록 {number}): 스캔 오류 {e}"))?;

        // 저장 헤더가 부모 해시로 이어진 한 줄이고, 커서는 보이는 head까지 번호마다 하나씩 왔다
        env.check_stored_chain()
            .await
            .map_err(|e| format!("{k}번째 알림(블록 {number}): {e}"))?;
        let cursor = env.cursor().await;
        if cursor.number.0 as usize != shown {
            return Err(format!(
                "{k}번째 알림: 커서 {}, 보이는 head {shown}",
                cursor.number
            ));
        }
        if env.entries("dep:revert:").await != 0 {
            return Err(format!("{k}번째 알림: 역분개가 생겼다"));
        }
        // 확정 분개는 그 블록 위로 N−1개가 처리된 블록의 입금에만 있다
        let mut want: Vec<String> = confirm_at
            .iter()
            .enumerate()
            .filter(|(j, _)| (j + 1) as u64 + scan::N - 1 <= cursor.number.0)
            .flat_map(|(_, keys)| keys.clone())
            .collect();
        want.sort();
        let got: Vec<String> = env
            .keys()
            .await
            .into_iter()
            .filter(|k| k.starts_with("dep:confirm:"))
            .collect();
        if got != want {
            return Err(format!("{k}번째 알림: 확정 분개 {got:?}, 기대 {want:?}"));
        }
    }
    if scanner.step().await.map_err(|e| e.to_string())? != Step::Idle {
        return Err("모든 알림 뒤에도 할 일이 남았다".into());
    }
    Ok((env.keys().await, env.balances().await.into_iter().collect()))
}
