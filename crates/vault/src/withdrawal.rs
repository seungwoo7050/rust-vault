//! 출금(ADR-003). 요청·홀드(`request`)와 상태 기계를 돌리는 처리기(`Processor`).
//!
//! ```text
//! Held ─서명·nonce 할당─▶ Signed ─전송─▶ Sent ─┐
//!                            └─ 결과 모름 ─▶ Unknown ─┼─ 영수증, 확정 수 N, status 1 ─▶ Confirmed (wd:settle)
//!                                                    ├─ 영수증, 확정 수 N, status 0 ─▶ Failed (wd:fee + wd:release)
//!                                                    └─ nonce를 다른 tx가 씀, 또는 기한 초과 ─▶ NeedsReview (홀드 유지)
//!                                                         └─ 조회는 계속 돈다. 영수증이 나오면 위와 같이 결론
//! ```
//!
//! - `Processor::step`은 출금 하나의 전이 하나를 DB 트랜잭션 하나로 한다. 모든 커밋은 `store::commit`(`Site::Withdrawal`)을 거친다.
//! - 상태(출금 행, 서명 tx, 주소별 nonce)는 모두 DB에 있다. `Processor`를 새로 만들면 DB에서 이어 간다(결정 7).
//!   메모리의 작업 줄(`queue`)은 이번 회차에 볼 출금 ID 목록일 뿐이다.
//! - 서명 함수는 `Held → Signed`(`sign_held`)에서만 부른다. 그 뒤의 모든 전송은 저장한 raw 바이트다(결정 4).
//! - 전송 결과가 `Indeterminate`이든 어떤 `Definite`이든 `Failed`로 가는 전이는 없다. `Failed`는 영수증 status 0에서만 나온다(I6).
//! - 확정 수는 입금 스캐너의 커서로 센다(ADR-002와 같은 head 정의). 스캐너가 늦으면 결론도 늦다.

use std::collections::VecDeque;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::{PgConnection, PgPool, Postgres, Transaction};

use crate::chain::{Address, BlockNumber, ChainClient, Hash32, Receipt, RpcOutcome, SendRejection};
use crate::clock::Clock;
use crate::deposit;
use crate::ledger::{self, AccountKey, Amount, Line, NewEntry, PostError};
use crate::signer::{SignError, SignedTx, Signer, TRANSFER_GAS, TxRequest};
use crate::store::{self, CommitError, Failure, FaultPoint, Faults, Injected, NoFaults, Site};

/// 출금 요청. `request_id`는 호출자가 정하는 멱등 키다(같은 ID는 한 번만 처리된다, I2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithdrawalRequest {
    pub request_id: String,
    pub user: i64,
    pub to: Address,
    pub amount: Amount,
}

/// 출금 상태(ADR-003 결정 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    /// 홀드 분개를 기록했다. 아직 서명하지 않았다
    Held,
    /// 잔액이 모자라 받지 않았다. 분개는 없다
    Rejected,
    /// nonce를 할당하고 서명한 tx를 저장했다. 아직 보냈다는 응답을 못 받았다
    Signed,
    /// 노드가 받았거나, 이미 들어갔을 수 있다는 거절을 받았다
    Sent,
    /// 전송 결과를 모른다
    Unknown,
    /// 영수증 status 1, 확정 수 N. 지급 분개를 기록했다
    Confirmed,
    /// 영수증 status 0, 확정 수 N. 가스비 분개와 홀드 해제를 기록했다
    Failed,
    /// 사람이 봐야 한다. 홀드는 유지하고 조회는 계속한다
    NeedsReview,
}

impl Status {
    pub const ALL: [Status; 8] = [
        Status::Held,
        Status::Rejected,
        Status::Signed,
        Status::Sent,
        Status::Unknown,
        Status::Confirmed,
        Status::Failed,
        Status::NeedsReview,
    ];

    /// DB의 `withdrawals.status` 값.
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Held => "held",
            Status::Rejected => "rejected",
            Status::Signed => "signed",
            Status::Sent => "sent",
            Status::Unknown => "unknown",
            Status::Confirmed => "confirmed",
            Status::Failed => "failed",
            Status::NeedsReview => "needs_review",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Status::ALL.into_iter().find(|v| v.as_str() == s)
    }

    /// 결론이 났는가. 결론 난 출금은 처리기가 다시 보지 않는다.
    pub fn is_final(self) -> bool {
        matches!(self, Status::Rejected | Status::Confirmed | Status::Failed)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 출금 행.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withdrawal {
    pub request_id: String,
    pub user: i64,
    pub to: Address,
    pub amount: Amount,
    pub status: Status,
}

/// 저장한 서명 tx(`signed_txs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTx {
    pub from: Address,
    pub nonce: u64,
    pub raw: Vec<u8>,
    pub hash: Hash32,
    pub signed_at: SystemTime,
}

/// 처리기 설정.
#[derive(Debug, Clone, Copy)]
pub struct WithdrawalConfig {
    pub chain_id: u64,
    /// 확정 수 N. 스캐너와 같은 값이어야 한다
    pub confirmations: u64,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    /// 서명 뒤 이만큼 지나도 결론이 안 나면 `NeedsReview`(ADR-003 결정 3)
    pub review_after: Duration,
    /// `run_until_idle`이 일시 오류 뒤 같은 출금을 다시 보는 최대 횟수(연속)
    pub transient_retries: u32,
}

/// `step` 하나의 결과.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// 출금 하나가 한 상태에서 다른 상태로 갔다
    Moved {
        request_id: String,
        from: Status,
        to: Status,
    },
    /// 출금 하나를 봤지만 상태는 그대로다(영수증이 아직 없거나 조회 결과를 모름)
    Waiting { request_id: String, status: Status },
    /// 볼 출금이 없다
    Idle,
}

/// 출금 오류.
#[derive(Debug)]
pub enum WithdrawalError {
    /// 확정 뒤 깊이의 reorg 사고 기록이 있다. 서명·전송을 하지 않는다(ADR-003 결정 6)
    Halted(String),
    /// 같은 `request_id`로 내용이 다른 요청이 이미 있다
    RequestConflict(String),
    /// 그 출금이 없다
    NotFound(String),
    /// 주입된 프로세스 종료. 이 `Processor`는 버리고 새로 만든다(재시작)
    Crashed(FaultPoint),
    /// 일시 DB 오류. 열린 트랜잭션은 버렸다. 다시 하면 된다
    Transient(String),
    /// 커밋 결과를 모른다. 다음 `step`이 DB의 상태에서 다시 본다
    CommitUnknown(String),
    Sign(SignError),
    Ledger(PostError),
    /// DB에 든 값을 읽을 수 없다
    Corrupt(String),
    Db(sqlx::Error),
}

impl fmt::Display for WithdrawalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WithdrawalError::Halted(r) => write!(f, "withdrawals halted: {r}"),
            WithdrawalError::RequestConflict(id) => {
                write!(f, "request {id} already exists with a different body")
            }
            WithdrawalError::NotFound(id) => write!(f, "withdrawal {id} not found"),
            WithdrawalError::Crashed(p) => write!(f, "crashed at {p}"),
            WithdrawalError::Transient(e) => write!(f, "transient database error: {e}"),
            WithdrawalError::CommitUnknown(e) => write!(f, "commit result unknown: {e}"),
            WithdrawalError::Sign(e) => write!(f, "{e}"),
            WithdrawalError::Ledger(e) => write!(f, "ledger: {e}"),
            WithdrawalError::Corrupt(m) => write!(f, "corrupt row: {m}"),
            WithdrawalError::Db(e) => write!(f, "database: {e}"),
        }
    }
}

impl std::error::Error for WithdrawalError {}

impl From<sqlx::Error> for WithdrawalError {
    fn from(e: sqlx::Error) -> Self {
        if store::is_transient(&e) {
            WithdrawalError::Transient(e.to_string())
        } else {
            WithdrawalError::Db(e)
        }
    }
}

impl From<PostError> for WithdrawalError {
    fn from(e: PostError) -> Self {
        match e {
            PostError::Db(e) => e.into(),
            e => WithdrawalError::Ledger(e),
        }
    }
}

impl From<SignError> for WithdrawalError {
    fn from(e: SignError) -> Self {
        WithdrawalError::Sign(e)
    }
}

type Result<T, E = WithdrawalError> = std::result::Result<T, E>;

// ---- 요청 ----------------------------------------------------------------------------------

/// 출금을 요청한다. 요청 저장과 홀드 분개를 한 트랜잭션에서 한다(ADR-003 결정 2의 `Held`).
///
/// - 같은 `request_id`가 이미 있으면 아무것도 쓰지 않고 기존 행을 돌려준다(I2). 내용이 다르면 `RequestConflict`.
/// - `user:u` 잔액이 모자라면 `Rejected` 행만 남기고 분개는 없다. 잔액 검사는 원장이 계정을 잠그고 한다.
pub async fn request(pool: &PgPool, req: WithdrawalRequest) -> Result<Withdrawal> {
    if let Some(existing) = get(pool, &req.request_id).await? {
        return same_request(existing, &req);
    }
    let user = AccountKey::user(req.user);
    let mut tx = pool.begin().await?;
    ledger::open_account(&mut tx, &user).await?;
    ledger::open_account(&mut tx, &AccountKey::withdrawal_pending(req.user)).await?;
    let status = match ledger::post(&mut tx, hold_entry(&req)).await {
        Ok(_) => Status::Held,
        Err(PostError::InsufficientFunds(k)) if k == user => Status::Rejected,
        Err(e) => return Err(e.into()),
    };
    if !insert_request(&mut tx, &req, status).await? {
        // 같은 ID의 요청이 동시에 먼저 커밋됐다. 이 트랜잭션(홀드 포함)은 버리고 그 행을 돌려준다
        drop(tx);
        let existing = get(pool, &req.request_id)
            .await?
            .ok_or_else(|| WithdrawalError::NotFound(req.request_id.clone()))?;
        return same_request(existing, &req);
    }
    store::commit(tx, &NoFaults, Site::Withdrawal)
        .await
        .map_err(commit_error)?;
    Ok(Withdrawal {
        request_id: req.request_id,
        user: req.user,
        to: req.to,
        amount: req.amount,
        status,
    })
}

fn same_request(existing: Withdrawal, req: &WithdrawalRequest) -> Result<Withdrawal> {
    if existing.user == req.user && existing.to == req.to && existing.amount == req.amount {
        Ok(existing)
    } else {
        Err(WithdrawalError::RequestConflict(req.request_id.clone()))
    }
}

/// 출금 행 하나.
pub async fn get(pool: &PgPool, request_id: &str) -> Result<Option<Withdrawal>> {
    let row: Option<WithdrawalRow> = sqlx::query_as(
        "SELECT request_id, user_id, to_address, amount::text, status
           FROM withdrawals WHERE request_id = $1",
    )
    .bind(request_id)
    .fetch_optional(pool)
    .await?;
    row.map(WithdrawalRow::decode).transpose()
}

/// 그 출금의 저장된 서명 tx. 서명 전이면 `None`.
pub async fn signed_tx(pool: &PgPool, request_id: &str) -> Result<Option<StoredTx>> {
    load_signed(&mut *pool.acquire().await?, request_id).await
}

/// 출금 처리를 멈출 사유. 확정 뒤 깊이의 reorg 사고 기록이 하나라도 있으면 멈춘다(ADR-003 결정 6).
pub async fn halted(conn: &mut PgConnection) -> Result<Option<String>> {
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM finality_violations")
        .fetch_one(conn)
        .await?;
    Ok((n > 0).then(|| format!("{n} finality violation(s) recorded")))
}

// ---- 처리기 --------------------------------------------------------------------------------

/// 출금 상태 기계를 돌린다. `C` 체인, `S` 서명기, `K` 시계, `F` 장애 주입 자리.
pub struct Processor<C, S, K, F = NoFaults> {
    pool: PgPool,
    chain: C,
    signer: S,
    clock: K,
    faults: F,
    config: WithdrawalConfig,
    /// 이번 회차에 볼 출금 ID. 비면 DB에서 다시 채운다(상태가 아니라 순서일 뿐이다)
    queue: VecDeque<String>,
}

impl<C: ChainClient, S: Signer, K: Clock, F: Faults> Processor<C, S, K, F> {
    /// 처리기를 만든다. 출금 주소의 계정(`hot_wallet:{주소}`, `network_fee`)을 연다.
    ///
    /// 재시작은 이것을 다시 부르는 것이다(ADR-003 결정 7). 따로 도는 복구 루틴은 없다:
    /// `Held`는 서명, `Signed`는 저장한 raw tx 재전송, `Sent`·`Unknown`·`NeedsReview`는 조회로
    /// 평소의 `step`이 DB의 상태에서 그대로 이어 간다.
    pub async fn new(
        pool: PgPool,
        chain: C,
        signer: S,
        clock: K,
        faults: F,
        config: WithdrawalConfig,
    ) -> Result<Self> {
        let mut conn = pool.acquire().await?;
        ledger::open_account(&mut conn, &AccountKey::hot_wallet(signer.address())).await?;
        ledger::open_account(&mut conn, &AccountKey::network_fee()).await?;
        drop(conn);
        Ok(Self {
            pool,
            chain,
            signer,
            clock,
            faults,
            config,
            queue: VecDeque::new(),
        })
    }

    /// 출금 하나의 전이 하나. 이번 회차 작업 줄이 비었으면 DB에서 결론 안 난 출금을 다시 읽는다.
    pub async fn step(&mut self) -> Result<Step> {
        if self.queue.is_empty() {
            self.queue = open_withdrawals(&self.pool).await?;
        }
        let Some(id) = self.queue.pop_front() else {
            return Ok(Step::Idle);
        };
        self.advance(&id).await
    }

    /// 결론 안 난 출금을 한 번씩 보는 회차를, 한 회차에 아무 전이도 없을 때까지 돈다.
    /// 일시 오류면 같은 출금을 다시 본다(연속 `transient_retries`번까지). 다른 오류는 돌려준다.
    pub async fn run_until_idle(&mut self) -> Result<()> {
        loop {
            self.queue = open_withdrawals(&self.pool).await?;
            let mut moved = false;
            while let Some(id) = self.queue.pop_front() {
                let mut failures = 0;
                loop {
                    match self.advance(&id).await {
                        Ok(Step::Moved { .. }) => moved = true,
                        Ok(_) => {}
                        Err(WithdrawalError::Transient(_))
                            if failures < self.config.transient_retries =>
                        {
                            failures += 1;
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                    break;
                }
            }
            if !moved {
                return Ok(());
            }
        }
    }

    /// 출금 하나를 지금 상태에 맞는 전이로 보낸다.
    async fn advance(&mut self, id: &str) -> Result<Step> {
        // 사고 기록이 있으면 체인에 아무것도 보내지 않는다(ADR-003 결정 6). 사람이 판단한다
        if let Some(reason) = halted(&mut *self.pool.acquire().await?).await? {
            return Err(WithdrawalError::Halted(reason));
        }
        let w = get(&self.pool, id)
            .await?
            .ok_or_else(|| WithdrawalError::NotFound(id.to_owned()))?;
        match w.status {
            Status::Held => self.sign_held(&w).await,
            Status::Signed => self.send_signed(&w).await,
            Status::Sent | Status::Unknown | Status::NeedsReview => self.check_chain(&w).await,
            Status::Rejected | Status::Confirmed | Status::Failed => Ok(waiting(&w)),
        }
    }

    // ---- Held → Signed ----------------------------------------------------------------------

    /// nonce 할당 → 서명 → 서명 tx 저장 → `Signed`를 한 트랜잭션에서 한다(ADR-003 결정 2, 5).
    /// 커밋 전에 죽거나 커밋이 실패하면 nonce도 서명 tx도 없던 일이 되고, 아직 아무것도 보내지 않았다(I5).
    /// 서명기를 부르는 곳은 여기뿐이다(결정 4).
    async fn sign_held(&mut self, w: &Withdrawal) -> Result<Step> {
        let mut tx = self.pool.begin().await?;
        if lock_status(&mut tx, &w.request_id).await? != Status::Held {
            return Ok(waiting(w));
        }
        let from = self.signer.address();
        let Some(nonce) = self.allocate_nonce(&mut tx, from).await? else {
            // 처음 nonce를 체인에서 못 읽었다. 아무것도 쓰지 않고 다음 회차에 다시 본다
            return Ok(waiting(w));
        };
        let signed = self.signer.sign(TxRequest {
            chain_id: self.config.chain_id,
            nonce,
            to: w.to,
            value: w.amount.get(),
            gas_limit: TRANSFER_GAS,
            max_fee_per_gas: self.config.max_fee_per_gas,
            max_priority_fee_per_gas: self.config.max_priority_fee_per_gas,
        })?;
        let now = self.clock.now();
        insert_signed(&mut tx, &w.request_id, from, nonce, &signed, now).await?;
        self.finish(tx, w, Status::Held, Status::Signed).await
    }

    /// 출금 주소의 다음 nonce를 잠그고 할당한다(ADR-003 결정 5).
    /// 행이 없을 때만 체인의 확정 nonce로 채운다. 그 뒤로는 DB 행이 원본이고 체인에 묻지 않는다.
    async fn allocate_nonce(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        from: Address,
    ) -> Result<Option<u64>> {
        let next = match lock_nonce(tx, from).await? {
            Some(n) => n,
            None => {
                let block = self.confirmed_block().await?;
                let RpcOutcome::Ok(n) = self.chain.nonce_at(from, block).await else {
                    return Ok(None);
                };
                init_nonce(tx, from, n).await?;
                lock_nonce(tx, from)
                    .await?
                    .ok_or_else(|| WithdrawalError::Corrupt("wallet_nonces row vanished".into()))?
            }
        };
        set_next_nonce(tx, from, next + 1).await?;
        Ok(Some(next))
    }

    // ---- Signed → Sent | Unknown ------------------------------------------------------------

    /// 저장한 raw tx를 보내고 그 응답을 상태로 쓴다(ADR-003 결정 2).
    /// 재시작 뒤의 `Signed`도 이 길로 같은 바이트를 다시 보낸다(결정 7). 다시 서명하지 않는다.
    async fn send_signed(&mut self, w: &Withdrawal) -> Result<Step> {
        let st = self.stored(w).await?;
        let outcome = self.chain.send_raw(&st.raw).await;
        // 응답을 받고 상태를 쓰기 전에 죽으면 `Signed`로 남는다. 재시작이 같은 바이트를 다시 보내고
        // 노드는 "already known"이나 "nonce too low"를 돌려준다(결정 7, T5.3)
        self.inject(FaultPoint::AfterSend)?;
        let to = status_after_send(&outcome);
        let tx = self.pool.begin().await?;
        self.finish(tx, w, Status::Signed, to).await
    }

    // ---- Sent | Unknown | NeedsReview → 조회 -------------------------------------------------

    /// 체인 조회로만 결론 낸다(ADR-003 결정 3). 한 번에 다음 중 하나를 한다.
    /// 1. 영수증이 확정 수 N에 이르렀으면 `Confirmed` 또는 `Failed`(결론)
    /// 2. 영수증이 없고 확정 nonce가 내 nonce를 지났으면 `NeedsReview`(다른 tx가 그 nonce를 썼다)
    /// 3. 영수증이 없고 확정 nonce가 내 nonce 이하면 같은 raw tx를 다시 보낸다
    /// 4. 결론이 없으면 기한을 본다(`NeedsReview`), 아니면 재전송 응답에 따라 `Unknown → Sent`
    async fn check_chain(&mut self, w: &Withdrawal) -> Result<Step> {
        let st = self.stored(w).await?;
        let resent = match self.chain.receipt(st.hash).await {
            RpcOutcome::Ok(Some(r)) => {
                if self.is_final(&r).await? {
                    return self.conclude(w, &st, &r).await;
                }
                // 블록에 들어갔지만 아직 확정 수 N이 아니다(또는 스캐너가 그 블록을 아직 못 봤다). 기다린다
                None
            }
            RpcOutcome::Ok(None) => match self
                .chain
                .nonce_at(st.from, self.confirmed_block().await?)
                .await
            {
                RpcOutcome::Ok(n) if n > st.nonce => return self.nonce_taken(w).await,
                RpcOutcome::Ok(_) => Some(self.resend(&st).await),
                // 확정 nonce를 모른다. 이번에는 아무것도 하지 않는다
                _ => None,
            },
            // 영수증 조회 결과를 모른다. 없다고 보지 않는다(I6)
            _ => None,
        };
        if self.past_deadline(&st) {
            return self.deadline_passed(w).await;
        }
        match resent {
            Some(accepted) => self.after_resend(w, accepted).await,
            None => Ok(waiting(w)),
        }
    }

    /// 영수증이 확정 수 N에 이르렀는가. 블록 해시가 스캐너가 저장한 canonical 헤더에 있어야 하고,
    /// 확정 수 = 스캐너 커서 번호 − 영수증 블록 번호 + 1이다(ADR-002 결정 1과 같은 head).
    async fn is_final(&self, r: &Receipt) -> Result<bool> {
        let mut conn = self.pool.acquire().await?;
        let Some(cursor) = deposit::cursor(&mut conn).await? else {
            return Ok(false);
        };
        if !is_stored_canonical(&mut conn, r).await? {
            return Ok(false);
        }
        let Some(depth) = (cursor.number.0 + 1).checked_sub(r.block_number.0) else {
            return Ok(false);
        };
        Ok(depth >= self.config.confirmations)
    }

    /// 결론(ADR-003 결정 3). status 1이면 `Confirmed`와 지급 분개, status 0이면 `Failed`와
    /// 가스비 분개·홀드 해제. `NeedsReview`에서도 같다(사람 결정 (a): 영수증이 나오면 결론).
    async fn conclude(&mut self, w: &Withdrawal, st: &StoredTx, r: &Receipt) -> Result<Step> {
        let mut tx = self.pool.begin().await?;
        let from = lock_status(&mut tx, &w.request_id).await?;
        if !matches!(from, Status::Sent | Status::Unknown | Status::NeedsReview) {
            return Ok(waiting(w));
        }
        let to = if r.success {
            ledger::post(&mut tx, settle_entry(w, st, r)).await?;
            Status::Confirmed
        } else {
            if let Some(fee) = fee_entry(self.config.chain_id, st, r) {
                ledger::post(&mut tx, fee).await?;
            }
            ledger::post(&mut tx, release_entry(w)).await?;
            Status::Failed
        };
        set_block_hash(&mut tx, &w.request_id, r.block_hash).await?;
        self.finish(tx, w, from, to).await
    }

    /// 영수증이 없는데 확정 nonce가 내 nonce를 지났다: 다른 tx가 그 nonce를 썼다(F5, ADR-003 결정 3).
    /// 내 tx는 이제 들어갈 수 없지만 그 tx가 같은 수취인에게 지급했을 수 있다. 홀드를 풀지 않고 `NeedsReview`.
    async fn nonce_taken(&mut self, w: &Withdrawal) -> Result<Step> {
        self.mark_needs_review(w).await
    }

    /// 기한 안에 결론이 안 났다(ADR-003 결정 3). 홀드는 유지하고 `NeedsReview`. 자동 `Failed`는 없다(I6).
    async fn deadline_passed(&mut self, w: &Withdrawal) -> Result<Step> {
        self.mark_needs_review(w).await
    }

    /// `Sent`·`Unknown` → `NeedsReview`. 이미 `NeedsReview`면 그대로 둔다(조회는 계속한다).
    async fn mark_needs_review(&mut self, w: &Withdrawal) -> Result<Step> {
        if w.status == Status::NeedsReview {
            return Ok(waiting(w));
        }
        let tx = self.pool.begin().await?;
        self.finish(tx, w, w.status, Status::NeedsReview).await
    }

    /// 아직 안 들어간 tx를 다시 보낸다. 저장한 raw 바이트 그대로다(ADR-003 결정 4).
    /// 같은 해시라 체인에는 최대 한 번 들어간다. 돌려주는 값은 "노드가 받았다"(`Ok`·"already known")인지다.
    async fn resend(&self, st: &StoredTx) -> bool {
        let outcome = self.chain.send_raw(&st.raw).await;
        resend_accepted(&outcome)
    }

    /// 재전송 응답을 상태로(ADR-003 결정 2). `Unknown`에서 노드가 받았으면 `Sent`, 그 밖은 그대로다.
    /// `NeedsReview`는 돌아가지 않는다(사람에게 보인 표시를 조용히 지우지 않는다. 결론만 그것을 끝낸다).
    async fn after_resend(&mut self, w: &Withdrawal, accepted: bool) -> Result<Step> {
        if !(accepted && w.status == Status::Unknown) {
            return Ok(waiting(w));
        }
        let tx = self.pool.begin().await?;
        self.finish(tx, w, Status::Unknown, Status::Sent).await
    }

    fn past_deadline(&self, st: &StoredTx) -> bool {
        self.clock
            .now()
            .duration_since(st.signed_at)
            .is_ok_and(|age| age >= self.config.review_after)
    }

    // ---- 공통 --------------------------------------------------------------------------------

    /// 상태를 `from`에서 `to`로 바꾸고 커밋한다. 그 사이 다른 쪽이 상태를 바꿨으면 아무것도 쓰지 않는다.
    async fn finish(
        &mut self,
        mut tx: Transaction<'_, Postgres>,
        w: &Withdrawal,
        from: Status,
        to: Status,
    ) -> Result<Step> {
        if !move_status(&mut tx, &w.request_id, from, to, self.clock.now()).await? {
            return Ok(waiting(w));
        }
        store::commit(tx, &self.faults, Site::Withdrawal)
            .await
            .map_err(commit_error)?;
        Ok(Step::Moved {
            request_id: w.request_id.clone(),
            from,
            to,
        })
    }

    async fn stored(&self, w: &Withdrawal) -> Result<StoredTx> {
        load_signed(&mut *self.pool.acquire().await?, &w.request_id)
            .await?
            .ok_or_else(|| WithdrawalError::Corrupt(format!("{} has no signed tx", w.request_id)))
    }

    /// 확정 nonce를 볼 블록: 스캐너 커서 − N + 1(커서가 그보다 낮으면 0).
    async fn confirmed_block(&self) -> Result<BlockNumber> {
        let cursor = deposit::cursor(&mut *self.pool.acquire().await?).await?;
        let head = cursor.map_or(0, |c| c.number.0);
        Ok(BlockNumber(
            (head + 1).saturating_sub(self.config.confirmations),
        ))
    }

    fn inject(&self, point: FaultPoint) -> Result<()> {
        self.faults.hit(point).map_err(|i| injected(point, i))
    }
}

fn waiting(w: &Withdrawal) -> Step {
    Step::Waiting {
        request_id: w.request_id.clone(),
        status: w.status,
    }
}

/// 첫 전송의 응답 → 상태(ADR-003 결정 2).
/// - `Ok`: 노드가 받았다 → `Sent`
/// - "already known"·"nonce too low"·"replacement underpriced": 이미 들어갔을 수 있다 → `Sent`(조회가 결론 낸다)
/// - 그 밖의 `Definite`: 이 노드가 지금 거절했다는 것이 같은 tx가 다른 곳에 없다는 증거는 아니다 → `Unknown`
/// - `Indeterminate`: 결과를 모른다 → `Unknown`. 실패로 바꾸지 않는다(I6)
fn status_after_send(outcome: &RpcOutcome<Hash32>) -> Status {
    match outcome {
        RpcOutcome::Ok(_) => Status::Sent,
        RpcOutcome::Definite(e) => match e.send_rejection() {
            SendRejection::AlreadyKnown
            | SendRejection::NonceTooLow
            | SendRejection::ReplacementUnderpriced => Status::Sent,
            SendRejection::Other(_) => Status::Unknown,
        },
        RpcOutcome::Indeterminate(_) => Status::Unknown,
    }
}

/// 재전송 응답이 "노드가 받았다"인가(`Ok`·"already known"). 그 밖은 상태를 바꾸지 않는다(ADR-003 결정 2).
fn resend_accepted(outcome: &RpcOutcome<Hash32>) -> bool {
    match outcome {
        RpcOutcome::Ok(_) => true,
        RpcOutcome::Definite(e) => e.send_rejection() == SendRejection::AlreadyKnown,
        RpcOutcome::Indeterminate(_) => false,
    }
}

fn injected(point: FaultPoint, i: Injected) -> WithdrawalError {
    match i {
        Injected::Crash => WithdrawalError::Crashed(point),
        Injected::Transient => WithdrawalError::Transient(format!("injected at {point}")),
        Injected::CommitOutcomeUnknown => {
            WithdrawalError::CommitUnknown(format!("injected at {point}"))
        }
    }
}

/// 커밋 오류를 출금 오류로. 커밋됐는지 모르는 경우도 다음 `step`이 DB에서 상태를 다시 읽으므로 따로 기억하지 않는다.
fn commit_error(e: CommitError) -> WithdrawalError {
    match e {
        CommitError::NotCommitted(Failure::Injected(p, Injected::CommitOutcomeUnknown)) => {
            WithdrawalError::Transient(format!("injected at {p}"))
        }
        CommitError::NotCommitted(Failure::Injected(p, i))
        | CommitError::Unknown(Failure::Injected(p, i)) => injected(p, i),
        CommitError::NotCommitted(Failure::Db(e)) => e.into(),
        CommitError::Unknown(Failure::Db(e)) if store::is_transient(&e) => {
            WithdrawalError::Transient(e.to_string())
        }
        CommitError::Unknown(Failure::Db(e)) => WithdrawalError::CommitUnknown(e.to_string()),
    }
}

// ---- 분개 ----------------------------------------------------------------------------------
// 키와 줄은 ADR-001 결정 6의 표 그대로다

/// 홀드: 차 `user:u` / 대 `withdrawal_pending:u`.
fn hold_entry(req: &WithdrawalRequest) -> NewEntry {
    NewEntry {
        idempotency_key: format!("wd:hold:{}", req.request_id),
        kind: "wd:hold".into(),
        lines: vec![
            Line::debit(AccountKey::user(req.user), req.amount),
            Line::credit(AccountKey::withdrawal_pending(req.user), req.amount),
        ],
    }
}

/// 지급: 차 `withdrawal_pending:u` 금액, 차 `network_fee` 가스비 / 대 `hot_wallet:{출금 주소}` 금액 + 가스비.
fn settle_entry(w: &Withdrawal, st: &StoredTx, r: &Receipt) -> NewEntry {
    let fee = r.fee();
    let mut lines = vec![Line::debit(
        AccountKey::withdrawal_pending(w.user),
        w.amount,
    )];
    if let Some(f) = Amount::new(fee) {
        lines.push(Line::debit(AccountKey::network_fee(), f));
    }
    let total = Amount::new(w.amount.get() + fee).expect("금액 + 가스비는 0이 아니다");
    lines.push(Line::credit(AccountKey::hot_wallet(st.from), total));
    NewEntry {
        idempotency_key: format!("wd:settle:{}", w.request_id),
        kind: "wd:settle".into(),
        lines,
    }
}

/// 실패 tx 가스비: 차 `network_fee` / 대 `hot_wallet:{출금 주소}`. 가스비가 0이면 분개가 없다.
fn fee_entry(chain_id: u64, st: &StoredTx, r: &Receipt) -> Option<NewEntry> {
    let fee = Amount::new(r.fee())?;
    Some(NewEntry {
        idempotency_key: format!("wd:fee:{chain_id}:{}", st.hash),
        kind: "wd:fee".into(),
        lines: vec![
            Line::debit(AccountKey::network_fee(), fee),
            Line::credit(AccountKey::hot_wallet(st.from), fee),
        ],
    })
}

/// 홀드 해제: 차 `withdrawal_pending:u` / 대 `user:u`.
fn release_entry(w: &Withdrawal) -> NewEntry {
    NewEntry {
        idempotency_key: format!("wd:release:{}", w.request_id),
        kind: "wd:release".into(),
        lines: vec![
            Line::debit(AccountKey::withdrawal_pending(w.user), w.amount),
            Line::credit(AccountKey::user(w.user), w.amount),
        ],
    }
}

// ---- 확정 뒤 깊이의 reorg(입금 스캐너가 부른다) ---------------------------------------------

/// 저장된 canonical 헤더 중 `ancestor`보다 위 블록에서 결론 난 출금의 tx.
#[derive(Debug, Clone)]
pub(crate) struct ConcludedTx {
    pub block_hash: String,
    pub tx_hash: String,
    pub amount: String,
}

/// 되감기가 고아로 만들 블록(`ancestor` 위의 canonical 헤더)에서 결론 난(`Confirmed`·`Failed`) 출금의 tx.
/// 하나라도 있으면 확정 정책 가정이 깨진 것이다(ADR-003 결정 6, ADR-002 결정 7).
pub(crate) async fn concluded_above(
    tx: &mut Transaction<'_, Postgres>,
    ancestor: BlockNumber,
) -> Result<Vec<ConcludedTx>, sqlx::Error> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT w.block_hash, s.tx_hash, w.amount::text
           FROM withdrawals w
           JOIN signed_txs s ON s.request_id = w.request_id
           JOIN block_headers b ON b.hash = w.block_hash
          WHERE w.status IN ('confirmed', 'failed') AND NOT b.orphaned AND b.number > $1
          ORDER BY b.number, s.nonce",
    )
    .bind(db_i64(ancestor.0)?)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(block_hash, tx_hash, amount)| ConcludedTx {
            block_hash,
            tx_hash,
            amount,
        })
        .collect())
}

// ---- SQL -----------------------------------------------------------------------------------

/// 결론 안 난 출금 ID(요청 순서).
async fn open_withdrawals(pool: &PgPool) -> Result<VecDeque<String>> {
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT request_id FROM withdrawals
          WHERE status IN ('held', 'signed', 'sent', 'unknown', 'needs_review')
          ORDER BY created_at, request_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(ids.into())
}

/// 요청 행을 넣는다. 같은 ID가 이미 있으면 `false`. 시각은 요청 시점의 DB 시각이다.
async fn insert_request(
    tx: &mut Transaction<'_, Postgres>,
    req: &WithdrawalRequest,
    status: Status,
) -> Result<bool, sqlx::Error> {
    let n = sqlx::query(
        "INSERT INTO withdrawals
             (request_id, user_id, to_address, amount, status, created_at, status_changed_at)
         VALUES ($1, $2, $3, $4::numeric, $5, now(), now())
         ON CONFLICT (request_id) DO NOTHING",
    )
    .bind(&req.request_id)
    .bind(req.user)
    .bind(req.to.to_string())
    .bind(req.amount.get().to_string())
    .bind(status.as_str())
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// 출금 행을 잠그고 지금 상태를 읽는다.
async fn lock_status(tx: &mut Transaction<'_, Postgres>, id: &str) -> Result<Status> {
    let s: String =
        sqlx::query_scalar("SELECT status FROM withdrawals WHERE request_id = $1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
    Status::parse(&s).ok_or_else(|| WithdrawalError::Corrupt(format!("status {s:?}")))
}

/// 상태가 `from`일 때만 `to`로 바꾼다.
async fn move_status(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    from: Status,
    to: Status,
    now: SystemTime,
) -> Result<bool> {
    let n = sqlx::query(
        "UPDATE withdrawals
            SET status = $3, status_changed_at = timestamptz 'epoch' + $4 * interval '1 microsecond'
          WHERE request_id = $1 AND status = $2",
    )
    .bind(id)
    .bind(from.as_str())
    .bind(to.as_str())
    .bind(micros(now)?)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(n == 1)
}

async fn set_block_hash(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    block: Hash32,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE withdrawals SET block_hash = $2 WHERE request_id = $1")
        .bind(id)
        .bind(block.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn lock_nonce(tx: &mut Transaction<'_, Postgres>, address: Address) -> Result<Option<u64>> {
    let n: Option<i64> =
        sqlx::query_scalar("SELECT next_nonce FROM wallet_nonces WHERE address = $1 FOR UPDATE")
            .bind(address.to_string())
            .fetch_optional(&mut **tx)
            .await?;
    n.map(|n| u64::try_from(n).map_err(|_| WithdrawalError::Corrupt(format!("nonce {n}"))))
        .transpose()
}

async fn init_nonce(
    tx: &mut Transaction<'_, Postgres>,
    address: Address,
    next: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO wallet_nonces (address, next_nonce) VALUES ($1, $2)
         ON CONFLICT (address) DO NOTHING",
    )
    .bind(address.to_string())
    .bind(db_i64(next)?)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn set_next_nonce(
    tx: &mut Transaction<'_, Postgres>,
    address: Address,
    next: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE wallet_nonces SET next_nonce = $2 WHERE address = $1")
        .bind(address.to_string())
        .bind(db_i64(next)?)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn insert_signed(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    from: Address,
    nonce: u64,
    signed: &SignedTx,
    now: SystemTime,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO signed_txs (request_id, from_address, nonce, raw, tx_hash, signed_at)
         VALUES ($1, $2, $3, $4, $5, timestamptz 'epoch' + $6 * interval '1 microsecond')",
    )
    .bind(id)
    .bind(from.to_string())
    .bind(db_i64(nonce)?)
    .bind(&signed.raw)
    .bind(signed.hash.to_string())
    .bind(micros(now)?)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn load_signed(conn: &mut PgConnection, id: &str) -> Result<Option<StoredTx>> {
    let row: Option<(String, i64, Vec<u8>, String, i64)> = sqlx::query_as(
        "SELECT from_address, nonce, raw, tx_hash,
                (extract(epoch FROM signed_at) * 1000000)::bigint
           FROM signed_txs WHERE request_id = $1",
    )
    .bind(id)
    .fetch_optional(conn)
    .await?;
    let Some((from, nonce, raw, hash, at)) = row else {
        return Ok(None);
    };
    let bad = |what: &str| WithdrawalError::Corrupt(format!("signed_txs {id}: {what}"));
    Ok(Some(StoredTx {
        from: Address::parse(&from).ok_or_else(|| bad("from_address"))?,
        nonce: u64::try_from(nonce).map_err(|_| bad("nonce"))?,
        raw,
        hash: Hash32::parse(&hash).ok_or_else(|| bad("tx_hash"))?,
        signed_at: UNIX_EPOCH
            + Duration::from_micros(u64::try_from(at).map_err(|_| bad("signed_at"))?),
    }))
}

/// 영수증의 블록이 스캐너가 저장한 canonical 헤더(고아 아님)인가.
async fn is_stored_canonical(conn: &mut PgConnection, r: &Receipt) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM block_headers
                         WHERE hash = $1 AND number = $2 AND NOT orphaned)",
    )
    .bind(r.block_hash.to_string())
    .bind(db_i64(r.block_number.0)?)
    .fetch_one(conn)
    .await?)
}

#[derive(sqlx::FromRow)]
struct WithdrawalRow {
    request_id: String,
    user_id: i64,
    to_address: String,
    amount: String,
    status: String,
}

impl WithdrawalRow {
    fn decode(self) -> Result<Withdrawal> {
        let bad = |what: &str| WithdrawalError::Corrupt(format!("withdrawal {what}"));
        Ok(Withdrawal {
            to: Address::parse(&self.to_address).ok_or_else(|| bad("to_address"))?,
            amount: self
                .amount
                .parse::<u128>()
                .ok()
                .and_then(Amount::new)
                .ok_or_else(|| bad("amount"))?,
            status: Status::parse(&self.status).ok_or_else(|| bad("status"))?,
            request_id: self.request_id,
            user: self.user_id,
        })
    }
}

fn micros(t: SystemTime) -> Result<i64, sqlx::Error> {
    let d = t
        .duration_since(UNIX_EPOCH)
        .map_err(|e| sqlx::Error::Encode(e.to_string().into()))?;
    i64::try_from(d.as_micros()).map_err(|e| sqlx::Error::Encode(e.to_string().into()))
}

fn db_i64(n: u64) -> Result<i64, sqlx::Error> {
    i64::try_from(n).map_err(|_| sqlx::Error::Encode(format!("{n} too large").into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::RpcError;

    fn definite(m: &str) -> RpcOutcome<Hash32> {
        RpcOutcome::Definite(RpcError(m.into()))
    }

    #[test]
    fn no_send_outcome_maps_to_failed() {
        let ok = RpcOutcome::Ok(Hash32::ZERO);
        assert_eq!(status_after_send(&ok), Status::Sent);
        for m in [
            "already known",
            "nonce too low",
            "replacement transaction underpriced",
        ] {
            assert_eq!(status_after_send(&definite(m)), Status::Sent, "{m}");
        }
        assert_eq!(
            status_after_send(&definite("insufficient funds")),
            Status::Unknown
        );
        assert_eq!(
            status_after_send(&RpcOutcome::Indeterminate("timeout".into())),
            Status::Unknown
        );
        assert!(resend_accepted(&ok));
        assert!(resend_accepted(&definite("already known")));
        assert!(!resend_accepted(&definite("nonce too low")));
        assert!(!resend_accepted(&RpcOutcome::Indeterminate("t".into())));
    }

    #[test]
    fn status_round_trip() {
        for s in Status::ALL {
            assert_eq!(Status::parse(s.as_str()), Some(s));
        }
        assert!(Status::Failed.is_final() && !Status::NeedsReview.is_final());
    }
}
