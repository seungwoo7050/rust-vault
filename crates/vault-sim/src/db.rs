//! 테스트용 데이터베이스. 테스트마다 새로 만들고 끝나면 지운다(docs/ARCHITECTURE.md의 테스트 DB 결정).

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::{AssertSqlSafe, Connection, Executor, PgConnection, PgPool};

/// `DATABASE_URL`(관리용 데이터베이스) 옆에 이름이 겹치지 않는 데이터베이스를 하나 만들고
/// 마이그레이션까지 적용한 풀을 연다.
pub struct TestDb {
    admin_url: String,
    name: String,
    /// 이 데이터베이스의 연결 문자열
    pub url: String,
    /// 마이그레이션을 적용한 풀
    pub pool: PgPool,
}

impl TestDb {
    pub async fn create() -> Self {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let admin_url = std::env::var("DATABASE_URL").expect(
            "DATABASE_URL이 없다. scripts/harness/verify.sh full로 돌리거나 .env.example 값을 내보낸다",
        );
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // 이름은 영숫자·밑줄뿐이라 SQL에 그대로 넣어도 된다
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
        let pool = vault::store::connect(&url).await.expect("테스트 DB 연결");
        vault::store::migrate(&pool).await.expect("마이그레이션");
        Self {
            admin_url,
            name,
            url,
            pool,
        }
    }

    /// 풀을 닫고 데이터베이스를 지운다.
    pub async fn drop_db(self) {
        self.pool.close().await;
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
