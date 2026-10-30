//! I3 원장 정합성: 모든 회계 거래는 차변과 대변이 일치한다

use proptest::collection::vec;
use proptest::test_runner::{TestCaseError, TestRunner};
use vault::ledger::{self, AccountKey, NewEntry, Posted};
use vault_sim::db::TestDb;

use crate::history::{self, Op, Script};
use crate::model::{self, Expect, Model, Shape};
use crate::scan::{self, Env};

// T3.1 실패해야 할 변형: 분개 줄 기록 루프가 마지막 줄을 빠뜨림(커밋 트리거가 거부해 "모두 성공" 단언이 깨진다)
#[test]
fn every_posted_entry_sums_to_zero() {
    let rt = model::runtime();
    let mut runner = TestRunner::new(model::config(32));
    // 임의 모양(줄 2·3·4개) 중 모델이 받아들이는 것만 남긴 열. 맨 앞의 자금 입금으로 잔액을 채운다
    runner
        .run(&vec(model::shape(), 1..30), |shapes| {
            rt.block_on(run_valid(shapes)).map_err(TestCaseError::fail)
        })
        .unwrap();
}

async fn run_valid(shapes: Vec<Shape>) -> Result<(), String> {
    let db = TestDb::create().await;
    let result = async {
        let mut conn = db.pool.acquire().await.map_err(|e| e.to_string())?;
        model::open_all(&mut conn).await;

        let mut m = Model::new();
        let mut seq = vec![model::funding("t3.1:fund", 5_000)];
        for (i, s) in shapes.iter().enumerate() {
            seq.push(model::entry(format!("t3.1:{i}"), "test", s.lines()));
        }
        // 정상 분개만 남긴다(모델이 거부하는 것은 뺀다)
        let seq: Vec<NewEntry> = seq
            .into_iter()
            .filter(|e| matches!(m.apply(e), Expect::New(_)))
            .collect();

        let mut ids = Vec::new();
        for e in seq {
            let key = e.idempotency_key.clone();
            let mut tx = db.pool.begin().await.map_err(|e| e.to_string())?;
            match ledger::post(&mut tx, e).await {
                Ok(Posted::New(id)) => ids.push(id),
                other => return Err(format!("{key}: 정상 분개인데 {other:?}")),
            }
            tx.commit()
                .await
                .map_err(|err| format!("{key}: 커밋 실패: {err}"))?;
        }

        // 분개별 줄 합 0, 줄 2개 이상
        let per_entry: Vec<(i64, i64, String)> = sqlx::query_as(
            "SELECT e.id, count(l.id), coalesce(sum(l.amount), 0)::text
               FROM journal_entries e LEFT JOIN journal_lines l ON l.entry_id = e.id
              GROUP BY e.id ORDER BY e.id",
        )
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| e.to_string())?;
        if per_entry.len() != ids.len() {
            return Err(format!(
                "분개 수 DB {}, 기록 {}",
                per_entry.len(),
                ids.len()
            ));
        }
        for (id, n, sum) in &per_entry {
            if *n < 2 || sum != "0" {
                return Err(format!("분개 {id}: 줄 {n}개, 합 {sum}"));
            }
        }
        let tb = ledger::trial_balance(&mut conn)
            .await
            .map_err(|e| e.to_string())?;
        if tb != 0 {
            return Err(format!("시산 {tb}"));
        }
        model::check_balances(&mut conn, &m).await
    }
    .await;
    db.drop_db().await;
    result
}

// T3.2 실패해야 할 변형: 커밋 시점 제약 트리거 제거
#[tokio::test]
async fn db_rejects_unbalanced_entry() {
    let db = TestDb::create().await;
    let mut conn = db.pool.acquire().await.unwrap();
    model::open_all(&mut conn).await;

    // Rust 검증을 거치지 않고 SQL로 넣는다: (가) 합이 0이 아닌 두 줄 (나) 줄 하나 (다) 줄 없음
    let cases: [(&str, &[(&str, &str)]); 3] = [
        (
            "sql:unbalanced",
            &[("hot_wallet:0xa", "100"), ("operator_capital", "-50")],
        ),
        ("sql:one-line", &[("hot_wallet:0xa", "100")]),
        ("sql:no-lines", &[]),
    ];
    for (key, lines) in cases {
        let mut tx = db.pool.begin().await.unwrap();
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO journal_entries (idempotency_key, kind, body_hash)
             VALUES ($1, 'test', '\\x00') RETURNING id",
        )
        .bind(key)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        for (account, amount) in lines {
            sqlx::query(
                "INSERT INTO journal_lines (entry_id, account_key, amount) VALUES ($1, $2, $3::numeric)",
            )
            .bind(id)
            .bind(*account)
            .bind(*amount)
            .execute(&mut *tx)
            .await
            .unwrap();
        }
        let err = tx
            .commit()
            .await
            .expect_err(&format!("{key}: 차대 불일치 분개의 커밋이 성공했다"));
        let constraint = err.as_database_error().and_then(|e| e.constraint());
        assert_eq!(
            constraint,
            Some("journal_entry_balanced"),
            "{key}: 제약 위반이어야 한다, 받은 오류 {err}"
        );
    }

    let entries: i64 = sqlx::query_scalar("SELECT count(*) FROM journal_entries")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    let lines: i64 = sqlx::query_scalar("SELECT count(*) FROM journal_lines")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!((entries, lines), (0, 0), "거부된 분개나 줄이 남았다");
    assert_eq!(ledger::trial_balance(&mut conn).await.unwrap(), 0);

    drop(conn);
    db.drop_db().await;
}

// T3.3 실패해야 할 변형: 역분개가 잠정 입금 계정 줄만 쓰고 핫월렛 줄을 빠뜨림(거부되어 스캔이 멈춘다)
#[test]
fn trial_balance_zero_after_reorgs() {
    let rt = model::runtime();
    let mut runner = TestRunner::new(model::config(16));
    runner
        .run(&vec(history::op(), 0..12), |ops| {
            // 맨 앞은 언제나 [입금 블록, 스캔, 그 블록을 빼는 깊이 1 reorg, 스캔]. 역분개가 한 번은 일어난다
            let mut seq = vec![
                Op::Mine(vec![(0, 10)]),
                Op::Scan,
                Op::Reorg {
                    depth: 1,
                    extra: 1,
                    deposits: vec![],
                    reinclude: 0,
                },
                Op::Scan,
            ];
            seq.extend(ops);
            rt.block_on(run_history(seq)).map_err(TestCaseError::fail)
        })
        .unwrap();
}

async fn run_history(seq: Vec<Op>) -> Result<(), String> {
    let env = Env::new().await;
    let result = history(&env, seq).await;
    env.db.drop_db().await;
    result
}

async fn history(env: &Env, seq: Vec<Op>) -> Result<(), String> {
    let chain = &env.chain;
    let mut scanner = env.scanner().await;
    let mut script = Script::new();
    for (i, op) in seq.iter().enumerate() {
        match op {
            Op::Scan => scanner
                .run_until_idle()
                .await
                .map_err(|e| format!("{i}번째 스캔 오류: {e}"))?,
            op => script.apply(chain, op),
        }
    }
    // 끝에 빈 블록 N개를 더하고 head까지 스캔한다
    scan::extend(chain, chain.tip().hash, scan::N as usize);
    scanner
        .run_until_idle()
        .await
        .map_err(|e| format!("마지막 스캔 오류: {e}"))?;

    if env.cursor().await.hash != chain.tip().hash {
        return Err("커서가 head에 이르지 못했다".into());
    }
    let trial = env.trial_balance().await;
    if trial != 0 {
        return Err(format!("시산 {trial}"));
    }
    // 고아 블록의 잠정 입금(잠정 입금 분개 + 역분개의 pending_deposit 줄 합)은 0이어야 한다
    let orphan_pending: Vec<(String, String)> = sqlx::query_as(
        "SELECT b.hash, coalesce(sum(l.amount), 0)::text
           FROM block_headers b
           JOIN deposits d ON d.block_hash = b.hash
           JOIN journal_entries e ON e.idempotency_key IN (
                'dep:pending:' || $1 || ':' || d.tx_hash || ':' || b.hash,
                'dep:revert:' || $1 || ':' || d.tx_hash || ':' || b.hash)
           JOIN journal_lines l ON l.entry_id = e.id AND l.account_key LIKE 'pending_deposit:%'
          WHERE b.orphaned
          GROUP BY b.hash",
    )
    .bind(scan::CHAIN_ID.to_string())
    .fetch_all(&mut *env.conn().await)
    .await
    .map_err(|e| e.to_string())?;
    if let Some((h, sum)) = orphan_pending.iter().find(|(_, s)| s != "0") {
        return Err(format!("고아 블록 {h}의 잠정 입금 잔액 {sum}"));
    }
    let canonical = chain.canonical_blocks();
    for u in scan::USERS {
        let want = scan::deposits_to(&canonical, scan::addr(u));
        let hot = env.balance(&AccountKey::hot_wallet(scan::addr(u))).await;
        if hot != want {
            return Err(format!(
                "hot_wallet:{} 잔액 {hot}, canonical 입금 합 {want}",
                scan::addr(u)
            ));
        }
    }
    Ok(())
}
