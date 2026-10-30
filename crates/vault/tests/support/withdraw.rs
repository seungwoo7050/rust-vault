//! 출금 테스트가 함께 쓰는 환경: 입금 스캐너 환경(`scan::Env`) + 가짜 시계 + 테스트 키 처리기.
//! 사용자 잔액은 입금 경로(가짜 체인 입금 → 스캔 → 확정)로 채우고, 출금 주소는 운영 자금 입금
//! (`cap:fund`, ADR-001)으로 채운다: 가짜 체인에 자금 tx를 넣고 같은 분개를 직접 기록한다.
//! 단언용 읽기(행 수, 분개 수, 체인 지급)는 여기서 한다.
#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use vault::chain::{Address, ChainTx};
use vault::ledger::{self, AccountKey, Line, NewEntry};
use vault::signer::{LocalSigner, Signer};
use vault::withdrawal::{
    self, Processor, Status, StoredTx, Withdrawal, WithdrawalConfig, WithdrawalError,
    WithdrawalRequest,
};
use vault_sim::chain::{self as sim, FakeChain};
use vault_sim::clock::FakeClock;
use vault_sim::faults::FaultPlan;
use vault_sim::keys::test_signer;

use crate::model;
use crate::scan::{self, Env};

pub type Proc = Processor<FakeChain, LocalSigner, FakeClock, FaultPlan>;

/// `NeedsReview` 기한.
pub const REVIEW_AFTER: Duration = Duration::from_secs(600);
pub const MAX_FEE: u128 = 100;
pub const PRIORITY_FEE: u128 = 2;
/// 가짜 체인에서 전송 하나의 가스비: 21000 × min(최대 수수료, base fee + 우선 수수료).
pub const FEE: i128 = 21_000 * 9;
/// 출금 주소에 넣어 두는 운영 자금.
pub const CAPITAL: u128 = 1_000_000_000;

pub fn config() -> WithdrawalConfig {
    WithdrawalConfig {
        chain_id: scan::CHAIN_ID,
        confirmations: scan::N,
        max_fee_per_gas: MAX_FEE,
        max_priority_fee_per_gas: PRIORITY_FEE,
        review_after: REVIEW_AFTER,
        transient_retries: scan::RETRIES,
    }
}

/// 출금 주소(테스트 키의 주소).
pub fn hot() -> Address {
    test_signer().address()
}

/// 출금 수취인. 요청마다 다른 주소를 써서 체인 지급을 요청별로 센다.
pub fn recipient(n: u64) -> Address {
    sim::address(10_000 + n)
}

pub struct WEnv {
    pub env: Env,
    pub clock: FakeClock,
    next_tx: AtomicU64,
}

impl WEnv {
    /// 새 DB와 체인. 출금 주소에 운영 자금을 넣고 스캔해 둔다.
    pub async fn new() -> Self {
        let w = Self::from_env(Env::new().await);
        w.fund_operator(CAPITAL).await;
        w.scan().await;
        w
    }

    pub fn from_env(env: Env) -> Self {
        Self {
            env,
            clock: FakeClock::new(),
            next_tx: AtomicU64::new(1_000_000),
        }
    }

    pub fn chain(&self) -> &FakeChain {
        &self.env.chain
    }

    pub fn pool(&self) -> &sqlx::PgPool {
        &self.env.db.pool
    }

    /// 처리기를 새로 만든다(재시작). 상태는 DB에서 이어 간다.
    pub async fn processor(&self) -> Proc {
        Processor::new(
            self.pool().clone(),
            self.env.chain.clone(),
            test_signer(),
            self.clock.clone(),
            self.env.faults.clone(),
            config(),
        )
        .await
        .expect("처리기 생성")
    }

    /// 운영 자금 입금: 가짜 체인에 자금 tx를 넣고 `cap:fund:{chain_id}:{tx_hash}` 분개를 직접 기록한다.
    pub async fn fund_operator(&self, amount: u128) {
        let t = sim::transfer(self.tx_id(), hot(), amount);
        self.env
            .chain
            .mine_canonical(self.env.chain.tip().hash, vec![t]);
        let mut conn = self.env.conn().await;
        ledger::open_account(&mut conn, &AccountKey::hot_wallet(hot()))
            .await
            .unwrap();
        ledger::open_account(&mut conn, &AccountKey::operator_capital())
            .await
            .unwrap();
        drop(conn);
        let mut tx = self.pool().begin().await.unwrap();
        ledger::post(
            &mut tx,
            NewEntry {
                idempotency_key: format!("cap:fund:{}:{}", scan::CHAIN_ID, t.hash),
                kind: "cap:fund".into(),
                lines: vec![
                    Line::debit(AccountKey::hot_wallet(hot()), model::amt(amount)),
                    Line::credit(AccountKey::operator_capital(), model::amt(amount)),
                ],
            },
        )
        .await
        .expect("운영 자금 분개");
        tx.commit().await.unwrap();
    }

    /// 사용자 `u`의 입금 주소로 입금하고 확정될 때까지 캐고 스캔한다.
    pub async fn deposit(&self, user: i64, amount: u128) {
        let t = sim::transfer(self.tx_id(), scan::addr(user), amount);
        let b = self
            .env
            .chain
            .mine_canonical(self.env.chain.tip().hash, vec![t]);
        scan::extend(&self.env.chain, b, (scan::N - 1) as usize);
        self.scan().await;
    }

    fn tx_id(&self) -> u64 {
        self.next_tx.fetch_add(1, Ordering::Relaxed)
    }

    /// 스캐너를 새로 만들어 head까지 스캔한다.
    pub async fn scan(&self) {
        self.env
            .scanner()
            .await
            .run_until_idle()
            .await
            .expect("스캔");
    }

    /// canonical tip 위로 블록을 `n`개 캐고(멤풀의 tx가 들어간다) 스캔한다.
    pub async fn mine(&self, n: usize) {
        scan::extend(&self.env.chain, self.env.chain.tip().hash, n);
        self.scan().await;
    }

    /// 블록 하나 캐고 스캔하고 처리하기를 `rounds`번.
    pub async fn settle(&self, p: &mut Proc, rounds: usize) {
        for _ in 0..rounds {
            p.run_until_idle().await.expect("처리");
            self.mine(1).await;
        }
        p.run_until_idle().await.expect("처리");
    }

    pub async fn request(&self, id: &str, user: i64, to: Address, amount: u128) -> Withdrawal {
        withdrawal::request(
            self.pool(),
            WithdrawalRequest {
                request_id: id.into(),
                user,
                to,
                amount: model::amt(amount),
            },
        )
        .await
        .expect("출금 요청")
    }

    pub async fn status(&self, id: &str) -> Status {
        withdrawal::get(self.pool(), id)
            .await
            .unwrap()
            .expect("출금 행")
            .status
    }

    pub async fn stored(&self, id: &str) -> StoredTx {
        withdrawal::signed_tx(self.pool(), id)
            .await
            .unwrap()
            .expect("서명 tx")
    }

    pub async fn balance(&self, key: &AccountKey) -> i128 {
        self.env.balance(key).await
    }

    pub async fn entries(&self, prefix: &str) -> i64 {
        self.env.entries(prefix).await
    }

    /// 출금 행 수(이 요청 ID).
    pub async fn rows(&self, id: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM withdrawals WHERE request_id = $1")
            .bind(id)
            .fetch_one(self.pool())
            .await
            .unwrap()
    }

    /// 서명 tx 행 수(전체).
    pub async fn signed_rows(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM signed_txs")
            .fetch_one(self.pool())
            .await
            .unwrap()
    }

    /// 저장된 서명 tx의 raw 바이트 전부.
    pub async fn stored_raws(&self) -> Vec<Vec<u8>> {
        sqlx::query_scalar("SELECT raw FROM signed_txs ORDER BY nonce")
            .fetch_all(self.pool())
            .await
            .unwrap()
    }

    /// canonical chain에서 `to`에게 간 성공한 tx(체인 지급).
    pub fn payments_to(&self, to: Address) -> Vec<ChainTx> {
        self.env
            .chain
            .canonical_blocks()
            .iter()
            .flat_map(|b| b.txs.clone())
            .filter(|t| t.success && t.to == to)
            .collect()
    }

    /// 지급 분개(`wd:settle:*`)의 출금액 합(`withdrawal_pending` 차변 합).
    pub async fn settled_amount(&self) -> i128 {
        let s: String = sqlx::query_scalar(
            "SELECT coalesce(sum(l.amount), 0)::text
               FROM journal_lines l JOIN journal_entries e ON e.id = l.entry_id
              WHERE e.idempotency_key LIKE 'wd:settle:%'
                AND l.account_key LIKE 'withdrawal_pending:%'",
        )
        .fetch_one(self.pool())
        .await
        .unwrap();
        s.parse().unwrap()
    }
}

/// 처리기를 돌리다 주입된 종료가 나면 새로 만들어(재시작) 이어 간다. 재시작 횟수를 돌려준다.
pub async fn run_restarting(w: &WEnv, p: &mut Proc) -> Result<usize, WithdrawalError> {
    let mut restarts = 0;
    loop {
        match p.run_until_idle().await {
            Ok(()) => return Ok(restarts),
            Err(WithdrawalError::Crashed(_)) => {
                restarts += 1;
                *p = w.processor().await;
            }
            Err(e) => return Err(e),
        }
    }
}
