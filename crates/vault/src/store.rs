//! 연결 풀, 마이그레이션, 장애 주입 자리(`Faults`). 재시작 검사용 주입 지점은 04에서 넓힌다.

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

/// 주입된 장애. 어느 지점에서 났는지만 담는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Injected(pub &'static str);

impl std::fmt::Display for Injected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "injected fault at {}", self.0)
    }
}

impl std::error::Error for Injected {}

/// 장애 주입 자리. 운영 코드는 정해진 지점에서 이것을 부르고, 오류를 받으면 실제 장애와 똑같이 다룬다.
/// 운영 기본값은 `NoFaults`(아무것도 하지 않음)이고, 테스트는 `vault-sim`의 구현을 넣는다.
/// `cfg(test)` 분기를 두지 않으므로 테스트한 코드와 운영 코드가 같다.
pub trait Faults {
    /// DB 커밋이 끝난 직후. 오류면 호출자는 "커밋 결과를 모른다"로 처리한다
    /// (커밋 응답이 유실된 것과 같다. 실제로는 커밋되어 있다).
    fn after_commit(&self, point: &'static str) -> Result<(), Injected>;
}

/// 운영 기본값: 장애를 넣지 않는다.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoFaults;

impl Faults for NoFaults {
    fn after_commit(&self, _point: &'static str) -> Result<(), Injected> {
        Ok(())
    }
}
