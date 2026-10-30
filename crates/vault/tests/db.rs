//! DB 스모크 테스트. `--features db`로만 돈다(verify.sh full이 compose의 Postgres를 띄우고 켠다).
//! 테스트 DB는 `vault_sim::db::TestDb`가 만들고 지운다.
#![cfg(feature = "db")]

use vault_sim::db::TestDb;

#[tokio::test]
async fn connect_and_migrate_fresh_database() {
    let db = TestDb::create().await;
    let pool = &db.pool;
    // TestDb가 이미 한 번 적용했다. 다시 돌려도 이미 적용한 것은 건너뛴다(재시작 때 같은 경로를 탄다)
    vault::store::migrate(pool)
        .await
        .expect("마이그레이션 재실행");

    let versions: Vec<i32> =
        sqlx::query_scalar("SELECT version FROM schema_version ORDER BY version")
            .fetch_all(pool)
            .await
            .expect("schema_version 조회");
    assert_eq!(versions, vec![1, 2, 3]);

    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success")
        .fetch_one(pool)
        .await
        .expect("_sqlx_migrations 조회");
    assert_eq!(applied, vault::store::MIGRATOR.iter().count() as i64);

    db.drop_db().await;
}
