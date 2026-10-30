//! I2 멱등성: 같은 입금 이벤트나 출금 요청을 여러 번 처리해도 자산이 중복 반영되지 않는다

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};
use vault::ledger::{self, AccountKey, EntryId, NewEntry, PostError, Posted};
use vault_sim::db::TestDb;

use crate::model::{self, Expect, Model, Shape};

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
#[test]
#[ignore = "03-deposit-reorg"]
fn duplicate_block_events_credit_once() {
    unimplemented!(
        "T2.3: (가) 같은 블록을 2~3번 전달 (나) 블록 커밋 직후 \"결과 모름\" 오류를 주입해 같은 프로세스가 그 블록을 다시 처리 (다) 재시작 후 커서 직전 블록 재전달. 단언: 입금마다 `pending_deposit:u`·`user:u`에 한 번씩만 반영"
    );
}

// T2.4 실패해야 할 변형: `withdrawals.request_id` 유일 제약과 기존 요청 조회를 제거(홀드 키는 그대로 두어도 두 번째 출금 행이 따로 서명·지급된다)
#[test]
#[ignore = "05-withdrawal"]
fn retried_withdrawal_request_holds_once() {
    unimplemented!(
        "T2.4: 같은 출금 요청 ID를 여러 번 제출(재시작 사이에도). 단언: 출금 행 1개, 홀드 분개 1개, 체인 지급 1회"
    );
}
