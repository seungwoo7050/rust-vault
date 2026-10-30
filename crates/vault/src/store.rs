//! 연결 풀과 마이그레이션. 트랜잭션 경계와 장애 주입 래퍼는 02·04에서 더한다.

use sqlx::PgPool;
use sqlx::migrate::{MigrateError, Migrator};

/// `crates/vault/migrations/`를 컴파일 시점에 담는다. 이미 커밋된 파일은 고치지 않고 새 파일만 더한다.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Postgres 연결 풀을 연다.
pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    PgPool::connect(database_url).await
}

/// 아직 적용하지 않은 마이그레이션을 순서대로 적용한다.
pub async fn migrate(pool: &PgPool) -> Result<(), MigrateError> {
    MIGRATOR.run(pool).await
}
