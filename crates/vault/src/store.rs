//! 연결 풀, 마이그레이션, 커밋 도우미, 장애 주입 자리(`Faults`), 일시 오류 분류.

use sqlx::migrate::{MigrateError, Migrator};
use sqlx::{PgPool, Postgres, Transaction};

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

/// 커밋하는 작업 단위. 커밋 직전·직후 주입 지점이 어느 커밋인지 가른다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Site {
    /// 블록 하나 처리(헤더·잠정 입금·확정·커서 전진), 또는 블록 처리 중의 정지 기록
    Block,
    /// 되감기(고아 표시·역분개·커서 이동), 또는 되감기 중의 사고 기록·정지
    Rewind,
}

/// 장애 주입 지점. 이름은 docs/FAILURE_SCENARIOS.md의 "시나리오를 테스트로 만들 때"와 같다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FaultPoint {
    /// 블록을 체인에서 받은 직후, 아무것도 쓰기 전(이벤트 수신 직후)
    BlockFetched,
    /// 블록의 분개(잠정 입금·확정)는 트랜잭션에 썼고 커서 전진 전(분개 기록 중간)
    BlockEntriesWritten,
    /// 커밋 직전. 트랜잭션은 아직 열려 있다
    BeforeCommit(Site),
    /// 커밋 직후. 커밋은 됐다
    AfterCommit(Site),
}

impl std::fmt::Display for FaultPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FaultPoint::BlockFetched => f.write_str("block fetched"),
            FaultPoint::BlockEntriesWritten => f.write_str("block entries written"),
            FaultPoint::BeforeCommit(s) => write!(f, "before {s:?} commit"),
            FaultPoint::AfterCommit(s) => write!(f, "after {s:?} commit"),
        }
    }
}

/// 주입된 장애의 종류.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Injected {
    /// 프로세스가 그 자리에서 죽는다. 열린 트랜잭션은 버려지고, 호출자는 `Crashed`로 빠져나온다.
    /// 테스트는 DB를 뺀 객체를 모두 버리고 같은 DB로 새로 만든다(재시작)
    Crash,
    /// 일시 DB 오류(연결 끊김, 직렬화 실패 등). 열린 트랜잭션은 버려진다.
    /// `AfterCommit`에서 나면 커밋은 된 상태다
    Transient,
    /// 커밋 응답이 유실됐다(실제로는 커밋됐다). `AfterCommit`에서만 뜻이 있고, 그 앞 지점에서는 `Transient`와 같다
    CommitOutcomeUnknown,
}

/// 장애 주입 자리. 운영 코드는 정해진 지점마다 `hit`을 부르고, 오류를 받으면 실제 장애와 똑같이 다룬다.
/// 운영 기본값은 `NoFaults`(언제나 `Ok`)이고, 테스트는 `vault-sim`의 구현을 넣는다.
/// `cfg(test)` 분기를 두지 않으므로 테스트한 코드와 운영 코드가 같다.
pub trait Faults {
    fn hit(&self, point: FaultPoint) -> Result<(), Injected>;
}

/// 운영 기본값: 장애를 넣지 않는다.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoFaults;

impl Faults for NoFaults {
    fn hit(&self, _point: FaultPoint) -> Result<(), Injected> {
        Ok(())
    }
}

/// 커밋하지 못했거나 커밋을 확인하지 못했다.
#[derive(Debug)]
pub enum CommitError {
    /// 커밋되지 않았다(커밋 전 장애이거나, DB가 커밋을 거부했다). 트랜잭션은 되돌려졌다
    NotCommitted(Failure),
    /// 커밋 결과를 모르거나(응답 유실), 커밋은 됐지만 호출자가 오류를 받았다
    Unknown(Failure),
}

/// 커밋 실패의 원인.
#[derive(Debug)]
pub enum Failure {
    /// 주입된 장애
    Injected(FaultPoint, Injected),
    /// 실제 DB 오류
    Db(sqlx::Error),
}

/// 모든 커밋이 거치는 도우미. 커밋 직전·직후에 장애 주입 지점을 부른다.
/// 작업을 여러 트랜잭션으로 나누면 나뉜 커밋마다 이 지점이 걸린다.
pub async fn commit(
    tx: Transaction<'_, Postgres>,
    faults: &impl Faults,
    site: Site,
) -> Result<(), CommitError> {
    let before = FaultPoint::BeforeCommit(site);
    if let Err(i) = faults.hit(before) {
        // 트랜잭션을 버리면 되돌려진다
        drop(tx);
        return Err(CommitError::NotCommitted(Failure::Injected(before, i)));
    }
    match tx.commit().await {
        Ok(()) => {}
        // DB가 응답한 오류(커밋 시점 제약 위반 등)는 커밋이 거부된 것이다
        Err(e @ sqlx::Error::Database(_)) => return Err(CommitError::NotCommitted(Failure::Db(e))),
        // 그 밖(연결 끊김 등)은 응답 유실과 구분할 수 없다
        Err(e) => return Err(CommitError::Unknown(Failure::Db(e))),
    }
    let after = FaultPoint::AfterCommit(site);
    faults
        .hit(after)
        .map_err(|i| CommitError::Unknown(Failure::Injected(after, i)))
}

/// 일시 오류인가: 연결 오류(I/O, SQLSTATE `08xxx`), 직렬화 실패(`40001`), 교착(`40P01`), 풀 타임아웃.
/// 일시 오류는 작업 단위 전체를 다시 하면 된다. 그 밖의 오류(제약 위반 등)는 다시 해도 같으므로 재시도하지 않는다.
pub fn is_transient(e: &sqlx::Error) -> bool {
    match e {
        sqlx::Error::Io(_) | sqlx::Error::PoolTimedOut => true,
        sqlx::Error::Database(db) => db
            .code()
            .is_some_and(|c| c == "40001" || c == "40P01" || c.starts_with("08")),
        _ => false,
    }
}
