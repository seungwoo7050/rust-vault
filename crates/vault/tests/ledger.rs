//! 불변조건 표 밖의 원장 테스트(`--features db`). 추가 전용, 동시 멱등 기록, 모든 계정의 음수 금지.

#[path = "support/model.rs"]
mod model;

use vault::ledger::{self, AccountKey, Line, PostError, Posted};
use vault_sim::db::TestDb;

/// (가) 분개·분개 줄의 UPDATE·DELETE·TRUNCATE는 트리거가 거부하고, 기록은 그대로 남는다.
#[tokio::test]
async fn journal_is_append_only() {
    let db = TestDb::create().await;
    let mut conn = db.pool.acquire().await.unwrap();
    model::open_all(&mut conn).await;
    let mut tx = db.pool.begin().await.unwrap();
    let id = ledger::post(&mut tx, model::funding("cap:fund:1:0x1", 100))
        .await
        .unwrap()
        .id();
    tx.commit().await.unwrap();

    for sql in [
        "UPDATE journal_lines SET amount = amount * 2",
        "DELETE FROM journal_lines",
        "TRUNCATE journal_lines",
        "UPDATE journal_entries SET kind = 'edited'",
        "DELETE FROM journal_entries",
        "TRUNCATE journal_entries CASCADE",
    ] {
        let err = sqlx::query(sql)
            .execute(&mut *conn)
            .await
            .expect_err(&format!("{sql}: 거부되어야 한다"));
        assert!(
            err.to_string().contains("append-only"),
            "{sql}: 추가 전용 트리거의 오류여야 한다, 받은 오류 {err}"
        );
    }

    let kinds: Vec<(i64, String)> = sqlx::query_as("SELECT id, kind FROM journal_entries")
        .fetch_all(&mut *conn)
        .await
        .unwrap();
    assert_eq!(kinds, vec![(id.0, "cap:fund".to_owned())]);
    assert_eq!(
        ledger::balance(&mut conn, &AccountKey::hot_wallet("0xa"))
            .await
            .unwrap(),
        100
    );
    assert_eq!(ledger::trial_balance(&mut conn).await.unwrap(), 0);
    drop(conn);
    db.drop_db().await;
}

/// (나) 두 연결이 같은 멱등 키로 동시에 기록해도 분개는 하나이고 둘 다 같은 ID를 받는다.
#[tokio::test]
async fn concurrent_same_key_posts_once() {
    let db = TestDb::create().await;
    let mut conn = db.pool.acquire().await.unwrap();
    model::open_all(&mut conn).await;

    for round in 0..8 {
        let key = format!("cap:fund:1:0x{round}");
        let e = model::funding(key.clone(), 10);
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let run = |e: ledger::NewEntry| {
            let pool = db.pool.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                let mut tx = pool.begin().await.unwrap();
                barrier.wait().await;
                let posted = ledger::post(&mut tx, e).await.unwrap();
                tx.commit().await.unwrap();
                posted
            })
        };
        let (a, b) = tokio::join!(run(e.clone()), run(e));
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a.id(), b.id(), "{key}: 두 기록의 ID가 다르다 {a:?} {b:?}");
        assert!(
            matches!(
                (a, b),
                (Posted::New(_), Posted::Existing(_)) | (Posted::Existing(_), Posted::New(_))
            ),
            "{key}: 하나는 New, 하나는 Existing이어야 한다 {a:?} {b:?}"
        );
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM journal_entries WHERE idempotency_key = $1")
                .bind(&key)
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert_eq!(n, 1, "{key}");
    }
    assert_eq!(
        ledger::balance(&mut conn, &AccountKey::hot_wallet("0xa"))
            .await
            .unwrap(),
        80
    );
    drop(conn);
    db.drop_db().await;
}

/// (다) 정상 방향 음수 금지는 사용자 계정만이 아니라 잠정 입금(대변 정상)과 핫월렛(차변 정상)에도 걸린다.
#[tokio::test]
async fn every_account_kind_rejects_negative_balance() {
    let db = TestDb::create().await;
    let mut conn = db.pool.acquire().await.unwrap();
    model::open_all(&mut conn).await;
    let pd = AccountKey::pending_deposit(1);
    let hw = AccountKey::hot_wallet("0xb");

    let mut tx = db.pool.begin().await.unwrap();
    // 운영 자금 100을 hot_wallet:0xb에
    ledger::post(
        &mut tx,
        model::entry(
            "cap:fund:1:0xc",
            "cap:fund",
            vec![
                Line::debit(hw.clone(), model::amt(100)),
                Line::credit(AccountKey::operator_capital(), model::amt(100)),
            ],
        ),
    )
    .await
    .unwrap();
    // 잠정 입금 30
    ledger::post(
        &mut tx,
        model::entry(
            "dep:pending:1:0xt:0xb1",
            "dep:pending",
            vec![
                Line::debit(hw.clone(), model::amt(30)),
                Line::credit(pd.clone(), model::amt(30)),
            ],
        ),
    )
    .await
    .unwrap();
    // 잠정 입금보다 큰 역분개: pending_deposit:1이 음수가 된다
    let got = ledger::post(
        &mut tx,
        model::entry(
            "dep:revert:1:0xt:0xb1",
            "dep:revert",
            vec![
                Line::debit(pd.clone(), model::amt(31)),
                Line::credit(hw.clone(), model::amt(31)),
            ],
        ),
    )
    .await;
    assert!(
        matches!(&got, Err(PostError::InsufficientFunds(k)) if *k == pd),
        "{got:?}"
    );
    // 핫월렛에 있는 것(130)보다 큰 가스비: hot_wallet:0xb가 음수가 된다
    let got = ledger::post(
        &mut tx,
        model::entry(
            "wd:fee:1:0xf",
            "wd:fee",
            vec![
                Line::debit(AccountKey::network_fee(), model::amt(131)),
                Line::credit(hw.clone(), model::amt(131)),
            ],
        ),
    )
    .await;
    assert!(
        matches!(&got, Err(PostError::InsufficientFunds(k)) if *k == hw),
        "{got:?}"
    );
    // 거부 뒤에도 같은 트랜잭션으로 정확히 남은 만큼은 쓸 수 있다
    ledger::post(
        &mut tx,
        model::entry(
            "wd:fee:1:0xg",
            "wd:fee",
            vec![
                Line::debit(AccountKey::network_fee(), model::amt(130)),
                Line::credit(hw.clone(), model::amt(130)),
            ],
        ),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(ledger::balance(&mut conn, &pd).await.unwrap(), 30);
    assert_eq!(ledger::balance(&mut conn, &hw).await.unwrap(), 0);
    assert_eq!(
        ledger::balance(&mut conn, &AccountKey::network_fee())
            .await
            .unwrap(),
        130
    );
    assert_eq!(ledger::trial_balance(&mut conn).await.unwrap(), 0);
    drop(conn);
    db.drop_db().await;
}

/// 계정 열기는 멱등이고, 없는 계정과 사용자 잔액 부족은 각각의 오류로 거부된다.
#[tokio::test]
async fn open_account_idempotent_and_rejections_named() {
    let db = TestDb::create().await;
    let mut conn = db.pool.acquire().await.unwrap();
    model::open_all(&mut conn).await;
    model::open_all(&mut conn).await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM accounts")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(n, model::opened().len() as i64);

    let mut tx = db.pool.begin().await.unwrap();
    let got = ledger::post(
        &mut tx,
        model::entry(
            "x:1",
            "test",
            vec![
                Line::debit(AccountKey::hot_wallet("0xa"), model::amt(5)),
                Line::credit(AccountKey::user(99), model::amt(5)),
            ],
        ),
    )
    .await;
    assert!(
        matches!(&got, Err(PostError::UnknownAccount(k)) if *k == AccountKey::user(99)),
        "{got:?}"
    );
    let got = ledger::post(
        &mut tx,
        model::entry(
            "wd:hold:r1",
            "wd:hold",
            vec![
                Line::debit(AccountKey::user(1), model::amt(1)),
                Line::credit(AccountKey::withdrawal_pending(1), model::amt(1)),
            ],
        ),
    )
    .await;
    assert!(
        matches!(&got, Err(PostError::InsufficientFunds(k)) if *k == AccountKey::user(1)),
        "{got:?}"
    );
    tx.commit().await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM journal_entries")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(n, 0);
    drop(conn);
    db.drop_db().await;
}
