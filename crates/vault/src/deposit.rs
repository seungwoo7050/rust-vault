//! 입금 스캐너(ADR-002). 커서 다음 블록을 체인에 묻고(끌어오기), 부모 해시로 reorg를 감지한다.
//!
//! 한 `step`은 둘 중 하나를 DB 트랜잭션 하나로 한다.
//! - 블록 처리: 헤더 저장 → 잠정 입금 → 확정 → 커서 전진 (`apply_block`)
//! - 되감기: 공통 조상 찾기 → 고아 표시·역분개·커서 이동, 또는 확정 입금이 걸리면 사고 기록·정지 (`rewind`)
//!
//! 상태(커서, 헤더, 입금, 정지 여부)는 모두 DB에 있다. `Scanner`를 새로 만들면 DB에서 이어 간다.
//! 메모리에 두는 것은 "커밋 결과를 모르는 블록" 하나뿐이고, 그것도 같은 프로세스 안의 재시도용이다.

use std::fmt;

use sqlx::{PgConnection, PgPool, Postgres, Transaction};

use crate::chain::{
    Address, Block, BlockHeader, BlockNumber, ChainClient, Hash32, Notification, RpcError,
    RpcOutcome,
};
use crate::ledger::{self, AccountKey, Amount, Line, NewEntry, PostError};
use crate::store::{Faults, Injected, NoFaults};

/// 블록 커밋 직후 장애 주입 지점의 이름.
pub const AFTER_BLOCK_COMMIT: &str = "deposit.block.after_commit";

/// 스캐너 설정.
#[derive(Debug, Clone, Copy)]
pub struct ScanConfig {
    pub chain_id: u64,
    /// 확정 수 N. 1 이상이다
    pub confirmations: u64,
    /// 처음 한 번 저장해 커서로 삼는 헤더(가짜 체인의 genesis 등). 이 아래로는 되감지 않는다
    pub start: BlockHeader,
}

/// 마지막으로 처리한 canonical 블록.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub number: BlockNumber,
    pub hash: Hash32,
}

/// `step` 하나의 결과.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// 블록 하나를 처리하고 커서를 그 블록으로 옮겼다
    Processed(BlockHeader),
    /// 공통 조상까지 되감았다
    Rewound { from: BlockNumber, to: BlockNumber },
    /// 할 일이 없다(커서가 head이거나, 그 번호의 블록이 아직 안 보인다)
    Idle,
}

/// 스캔 오류. `Halted` 말고는 다음 `step`에서 다시 시도하면 된다.
#[derive(Debug)]
pub enum ScanError {
    /// 정지 상태다. 사람이 풀기 전에는 아무것도 하지 않는다
    Halted(String),
    /// 체인 조회의 결과를 모른다(타임아웃 등). 아무것도 쓰지 않았다
    Indeterminate(String),
    /// 체인이 오류를 돌려줬다. 아무것도 쓰지 않았다
    Rpc(RpcError),
    /// 블록 커밋의 결과를 모른다. 다음 `step`이 같은 블록을 다시 처리한다
    CommitUnknown(Injected),
    /// 조건부 커서 이동이 0행이었다. 그 트랜잭션은 되돌렸다
    CursorMoved,
    /// 체인이 준 블록이 요청과 맞지 않거나 원장에 담을 수 없는 값이다
    BadBlock(String),
    Ledger(PostError),
    Db(sqlx::Error),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanError::Halted(r) => write!(f, "scanner halted: {r}"),
            ScanError::Indeterminate(m) => write!(f, "chain read indeterminate: {m}"),
            ScanError::Rpc(e) => write!(f, "chain read failed: {e}"),
            ScanError::CommitUnknown(e) => write!(f, "block commit result unknown: {e}"),
            ScanError::CursorMoved => f.write_str("cursor moved under the scanner"),
            ScanError::BadBlock(m) => write!(f, "bad block: {m}"),
            ScanError::Ledger(e) => write!(f, "ledger: {e}"),
            ScanError::Db(e) => write!(f, "database: {e}"),
        }
    }
}

impl std::error::Error for ScanError {}

impl From<sqlx::Error> for ScanError {
    fn from(e: sqlx::Error) -> Self {
        ScanError::Db(e)
    }
}

impl From<PostError> for ScanError {
    fn from(e: PostError) -> Self {
        ScanError::Ledger(e)
    }
}

/// 입금 주소를 등록하고, 그 입금이 쓰는 계정(`user`, `pending_deposit`, `hot_wallet`)을 연다. 멱등이다.
pub async fn register_address(
    conn: &mut PgConnection,
    address: Address,
    user_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO deposit_addresses (address, user_id) VALUES ($1, $2)
         ON CONFLICT (address) DO NOTHING",
    )
    .bind(address.to_string())
    .bind(user_id)
    .execute(&mut *conn)
    .await?;
    for key in [
        AccountKey::user(user_id),
        AccountKey::pending_deposit(user_id),
        AccountKey::hot_wallet(address),
    ] {
        ledger::open_account(conn, &key).await?;
    }
    Ok(())
}

/// 지금 커서. 스캐너를 한 번도 만들지 않았으면 `None`.
pub async fn cursor(conn: &mut PgConnection) -> Result<Option<Cursor>, sqlx::Error> {
    let row: Option<(i64, String)> = sqlx::query_as("SELECT number, hash FROM scan_cursor")
        .fetch_optional(conn)
        .await?;
    row.map(|(n, h)| decode_cursor(n, &h)).transpose()
}

/// 정지 상태면 그 사유.
pub async fn halted(conn: &mut PgConnection) -> Result<Option<String>, sqlx::Error> {
    let (halted, reason): (bool, Option<String>) =
        sqlx::query_as("SELECT halted, reason FROM scanner_state")
            .fetch_one(conn)
            .await?;
    Ok(halted.then(|| reason.unwrap_or_default()))
}

/// 입금 스캐너. `C`는 체인, `F`는 장애 주입 자리(운영 기본값 `NoFaults`).
pub struct Scanner<C, F = NoFaults> {
    pool: PgPool,
    chain: C,
    config: ScanConfig,
    faults: F,
    /// 커밋 결과를 모르는 블록. 다음 `step`이 체인에 다시 묻지 않고 이 블록을 그대로 다시 처리한다
    retry: Option<Block>,
}

impl<C: ChainClient> Scanner<C, NoFaults> {
    /// 스캐너를 만든다. 처음이면 `config.start`를 저장해 커서로 삼고, 아니면 DB의 커서에서 이어 간다.
    pub async fn new(pool: PgPool, chain: C, config: ScanConfig) -> Result<Self, ScanError> {
        Self::with_faults(pool, chain, config, NoFaults).await
    }
}

impl<C: ChainClient, F: Faults> Scanner<C, F> {
    /// 장애 주입 자리를 넣어 만든다.
    pub async fn with_faults(
        pool: PgPool,
        chain: C,
        config: ScanConfig,
        faults: F,
    ) -> Result<Self, ScanError> {
        if config.confirmations == 0 {
            return Err(ScanError::BadBlock(
                "confirmations must be at least 1".into(),
            ));
        }
        init_cursor(&pool, &config.start).await?;
        Ok(Self {
            pool,
            chain,
            config,
            faults,
            retry: None,
        })
    }

    /// 블록 하나를 처리하거나 한 번 되감는다. 할 일이 없으면 `Step::Idle`.
    pub async fn step(&mut self) -> Result<Step, ScanError> {
        // 정지는 DB에 있어서 재시작해도 유지된다. 사람이 풀기 전에는 체인도 묻지 않는다
        if let Some(reason) = halted(&mut *self.pool.acquire().await?).await? {
            return Err(ScanError::Halted(reason));
        }
        // 커밋 결과를 모르는 블록은 커서와 상관없이 같은 블록을 다시 처리한다(ADR-002 결정 3).
        // 커밋이 됐다면 멱등 키와 조건부 커서 전진이 흡수하고, 안 됐다면 처음 처리와 같다
        if let Some(block) = self.retry.take() {
            return self.apply_block(block).await;
        }

        let cursor = load_cursor(&self.pool).await?;
        let next = BlockNumber(cursor.number.0 + 1);
        let head = rpc(self.chain.head().await)?;
        if head < next {
            return Ok(Step::Idle);
        }
        // `None`은 "아직 안 보인다"이다. 빈 블록으로 넘기지 않고 기다린다(I6)
        let Some(block) = rpc(self.chain.block_by_number(next).await)? else {
            return Ok(Step::Idle);
        };
        if block.header.number != next {
            return Err(ScanError::BadBlock(format!(
                "asked block {next}, got {}",
                block.header.number
            )));
        }

        if extends_cursor(&block.header, &cursor) {
            self.apply_block(block).await
        } else {
            self.rewind(cursor).await
        }
    }

    /// 새 head 알림. 내용은 쓰지 않고 스캔 신호로만 쓴다.
    pub async fn on_notification(&mut self, _n: Notification) -> Result<(), ScanError> {
        // 알림은 몇 번이 어떤 순서로 오든 "스캔하라"는 뜻일 뿐이다(ADR-002 결정 2).
        // 알림의 블록을 바로 처리하면 순서가 바뀐 알림이 커서를 건너뛰게 한다. 언제나 커서 다음을 끌어온다
        self.run_until_idle().await
    }

    /// `Step::Idle`이 나올 때까지 `step`을 돈다. 오류가 나면 그 오류로 멈춘다.
    pub async fn run_until_idle(&mut self) -> Result<(), ScanError> {
        loop {
            if self.step().await? == Step::Idle {
                return Ok(());
            }
        }
    }

    // ---- 블록 처리 -------------------------------------------------------------------------

    /// 블록 하나를 한 트랜잭션으로 처리한다(ADR-002 결정 3).
    async fn apply_block(&mut self, block: Block) -> Result<Step, ScanError> {
        let mut tx = self.pool.begin().await?;
        if !save_header(&mut tx, &block.header).await? {
            drop(tx);
            let reason = format!(
                "orphaned block {} {} is canonical again",
                block.header.number, block.header.hash
            );
            return self.halt(reason).await;
        }
        record_deposits(&mut tx, self.config.chain_id, &block).await?;
        confirm_ready(
            &mut tx,
            self.config.chain_id,
            block.header.number,
            self.config.confirmations,
        )
        .await?;
        advance_cursor(&mut tx, &block.header).await?;

        // 여기서부터 오류는 "커밋됐는지 모른다"이다. 같은 블록을 다시 처리하도록 남겨 둔다
        let header = block.header;
        if let Err(e) = tx.commit().await {
            self.retry = Some(block);
            return Err(ScanError::Db(e));
        }
        if let Err(e) = self.faults.after_commit(AFTER_BLOCK_COMMIT) {
            self.retry = Some(block);
            return Err(ScanError::CommitUnknown(e));
        }
        Ok(Step::Processed(header))
    }

    // ---- 되감기 ---------------------------------------------------------------------------

    /// 공통 조상까지 되감는다(ADR-002 결정 5, 7, 8).
    async fn rewind(&mut self, cursor: Cursor) -> Result<Step, ScanError> {
        // 조상 찾기는 읽기만 한다. 조회가 하나라도 결과를 모르면 아무것도 쓰지 않고 이 step을 끝낸다
        let ancestor = match self.find_common_ancestor(&cursor).await? {
            Ancestor::Found(a) => a,
            Ancestor::Wait => return Ok(Step::Idle),
            Ancestor::NotFound => {
                let reason = format!(
                    "no common ancestor down to start block {}",
                    self.config.start.number
                );
                return self.halt(reason).await;
            }
        };

        let mut tx = self.pool.begin().await?;
        let confirmed = deposits_above(&mut tx, ancestor.number, "confirmed").await?;
        if !confirmed.is_empty() {
            // 확정된 입금이 canonical에서 빠졌다: "N보다 깊은 reorg는 없다"는 정책 가정이 깨졌다.
            // 확정된 입금은 되돌리지 않는다. 그 사이 출금됐을 수 있어 자동 회수는 음수나 엉뚱한 회수를 만든다.
            // 그래서 원장·헤더·커서는 그대로 두고 사고 기록과 정지만 커밋한다
            let reason = record_violations(&mut tx, &confirmed).await?;
            set_halted(&mut tx, &reason).await?;
            tx.commit().await?;
            return Err(ScanError::Halted(reason));
        }

        // 확정 전 입금만 걸렸다: 고아 표시 → 잠정 입금 역분개 → 커서를 공통 조상으로
        let pending = deposits_above(&mut tx, ancestor.number, "pending").await?;
        orphan_above(&mut tx, ancestor.number).await?;
        for d in &pending {
            ledger::post(&mut tx, revert_entry(self.config.chain_id, d)).await?;
            set_status(&mut tx, d, "reverted").await?;
        }
        move_cursor_back(&mut tx, &cursor, &ancestor).await?;
        tx.commit().await?;
        Ok(Step::Rewound {
            from: cursor.number,
            to: ancestor.number,
        })
    }

    /// 커서에서 아래로 내려가며 체인의 해시와 저장된 해시가 처음 같아지는 곳을 찾는다.
    async fn find_common_ancestor(&self, cursor: &Cursor) -> Result<Ancestor, ScanError> {
        let mut conn = self.pool.acquire().await?;
        let mut n = cursor.number;
        loop {
            let stored = stored_canonical_hash(&mut conn, n).await?;
            // 번호가 아니라 해시로 비교한다. 같은 번호라도 다른 분기의 블록일 수 있다
            match rpc(self.chain.block_by_number(n).await)? {
                Some(b) if b.header.hash == stored => {
                    return Ok(Ancestor::Found(Cursor {
                        number: n,
                        hash: stored,
                    }));
                }
                Some(_) => {}
                // 체인이 그 사이 짧아졌다. 다음 step에서 다시 본다
                None => return Ok(Ancestor::Wait),
            }
            if n <= self.config.start.number {
                return Ok(Ancestor::NotFound);
            }
            n = BlockNumber(n.0 - 1);
        }
    }

    /// 정지 상태로 두고 커밋한다. 원장·헤더·커서는 건드리지 않는다.
    async fn halt(&mut self, reason: String) -> Result<Step, ScanError> {
        let mut tx = self.pool.begin().await?;
        set_halted(&mut tx, &reason).await?;
        tx.commit().await?;
        Err(ScanError::Halted(reason))
    }
}

enum Ancestor {
    Found(Cursor),
    /// 비교할 블록이 체인에 아직 없다
    Wait,
    /// 시작 블록까지 내려가도 같은 해시가 없다
    NotFound,
}

/// reorg 감지(ADR-002 결정 4). 번호가 커서 다음이어도 부모 해시가 커서 해시와 다르면 다른 분기의 블록이다.
fn extends_cursor(header: &BlockHeader, cursor: &Cursor) -> bool {
    header.parent_hash == cursor.hash
}

/// RPC 결과를 스캔 결과로. `Indeterminate`는 그대로 `Indeterminate`다(실패나 빈 결과로 바꾸지 않는다).
fn rpc<T>(outcome: RpcOutcome<T>) -> Result<T, ScanError> {
    match outcome {
        RpcOutcome::Ok(v) => Ok(v),
        RpcOutcome::Definite(e) => Err(ScanError::Rpc(e)),
        RpcOutcome::Indeterminate(m) => Err(ScanError::Indeterminate(m)),
    }
}

// ---- 분개 ----------------------------------------------------------------------------------
// 키와 줄은 ADR-001 결정 6의 표 그대로다. 키는 체인 ID, tx 해시, 블록 해시라는 외부 사실로만 만든다

/// 블록에서 본 우리 입금 하나.
#[derive(Debug, Clone)]
struct Deposit {
    tx_hash: Hash32,
    block_hash: Hash32,
    address: Address,
    user_id: i64,
    amount: Amount,
}

/// 잠정 입금: 차 `hot_wallet:{주소}` / 대 `pending_deposit:u`. 사용자는 아직 못 쓴다.
/// 키에 블록 해시가 있어 reorg 뒤 다른 블록에 다시 들어간 같은 tx는 새 잠정 분개가 된다.
fn pending_entry(chain_id: u64, d: &Deposit) -> NewEntry {
    NewEntry {
        idempotency_key: format!("dep:pending:{chain_id}:{}:{}", d.tx_hash, d.block_hash),
        kind: "dep:pending".into(),
        lines: vec![
            Line::debit(AccountKey::hot_wallet(d.address), d.amount),
            Line::credit(AccountKey::pending_deposit(d.user_id), d.amount),
        ],
    }
}

/// 잠정 입금 역분개: 잠정 입금의 줄을 뒤집는다. 두 줄 모두 있어야 차대가 맞는다.
fn revert_entry(chain_id: u64, d: &Deposit) -> NewEntry {
    NewEntry {
        idempotency_key: format!("dep:revert:{chain_id}:{}:{}", d.tx_hash, d.block_hash),
        kind: "dep:revert".into(),
        lines: vec![
            Line::debit(AccountKey::pending_deposit(d.user_id), d.amount),
            Line::credit(AccountKey::hot_wallet(d.address), d.amount),
        ],
    }
}

/// 입금 확정: 차 `pending_deposit:u` / 대 `user:u`. 키에 블록 해시가 없어 tx 하나는 한 번만 확정된다.
fn confirm_entry(chain_id: u64, d: &Deposit) -> NewEntry {
    NewEntry {
        idempotency_key: format!("dep:confirm:{chain_id}:{}", d.tx_hash),
        kind: "dep:confirm".into(),
        lines: vec![
            Line::debit(AccountKey::pending_deposit(d.user_id), d.amount),
            Line::credit(AccountKey::user(d.user_id), d.amount),
        ],
    }
}

// ---- SQL -----------------------------------------------------------------------------------

/// 시작 헤더를 처음 한 번만 저장하고 커서로 삼는다. 이미 커서가 있으면 그대로 둔다.
async fn init_cursor(pool: &PgPool, start: &BlockHeader) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO block_headers (hash, number, parent_hash) VALUES ($1, $2, $3)
         ON CONFLICT (hash) DO NOTHING",
    )
    .bind(start.hash.to_string())
    .bind(db_number(start.number)?)
    .bind(start.parent_hash.to_string())
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO scan_cursor (number, hash) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING",
    )
    .bind(db_number(start.number)?)
    .bind(start.hash.to_string())
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

async fn load_cursor(pool: &PgPool) -> Result<Cursor, ScanError> {
    cursor(&mut *pool.acquire().await?)
        .await?
        .ok_or_else(|| ScanError::Db(sqlx::Error::RowNotFound))
}

/// 헤더를 저장한다. 같은 해시가 이미 있으면 그대로 둔다(재처리).
/// 그 헤더가 고아로 표시돼 있으면 `false`: 한 번 빠진 블록이 다시 canonical이 됐다.
async fn save_header(
    tx: &mut Transaction<'_, Postgres>,
    h: &BlockHeader,
) -> Result<bool, sqlx::Error> {
    sqlx::query(
        "INSERT INTO block_headers (hash, number, parent_hash) VALUES ($1, $2, $3)
         ON CONFLICT (hash) DO NOTHING",
    )
    .bind(h.hash.to_string())
    .bind(db_number(h.number)?)
    .bind(h.parent_hash.to_string())
    .execute(&mut **tx)
    .await?;
    let orphaned: bool = sqlx::query_scalar("SELECT orphaned FROM block_headers WHERE hash = $1")
        .bind(h.hash.to_string())
        .fetch_one(&mut **tx)
        .await?;
    Ok(!orphaned)
}

/// 우리 입금 주소로 온 성공 tx마다 `deposits` 행과 잠정 분개를 쓴다(블록 안 순서대로).
async fn record_deposits(
    tx: &mut Transaction<'_, Postgres>,
    chain_id: u64,
    block: &Block,
) -> Result<(), ScanError> {
    for (position, t) in block.txs.iter().enumerate() {
        // 실패한 tx와 value 0 tx는 코인을 옮기지 않았다. 입금이 아니다
        if !t.success || t.value == 0 {
            continue;
        }
        let Some(user_id) = owner_of(tx, t.to).await? else {
            continue;
        };
        let amount = Amount::new(t.value).ok_or_else(|| {
            ScanError::BadBlock(format!("tx {} value {} too large", t.hash, t.value))
        })?;
        let d = Deposit {
            tx_hash: t.hash,
            block_hash: block.header.hash,
            address: t.to,
            user_id,
            amount,
        };
        sqlx::query(
            "INSERT INTO deposits (tx_hash, block_hash, position, address, user_id, amount, status)
             VALUES ($1, $2, $3, $4, $5, $6::numeric, 'pending')
             ON CONFLICT (tx_hash, block_hash) DO NOTHING",
        )
        .bind(d.tx_hash.to_string())
        .bind(d.block_hash.to_string())
        .bind(position as i32)
        .bind(d.address.to_string())
        .bind(d.user_id)
        .bind(d.amount.get().to_string())
        .execute(&mut **tx)
        .await?;
        // 행이 이미 있어도(재처리) 분개는 다시 보낸다. 중복을 막는 것은 행이 아니라 멱등 키다(I2)
        ledger::post(tx, pending_entry(chain_id, &d)).await?;
    }
    Ok(())
}

/// 확정 수가 N에 이른 잠정 입금을 확정한다(ADR-002 결정 1).
///
/// 확정 수 = h − (입금 블록 번호) + 1 ≥ N, 곧 입금 블록 번호 ≤ h − N + 1.
/// 깊이는 저장된 canonical 헤더(고아 아님)의 번호로 잰다. 받은 헤더 개수로 세면 고아 블록까지 깊이로 쳐진다.
async fn confirm_ready(
    tx: &mut Transaction<'_, Postgres>,
    chain_id: u64,
    h: BlockNumber,
    confirmations: u64,
) -> Result<(), ScanError> {
    let Some(deepest) = (h.0 + 1).checked_sub(confirmations) else {
        return Ok(());
    };
    let rows: Vec<DepositRow> = sqlx::query_as(
        "SELECT d.tx_hash, d.block_hash, d.address, d.user_id, d.amount::text
           FROM deposits d JOIN block_headers b ON b.hash = d.block_hash
          WHERE d.status = 'pending' AND NOT b.orphaned AND b.number <= $1
          ORDER BY b.number, d.position",
    )
    .bind(db_number(BlockNumber(deepest))?)
    .fetch_all(&mut **tx)
    .await?;
    for row in rows {
        let d = row.decode()?;
        ledger::post(tx, confirm_entry(chain_id, &d)).await?;
        set_status(tx, &d, "confirmed").await?;
    }
    Ok(())
}

/// 커서를 이 블록으로 옮긴다. 커서가 이 블록의 부모이거나(처음 처리) 이미 이 블록일 때(재처리)만 된다.
async fn advance_cursor(
    tx: &mut Transaction<'_, Postgres>,
    h: &BlockHeader,
) -> Result<(), ScanError> {
    let moved = sqlx::query(
        "UPDATE scan_cursor SET number = $1, hash = $2
          WHERE (number = $1 - 1 AND hash = $3) OR (number = $1 AND hash = $2)",
    )
    .bind(db_number(h.number)?)
    .bind(h.hash.to_string())
    .bind(h.parent_hash.to_string())
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if moved == 1 {
        Ok(())
    } else {
        Err(ScanError::CursorMoved)
    }
}

/// 저장된 canonical 헤더 중 공통 조상보다 위에 있는 블록의, 그 상태의 입금.
async fn deposits_above(
    tx: &mut Transaction<'_, Postgres>,
    ancestor: BlockNumber,
    status: &str,
) -> Result<Vec<Deposit>, ScanError> {
    let rows: Vec<DepositRow> = sqlx::query_as(
        "SELECT d.tx_hash, d.block_hash, d.address, d.user_id, d.amount::text
           FROM deposits d JOIN block_headers b ON b.hash = d.block_hash
          WHERE d.status = $2 AND NOT b.orphaned AND b.number > $1
          ORDER BY b.number, d.position",
    )
    .bind(db_number(ancestor)?)
    .bind(status)
    .fetch_all(&mut **tx)
    .await?;
    rows.into_iter().map(DepositRow::decode).collect()
}

/// 확정 입금마다 사고 기록 한 행. 정지 사유를 돌려준다.
async fn record_violations(
    tx: &mut Transaction<'_, Postgres>,
    confirmed: &[Deposit],
) -> Result<String, sqlx::Error> {
    for d in confirmed {
        sqlx::query(
            "INSERT INTO finality_violations (kind, block_hash, tx_hash, amount)
             VALUES ('deposit', $1, $2, $3::numeric)",
        )
        .bind(d.block_hash.to_string())
        .bind(d.tx_hash.to_string())
        .bind(d.amount.get().to_string())
        .execute(&mut **tx)
        .await?;
    }
    Ok(format!(
        "reorg removes {} confirmed deposit(s), first tx {} in block {}",
        confirmed.len(),
        confirmed[0].tx_hash,
        confirmed[0].block_hash
    ))
}

async fn orphan_above(
    tx: &mut Transaction<'_, Postgres>,
    ancestor: BlockNumber,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE block_headers SET orphaned = true WHERE number > $1 AND NOT orphaned")
        .bind(db_number(ancestor)?)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// 커서를 공통 조상으로. 되감기를 시작할 때 본 커서 그대로일 때만 된다.
async fn move_cursor_back(
    tx: &mut Transaction<'_, Postgres>,
    from: &Cursor,
    to: &Cursor,
) -> Result<(), ScanError> {
    let moved = sqlx::query(
        "UPDATE scan_cursor SET number = $3, hash = $4 WHERE number = $1 AND hash = $2",
    )
    .bind(db_number(from.number)?)
    .bind(from.hash.to_string())
    .bind(db_number(to.number)?)
    .bind(to.hash.to_string())
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if moved == 1 {
        Ok(())
    } else {
        Err(ScanError::CursorMoved)
    }
}

async fn set_status(
    tx: &mut Transaction<'_, Postgres>,
    d: &Deposit,
    status: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE deposits SET status = $3 WHERE tx_hash = $1 AND block_hash = $2")
        .bind(d.tx_hash.to_string())
        .bind(d.block_hash.to_string())
        .bind(status)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn set_halted(tx: &mut Transaction<'_, Postgres>, reason: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE scanner_state SET halted = true, reason = $1")
        .bind(reason)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn owner_of(
    tx: &mut Transaction<'_, Postgres>,
    address: Address,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT user_id FROM deposit_addresses WHERE address = $1")
        .bind(address.to_string())
        .fetch_optional(&mut **tx)
        .await
}

async fn stored_canonical_hash(
    conn: &mut PgConnection,
    n: BlockNumber,
) -> Result<Hash32, ScanError> {
    let h: String =
        sqlx::query_scalar("SELECT hash FROM block_headers WHERE number = $1 AND NOT orphaned")
            .bind(db_number(n)?)
            .fetch_one(conn)
            .await?;
    decode_hash(&h).map_err(ScanError::Db)
}

#[derive(sqlx::FromRow)]
struct DepositRow {
    tx_hash: String,
    block_hash: String,
    address: String,
    user_id: i64,
    amount: String,
}

impl DepositRow {
    fn decode(self) -> Result<Deposit, ScanError> {
        let bad = |what: &str| ScanError::Db(sqlx::Error::Decode(what.to_owned().into()));
        Ok(Deposit {
            tx_hash: decode_hash(&self.tx_hash)?,
            block_hash: decode_hash(&self.block_hash)?,
            address: Address::parse(&self.address).ok_or_else(|| bad("deposit address"))?,
            user_id: self.user_id,
            amount: self
                .amount
                .parse::<u128>()
                .ok()
                .and_then(Amount::new)
                .ok_or_else(|| bad("deposit amount"))?,
        })
    }
}

fn decode_hash(s: &str) -> Result<Hash32, sqlx::Error> {
    Hash32::parse(s).ok_or_else(|| sqlx::Error::Decode(format!("bad hash {s:?}").into()))
}

fn decode_cursor(n: i64, h: &str) -> Result<Cursor, sqlx::Error> {
    let number = u64::try_from(n)
        .map_err(|_| sqlx::Error::Decode(format!("bad block number {n}").into()))?;
    Ok(Cursor {
        number: BlockNumber(number),
        hash: decode_hash(h)?,
    })
}

fn db_number(n: BlockNumber) -> Result<i64, sqlx::Error> {
    i64::try_from(n.0)
        .map_err(|_| sqlx::Error::Encode(format!("block number {n} too large").into()))
}
