//! I2 멱등성: 같은 입금 이벤트나 출금 요청을 여러 번 처리해도 자산이 중복 반영되지 않는다

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};
use vault::chain::Notification;
use vault::deposit::ScanError;
use vault::ledger::{self, AccountKey, EntryId, NewEntry, PostError, Posted};
use vault_sim::chain::transfer;
use vault_sim::db::TestDb;

use vault::withdrawal::{Status, Step};

use crate::model::{self, Expect, Model, Shape};
use crate::scan::{self, Env};
use crate::withdraw::{self, WEnv};

// T2.1 실패해야 할 변형: 멱등 키 유일 제약과 기존 키 조회 제거
#[test]
fn same_key_reposted_changes_nothing() {
    let rt = model::runtime();
    let mut runner = TestRunner::new(model::config(32));
    // [자금 입금] + 앞 열 + [재기록] + 뒤 열. 재기록은 그때까지 받아들여진 분개 중 하나를 같은 키·같은 내용으로 다시 보낸다
    let ops = (
        vec(op(), 0..20),
        any::<prop::sample::Index>(),
        vec(op(), 0..20),
    );
    runner
        .run(&ops, |(head, again, tail)| {
            let mut seq = vec![Op::Fresh(Shape::Pair {
                dr: 4,
                cr: 7,
                amount: 500,
            })];
            seq.extend(head);
            seq.push(Op::Again(again));
            seq.extend(tail);
            rt.block_on(run_with_reposts(seq))
                .map_err(TestCaseError::fail)
        })
        .unwrap();
}

#[derive(Debug, Clone)]
enum Op {
    /// 새 키의 분개(잔액이 모자라면 거부된다)
    Fresh(Shape),
    /// 앞서 받아들여진 분개 하나를 같은 키·같은 내용으로 다시 기록
    Again(prop::sample::Index),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => model::shape().prop_map(Op::Fresh),
        1 => any::<prop::sample::Index>().prop_map(Op::Again),
    ]
}

async fn run_with_reposts(seq: Vec<Op>) -> Result<(), String> {
    let db = TestDb::create().await;
    let result = async {
        let mut conn = db.pool.acquire().await.map_err(|e| e.to_string())?;
        model::open_all(&mut conn).await;

        // 중복 없는 열(재기록을 뺀 열)의 모델
        let mut dedup = Model::new();
        // 받아들여진 분개와 그 ID
        let mut accepted: Vec<(NewEntry, EntryId)> = Vec::new();
        let mut reposts = 0;

        for (i, op) in seq.iter().enumerate() {
            // 분개마다 트랜잭션 하나. 재기록이 다른 트랜잭션에서 와도 한 번만 반영되어야 한다
            let mut tx = db.pool.begin().await.map_err(|e| e.to_string())?;
            match op {
                Op::Fresh(s) => {
                    let e = model::entry(format!("t2.1:{i}"), "test", s.lines());
                    let want = dedup.apply(&e);
                    let got = ledger::post(&mut tx, e.clone()).await;
                    match (&got, &want) {
                        (Ok(Posted::New(id)), Expect::New(_)) => accepted.push((e, *id)),
                        (Err(PostError::InsufficientFunds(a)), Expect::InsufficientFunds(b))
                            if a == b => {}
                        _ => return Err(format!("{i}번째 {op:?}: post {got:?}, 모델 {want:?}")),
                    }
                }
                Op::Again(pick) => {
                    let (e, id) = pick.get(&accepted).clone();
                    let got = ledger::post(&mut tx, e.clone()).await;
                    if !matches!(got, Ok(Posted::Existing(x)) if x == id) {
                        return Err(format!(
                            "{i}번째 재기록 {}: post {got:?}, 기존 분개 {id:?}여야 한다",
                            e.idempotency_key
                        ));
                    }
                    reposts += 1;
                }
            }
            tx.commit()
                .await
                .map_err(|e| format!("{i}번째 커밋 실패: {e}"))?;
        }
        assert!(reposts >= 1);

        model::check_balances(&mut conn, &dedup).await?;
        let entries: i64 = sqlx::query_scalar("SELECT count(*) FROM journal_entries")
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| e.to_string())?;
        if entries != dedup.entries as i64 {
            return Err(format!(
                "분개 수 DB {entries}, 중복 없는 열 {}",
                dedup.entries
            ));
        }
        Ok(())
    }
    .await;
    db.drop_db().await;
    result
}

// T2.2 실패해야 할 변형: 기존 키가 있으면 내용 비교 없이 성공 반환
#[tokio::test]
async fn same_key_different_body_rejected() {
    let db = TestDb::create().await;
    let mut conn = db.pool.acquire().await.unwrap();
    model::open_all(&mut conn).await;

    let key = "cap:fund:1:0xtx";
    let original = model::funding(key, 100);
    let mut tx = db.pool.begin().await.unwrap();
    let first = ledger::post(&mut tx, original.clone()).await.unwrap();
    tx.commit().await.unwrap();
    let Posted::New(id) = first else {
        panic!("처음 기록은 New여야 한다: {first:?}")
    };

    let before: Vec<i128> = balances(&mut conn).await;
    // 같은 키, 다른 금액 / 다른 계정 / 다른 금액과 계정
    let other_amount = model::funding(key, 101);
    let mut other_account = model::funding(key, 100);
    other_account.lines[0].account = AccountKey::hot_wallet("0xb");
    let mut both = model::funding(key, 7);
    both.lines[1].account = AccountKey::pending_deposit(1);

    let mut tx = db.pool.begin().await.unwrap();
    for e in [other_amount, other_account, both] {
        let got = ledger::post(&mut tx, e.clone()).await;
        assert!(
            matches!(&got, Err(PostError::IdempotencyConflict(k)) if k == key),
            "{e:?}: IdempotencyConflict여야 한다, 받은 것 {got:?}"
        );
    }
    // 거부 뒤에도 트랜잭션은 커밋된다
    tx.commit().await.unwrap();

    assert_eq!(balances(&mut conn).await, before, "잔액이 바뀌었다");
    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, idempotency_key FROM journal_entries")
            .fetch_all(&mut *conn)
            .await
            .unwrap();
    assert_eq!(rows, vec![(id.0, key.to_owned())], "기존 분개가 바뀌었다");
    let lines: Vec<(String, String)> = sqlx::query_as(
        "SELECT account_key, amount::text FROM journal_lines WHERE entry_id = $1 ORDER BY account_key",
    )
    .bind(id.0)
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    assert_eq!(
        lines,
        vec![
            ("hot_wallet:0xa".to_owned(), "100".to_owned()),
            ("operator_capital".to_owned(), "-100".to_owned()),
        ]
    );
    // 같은 내용이면 여전히 기존 ID
    let mut tx = db.pool.begin().await.unwrap();
    let again = ledger::post(&mut tx, original).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(again, Posted::Existing(id));

    drop(conn);
    db.drop_db().await;
}

async fn balances(conn: &mut sqlx::PgConnection) -> Vec<i128> {
    let mut out = Vec::new();
    for k in model::opened() {
        out.push(ledger::balance(conn, &k).await.unwrap());
    }
    out
}

// T2.3 실패해야 할 변형: 입금 키를 tx 해시 대신 수신 순번으로 만듦((나)의 재처리가 원장까지 닿아 두 번 기록된다)
#[tokio::test]
async fn duplicate_block_events_credit_once() {
    let env = Env::new().await;
    let pending = |u: i64| AccountKey::pending_deposit(u);
    let user = |u: i64| AccountKey::user(u);
    let mut scanner = env.scanner().await;

    // 블록 1: 사용자 1에게 100. 블록 2: 사용자 1에게 50, 사용자 2에게 70. 그 위로 빈 블록 N개(모두 확정)
    let g = env.chain.genesis().hash;
    let b1 = env.chain.mine(g, vec![transfer(1, scan::addr(1), 100)]);
    let b2 = env.chain.mine(
        b1,
        vec![
            transfer(2, scan::addr(1), 50),
            transfer(3, scan::addr(2), 70),
        ],
    );
    let mut tip = b2;
    let mut blocks = vec![b1, b2];
    for _ in 0..scan::N {
        tip = env.chain.mine(tip, vec![]);
        blocks.push(tip);
    }
    // 블록마다 기대하는 (pending:1, pending:2, user:1, user:2). 블록 h에서 h − N + 1 이하 블록의 입금이 확정된다
    let expect = [
        (100, 0, 0, 0),
        (150, 70, 0, 0),
        (50, 70, 100, 0),
        (0, 0, 150, 70),
        (0, 0, 150, 70),
    ];

    for (i, hash) in blocks.iter().enumerate() {
        env.chain.set_canonical(*hash);
        let notes = env.chain.notifications();
        // (나) 이 블록의 커밋 직후 "결과 모름"을 한 번 주입한다. 커밋은 실제로 됐다
        env.faults.fail_after_block_commit(1);
        let first = scanner.on_notification(notes[0].clone()).await;
        assert!(
            matches!(first, Err(ScanError::CommitUnknown(_))),
            "블록 {}: 주입한 커밋 결과 모름이 나와야 한다, 받은 것 {first:?}",
            i + 1
        );
        // (가) 같은 블록 알림을 2~3번 더 전달한다. 첫 전달이 커밋 결과를 모르는 블록을 다시 처리한다
        for _ in 0..(2 + i % 2) {
            scanner.on_notification(notes[0].clone()).await.unwrap();
        }
        assert_eq!(env.cursor().await.hash, *hash, "블록 {}: 커서", i + 1);
        let (p1, p2, u1, u2) = expect[i];
        let got = (
            env.balance(&pending(1)).await,
            env.balance(&pending(2)).await,
            env.balance(&user(1)).await,
            env.balance(&user(2)).await,
        );
        assert_eq!(
            got,
            (p1, p2, u1, u2),
            "블록 {}: (pending:1, pending:2, user:1, user:2)",
            i + 1
        );

        // (다) 재시작 후 커서 직전 블록의 알림을 다시 전달한다
        if i >= 1 {
            scanner = env.scanner().await;
            let prev = env.chain.header(blocks[i - 1]);
            scanner
                .on_notification(Notification::NewHead(prev))
                .await
                .unwrap();
            let again = (
                env.balance(&pending(1)).await,
                env.balance(&pending(2)).await,
                env.balance(&user(1)).await,
                env.balance(&user(2)).await,
            );
            assert_eq!(
                again,
                got,
                "블록 {}: 재시작 뒤 재전달이 원장을 바꿨다",
                i + 1
            );
        }
    }

    assert_eq!(env.entries("dep:pending:").await, 3, "잠정 입금 분개 수");
    assert_eq!(env.entries("dep:confirm:").await, 3, "확정 분개 수");
    assert_eq!(env.entries("dep:revert:").await, 0, "역분개 수");
    env.db.drop_db().await;
}

// T2.4 실패해야 할 변형: `withdrawals.request_id` 유일 제약과 기존 요청 조회를 제거(홀드 키는 그대로 두어도 두 번째 출금 행이 따로 서명·지급된다)
#[tokio::test]
async fn retried_withdrawal_request_holds_once() {
    let w = WEnv::new().await;
    w.deposit(1, 100).await;
    let to = withdraw::recipient(1);
    let id = "t2.4";

    // 처음 제출과 곧바로 다시 제출
    assert_eq!(w.request(id, 1, to, 30).await.status, Status::Held);
    assert_eq!(w.request(id, 1, to, 30).await.status, Status::Held);
    assert_eq!(w.rows(id).await, 1, "다시 제출한 뒤 출금 행 수");
    assert_eq!(
        w.entries("wd:hold:").await,
        1,
        "다시 제출한 뒤 홀드 분개 수"
    );

    // 서명까지 하고 재시작한 뒤 다시 제출
    let mut p = w.processor().await;
    let first = p.step().await.unwrap();
    assert!(
        matches!(
            &first,
            Step::Moved {
                to: Status::Signed,
                ..
            }
        ),
        "첫 step은 서명: {first:?}"
    );
    drop(p);
    assert_eq!(w.request(id, 1, to, 30).await.status, Status::Signed);

    // 재시작한 처리기로 끝까지(전송·확정) 돌리고, 그 사이와 뒤에도 다시 제출
    let mut p = w.processor().await;
    p.run_until_idle().await.unwrap();
    w.request(id, 1, to, 30).await;
    w.settle(&mut p, scan::N as usize + 1).await;
    drop(p);
    let mut p = w.processor().await;
    assert_eq!(w.request(id, 1, to, 30).await.status, Status::Confirmed);
    w.settle(&mut p, scan::N as usize + 1).await;

    assert_eq!(w.rows(id).await, 1, "출금 행 수");
    assert_eq!(w.entries("wd:hold:").await, 1, "홀드 분개 수");
    assert_eq!(w.payments_to(to).len(), 1, "체인 지급 수");
    assert_eq!(w.status(id).await, Status::Confirmed);
    assert_eq!(w.balance(&AccountKey::user(1)).await, 70);

    w.env.db.drop_db().await;
}
