//! DB 스모크 테스트. `--features db`로만 돈다(verify.sh full이 compose의 Postgres를 띄우고 켠다).
//! 테스트마다 새 데이터베이스를 만들고 끝나면 지운다(docs/ARCHITECTURE.md의 테스트 DB 결정).
#![cfg(feature = "db")]

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// 이름은 이 파일이 만든 영숫자·밑줄뿐이라 SQL에 그대로 넣어도 된다
use sqlx::{AssertSqlSafe, Connection, Executor, PgConnection};

/// `DATABASE_URL`(관리용 데이터베이스) 옆에 이름이 겹치지 않는 데이터베이스를 하나 만든다.
struct TestDb {
    admin_url: String,
    name: String,
    url: String,
}

impl TestDb {
    async fn create() -> Self {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let admin_url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL이 없다. scripts/harness/verify.sh full로 돌리거나 .env.example 값을 내보낸다");
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!(
            "vault_test_{}_{}_{}",
            std::process::id(),
            nanos,
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let mut admin = PgConnection::connect(&admin_url)
            .await
            .expect("관리용 DB 연결");
        admin
            .execute(AssertSqlSafe(format!(r#"CREATE DATABASE "{name}""#)))
            .await
            .expect("테스트 DB 생성");
        admin.close().await.ok();
        let (base, _) = admin_url
            .rsplit_once('/')
            .expect("DATABASE_URL에 데이터베이스 이름이 없다");
        let url = format!("{base}/{name}");
        Self {
            admin_url,
            name,
            url,
        }
    }

    async fn drop_db(self) {
        let mut admin = PgConnection::connect(&self.admin_url)
            .await
            .expect("관리용 DB 연결");
        admin
            .execute(AssertSqlSafe(format!(
                r#"DROP DATABASE "{}" WITH (FORCE)"#,
                self.name
            )))
            .await
            .expect("테스트 DB 삭제");
    }
}

#[tokio::test]
async fn connect_and_migrate_fresh_database() {
    let db = TestDb::create().await;

    let pool = vault::store::connect(&db.url).await.expect("연결");
    vault::store::migrate(&pool).await.expect("마이그레이션");
    // 두 번 돌려도 이미 적용한 것은 건너뛴다(재시작 때 같은 경로를 탄다)
    vault::store::migrate(&pool)
        .await
        .expect("마이그레이션 재실행");

    let versions: Vec<i32> =
        sqlx::query_scalar("SELECT version FROM schema_version ORDER BY version")
            .fetch_all(&pool)
            .await
            .expect("schema_version 조회");
    assert_eq!(versions, vec![1]);

    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success")
        .fetch_one(&pool)
        .await
        .expect("_sqlx_migrations 조회");
    assert_eq!(applied, vault::store::MIGRATOR.iter().count() as i64);

    pool.close().await;
    db.drop_db().await;
}
