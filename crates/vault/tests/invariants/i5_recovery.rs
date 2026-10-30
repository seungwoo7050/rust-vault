//! I5 복구 가능성: 어떤 처리 단계에서 프로세스가 종료되어도 재시작 후 일관된 상태로 복구한다
//!
//! T5.1·T5.2는 같은 시나리오(고정 하나 + proptest로 만든 몇 개)를 쓴다. 먼저 장애 없이 한 번 돌려(기준)
//! 스캔마다의 상태와 주입 지점별 통과 횟수를 얻고, (지점, k)마다 새 DB에서 k번째 통과에 장애를 넣어 다시 돌린다.

use std::collections::BTreeMap;

use proptest::collection::vec;
use proptest::strategy::{Strategy, ValueTree};
use proptest::test_runner::TestRunner;
use vault::deposit::ScanError;
use vault::store::{FaultPoint, Injected, Site};

use crate::history::{self, Op, Script};
use crate::model;
use crate::scan::{self, Env};

// T5.1 실패해야 할 변형: 커서 전진을 원장 반영보다 먼저 별도 트랜잭션으로 커밋("커서 커밋 직후" 지점에서 죽으면 그 블록의 입금이 빠진다)
#[test]
fn crash_at_each_injection_point_matches_clean_run() {
    model::runtime().block_on(async {
        let mut total = 0;
        for (n, seq) in scenarios().iter().enumerate() {
            let clean = run(seq, None)
                .await
                .unwrap_or_else(|e| panic!("시나리오 {n} 기준 실행: {e}"));
            let want: usize = clean.hits.values().sum();
            let mut combos = 0;
            // 장애 실행 쪽에서 센 "실제로 낸 종료" 수의 합
            let mut fired = 0;
            for (&point, &count) in &clean.hits {
                for k in 1..=count {
                    let got = run(seq, Some((point, k, Injected::Crash)))
                        .await
                        .unwrap_or_else(|e| panic!("시나리오 {n} {point} {k}번째에서 종료: {e}"));
                    assert_eq!(
                        got.restarts, 1,
                        "시나리오 {n} {point} {k}번째: 주입한 종료가 한 번 나고 재시작해야 한다"
                    );
                    assert_same(
                        &clean,
                        &got,
                        &format!("시나리오 {n} {point} {k}번째에서 종료 후 재시작"),
                    );
                    fired += got.fired;
                    combos += 1;
                }
            }
            assert!(combos > 0, "시나리오 {n}: 돈 조합이 없다");
            assert_eq!(
                fired, want,
                "시나리오 {n}: 실제로 낸 종료 수의 합 = 기준 실행의 지점별 통과 횟수 합"
            );
            total += combos;
        }
        println!("T5.1: (지점, k) 조합 {total}개 실행");
    });
}

// T5.2 실패해야 할 변형: DB 오류를 "처리할 수 없는 블록"으로 분류해 재시도하지 않고, DB 커서를 그 블록으로 직접 전진
#[test]
fn transient_db_error_leaves_no_partial_state() {
    let points = [
        FaultPoint::BlockEntriesWritten,
        FaultPoint::BeforeCommit(Site::Block),
        FaultPoint::AfterCommit(Site::Block),
    ];
    model::runtime().block_on(async {
        let mut total = 0;
        for (n, seq) in scenarios().iter().enumerate() {
            let clean = run(seq, None)
                .await
                .unwrap_or_else(|e| panic!("시나리오 {n} 기준 실행: {e}"));
            for point in points {
                let count = clean.hits.get(&point).copied().unwrap_or(0);
                assert!(
                    count > 0,
                    "시나리오 {n}: 기준 실행이 {point}를 지나지 않았다"
                );
                for k in 1..=count {
                    // 재시작 없이 같은 프로세스의 run_until_idle만으로 끝나야 한다
                    let got = run(seq, Some((point, k, Injected::Transient)))
                        .await
                        .unwrap_or_else(|e| panic!("시나리오 {n} {point} {k}번째 일시 오류: {e}"));
                    assert_eq!(got.restarts, 0, "시나리오 {n} {point} {k}번째: 재시작");
                    assert_eq!(
                        got.fired, 1,
                        "시나리오 {n} {point} {k}번째: 주입한 일시 오류가 한 번 나야 한다"
                    );
                    // 그 블록 처리를 한 번 더 했다(일시 오류 뒤 같은 step을 다시 함)
                    assert_eq!(
                        got.hits.get(&point).copied().unwrap_or(0),
                        count + 1,
                        "시나리오 {n} {point} {k}번째: 일시 오류 뒤 그 블록을 다시 처리해야 한다"
                    );
                    assert_same(
                        &clean,
                        &got,
                        &format!("시나리오 {n} {point} {k}번째 일시 오류"),
                    );
                    total += 1;
                }
            }
        }
        println!("T5.2: (지점, k) 조합 {total}개 실행");
    });
}

// T5.3 실패해야 할 변형: 재시작 때 `Signed` 출금에 nonce를 새로 할당해 다시 서명(같은 nonce로 다시 서명하면 결정적 서명이라 같은 바이트가 나와 변형이 안 된다)
#[test]
#[ignore = "05-withdrawal"]
fn restart_after_sign_resends_same_tx() {
    unimplemented!(
        "T5.3: 서명 tx 커밋 직후, 전송 직후 종료 후 재시작. 단언: 출금마다 체인 지급 1회, 보낸 raw tx가 저장본과 같다"
    );
}

/// 고정 시나리오 하나와 proptest로 만든 시나리오 몇 개. 끝에는 언제나 빈 블록 N개와 스캔을 붙여 모두 확정한다.
/// 생성기는 결정적 RNG를 쓴다(실행마다 같은 시나리오, 같은 조합 수).
fn scenarios() -> Vec<Vec<Op>> {
    // 입금 두 블록 → 스캔 → 깊이 1 reorg(빠진 tx 하나 재포함 + 새 입금) → 스캔 → 블록 둘 → 스캔
    // → 깊이 2 reorg(빠진 입금은 재포함 없음, 역분개만) → 스캔. 그사이 앞 입금들이 확정된다
    let fixed = vec![
        Op::Mine(vec![(0, 100)]),
        Op::Mine(vec![(1, 70), (0, 50)]),
        Op::Scan,
        Op::Reorg {
            depth: 1,
            extra: 1,
            deposits: vec![(1, 30)],
            reinclude: 1,
        },
        Op::Scan,
        Op::Mine(vec![]),
        Op::Mine(vec![(0, 5)]),
        Op::Scan,
        Op::Reorg {
            depth: 2,
            extra: 0,
            deposits: vec![],
            reinclude: 0,
        },
        Op::Scan,
    ];
    let mut out = vec![fixed];
    let mut runner = TestRunner::deterministic();
    let strategy = vec(history::op(), 1..10);
    for _ in 0..3 {
        let mut seq = vec![Op::Mine(vec![(0, 10)]), Op::Scan];
        seq.extend(strategy.new_tree(&mut runner).unwrap().current());
        out.push(seq);
    }
    for seq in &mut out {
        seq.extend((0..scan::N).map(|_| Op::Mine(vec![])));
        seq.push(Op::Scan);
    }
    out
}

/// 한 번 돈 결과.
struct Run {
    /// 스캔마다(시나리오의 `Op::Scan` 순서대로) 끝난 뒤의 상태
    states: Vec<State>,
    /// 주입 지점별 통과 횟수
    hits: BTreeMap<FaultPoint, usize>,
    /// 주입된 종료 뒤 스캐너를 새로 만든 횟수
    restarts: usize,
    /// 장애 계획이 실제로 장애를 낸 횟수
    fired: usize,
}

/// 비교하는 상태: 분개 키, 잔액, 커서, `deposits`, `block_headers`, 정지 여부.
#[derive(Debug, PartialEq, Eq)]
struct State {
    keys: Vec<String>,
    balances: BTreeMap<String, i128>,
    cursor: (u64, String),
    deposits: Vec<(String, String, i32, String, i64, String, String)>,
    headers: Vec<(i64, String, String, bool)>,
    halted: Option<String>,
}

async fn state(env: &Env) -> State {
    let cursor = env.cursor().await;
    State {
        keys: env.keys().await,
        balances: env.balances().await,
        cursor: (cursor.number.0, cursor.hash.to_string()),
        deposits: sqlx::query_as(
            "SELECT tx_hash, block_hash, position, address, user_id, amount::text, status
               FROM deposits ORDER BY tx_hash, block_hash",
        )
        .fetch_all(&mut *env.conn().await)
        .await
        .unwrap(),
        headers: env.headers().await,
        halted: env.halted().await,
    }
}

/// 새 DB에서 시나리오를 돈다. `fault`가 있으면 그 지점의 k번째 통과에 장애를 한 번 넣는다.
/// 주입된 종료(`Crashed`)가 나면 스캐너를 버리고 새로 만든 뒤(재시작) 그 스캔을 다시 한다. 다른 오류는 실패다.
async fn run(seq: &[Op], fault: Option<(FaultPoint, usize, Injected)>) -> Result<Run, String> {
    let env = Env::new().await;
    let result = drive(&env, seq, fault).await;
    env.db.drop_db().await;
    result
}

async fn drive(
    env: &Env,
    seq: &[Op],
    fault: Option<(FaultPoint, usize, Injected)>,
) -> Result<Run, String> {
    if let Some((point, k, f)) = fault {
        env.faults.fail_at(point, k, f);
    }
    let mut scanner = env.scanner().await;
    let mut script = Script::new();
    let mut states = vec![];
    let mut restarts = 0;
    for (i, op) in seq.iter().enumerate() {
        if !matches!(op, Op::Scan) {
            script.apply(&env.chain, op);
            continue;
        }
        loop {
            match scanner.run_until_idle().await {
                Ok(()) => break,
                Err(ScanError::Crashed(_)) if restarts == 0 => {
                    restarts += 1;
                    // 재시작: DB를 뺀 모든 것을 새로 만든다. 메모리에서 이어받는 것은 없다
                    drop(scanner);
                    scanner = env.scanner().await;
                }
                Err(e) => return Err(format!("{i}번째 걸음(스캔) 오류: {e}")),
            }
        }
        states.push(state(env).await);
    }
    Ok(Run {
        states,
        hits: env.faults.hits(),
        restarts,
        fired: env.faults.fired(),
    })
}

/// 스캔마다의 상태(마지막 = 최종 원장·커서·`deposits`·헤더)가 기준과 같다.
fn assert_same(clean: &Run, got: &Run, what: &str) {
    assert_eq!(got.states.len(), clean.states.len(), "{what}: 스캔 수");
    for (i, (c, g)) in clean.states.iter().zip(&got.states).enumerate() {
        assert_eq!(
            g, c,
            "{what}: {i}번째 스캔 뒤 상태가 장애 없는 실행과 다르다"
        );
    }
}
