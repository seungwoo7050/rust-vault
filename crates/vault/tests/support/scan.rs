//! 입금 스캐너 테스트가 함께 쓰는 환경: 새 테스트 DB, 가짜 체인, 장애 계획, 등록된 입금 주소.
//! 단언용 읽기(분개 수, 커서, 헤더)는 여기서 직접 SQL로 한다.
#![allow(dead_code)]

use std::collections::BTreeMap;

use sqlx::PgConnection;
use vault::chain::{Address, Block, BlockHeader, Hash32};
use vault::deposit::{self, Cursor, ScanConfig, Scanner};
use vault::ledger::{self, AccountKey};
use vault_sim::chain::{self as sim, FakeChain};
use vault_sim::db::TestDb;
use vault_sim::faults::FaultPlan;

/// 테스트의 확정 수.
pub const N: u64 = 3;
pub const CHAIN_ID: u64 = 1;
/// 입금 주소를 등록하는 사용자. 사용자 `u`의 주소는 `addr(u)`다
pub const USERS: [i64; 2] = [1, 2];

pub fn addr(user: i64) -> Address {
    sim::address(user as u64)
}

/// 등록하지 않은 주소(입금이 아니다).
pub fn stranger() -> Address {
    sim::address(999)
}

pub struct Env {
    pub db: TestDb,
    pub chain: FakeChain,
    pub faults: FaultPlan,
}

impl Env {
    pub async fn new() -> Self {
        let db = TestDb::create().await;
        let mut conn = db.pool.acquire().await.unwrap();
        for u in USERS {
            deposit::register_address(&mut conn, addr(u), u)
                .await
                .expect("입금 주소 등록");
        }
        drop(conn);
        Self {
            db,
            chain: FakeChain::new(),
            faults: FaultPlan::new(),
        }
    }

    pub fn config(&self) -> ScanConfig {
        ScanConfig {
            chain_id: CHAIN_ID,
            confirmations: N,
            start: self.chain.genesis(),
        }
    }

    /// 스캐너를 새로 만든다(재시작). 상태는 DB에서 이어 간다.
    pub async fn scanner(&self) -> Scanner<FakeChain, FaultPlan> {
        Scanner::with_faults(
            self.db.pool.clone(),
            self.chain.clone(),
            self.config(),
            self.faults.clone(),
        )
        .await
        .expect("스캐너 생성")
    }

    pub async fn conn(&self) -> sqlx::pool::PoolConnection<sqlx::Postgres> {
        self.db.pool.acquire().await.unwrap()
    }

    pub async fn balance(&self, key: &AccountKey) -> i128 {
        ledger::balance(&mut *self.conn().await, key).await.unwrap()
    }

    pub async fn cursor(&self) -> Cursor {
        deposit::cursor(&mut *self.conn().await)
            .await
            .unwrap()
            .expect("커서")
    }

    /// 이 접두사로 시작하는 멱등 키의 분개 수.
    pub async fn entries(&self, prefix: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM journal_entries WHERE idempotency_key LIKE $1 || '%'",
        )
        .bind(prefix)
        .fetch_one(&mut *self.conn().await)
        .await
        .unwrap()
    }

    /// 모든 분개의 멱등 키(정렬).
    pub async fn keys(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT idempotency_key FROM journal_entries ORDER BY idempotency_key")
            .fetch_all(&mut *self.conn().await)
            .await
            .unwrap()
    }

    /// 열린 모든 계정의 정상 방향 잔액.
    pub async fn balances(&self) -> BTreeMap<String, i128> {
        all_balances(&mut *self.conn().await).await
    }

    pub async fn trial_balance(&self) -> i128 {
        ledger::trial_balance(&mut *self.conn().await)
            .await
            .unwrap()
    }

    /// 저장된 헤더 전부: (번호, 해시, 부모 해시, 고아)를 번호·해시 순으로.
    pub async fn headers(&self) -> Vec<(i64, String, String, bool)> {
        sqlx::query_as(
            "SELECT number, hash, parent_hash, orphaned FROM block_headers ORDER BY number, hash",
        )
        .fetch_all(&mut *self.conn().await)
        .await
        .unwrap()
    }

    pub async fn violations(&self) -> Vec<(String, String, String, String)> {
        sqlx::query_as(
            "SELECT kind, block_hash, tx_hash, amount::text FROM finality_violations ORDER BY id",
        )
        .fetch_all(&mut *self.conn().await)
        .await
        .unwrap()
    }

    pub async fn halted(&self) -> Option<String> {
        deposit::halted(&mut *self.conn().await).await.unwrap()
    }

    /// 저장된 canonical 헤더(고아 아님)가 시작 블록부터 커서까지 번호마다 하나씩, 부모 해시로 이어진 한 줄인지 본다.
    pub async fn check_stored_chain(&self) -> Result<(), String> {
        let rows = self.headers().await;
        let live: Vec<_> = rows.iter().filter(|r| !r.3).collect();
        let cursor = self.cursor().await;
        if live.len() as u64 != cursor.number.0 + 1 {
            return Err(format!(
                "canonical 헤더 {}개, 커서 번호 {}",
                live.len(),
                cursor.number
            ));
        }
        for (i, r) in live.iter().enumerate() {
            if r.0 != i as i64 {
                return Err(format!("{i}번 자리에 번호 {} 헤더", r.0));
            }
            if i > 0 && r.2 != live[i - 1].1 {
                return Err(format!(
                    "{}번 헤더의 부모 해시가 {}번 해시와 다르다",
                    r.0,
                    i - 1
                ));
            }
        }
        if live.last().unwrap().1 != cursor.hash.to_string() {
            return Err("커서 해시가 마지막 canonical 헤더와 다르다".into());
        }
        Ok(())
    }
}

pub async fn all_balances(conn: &mut PgConnection) -> BTreeMap<String, i128> {
    let keys: Vec<String> = sqlx::query_scalar("SELECT key FROM accounts ORDER BY key")
        .fetch_all(&mut *conn)
        .await
        .unwrap();
    let mut out = BTreeMap::new();
    for k in keys {
        let key = AccountKey::parse(&k).unwrap();
        out.insert(k, ledger::balance(conn, &key).await.unwrap());
    }
    out
}

/// 체인 블록들에서 `to`로 온 입금(성공, value > 0)의 합.
pub fn deposits_to(blocks: &[Block], to: Address) -> i128 {
    blocks
        .iter()
        .flat_map(|b| &b.txs)
        .filter(|t| t.success && t.value > 0 && t.to == to)
        .map(|t| t.value as i128)
        .sum()
}

/// `parent` 위에 빈 블록을 `n`개 캐서 canonical로 삼고 마지막 해시를 돌려준다.
pub fn extend(chain: &FakeChain, mut parent: Hash32, n: usize) -> Hash32 {
    for _ in 0..n {
        parent = chain.mine_canonical(parent, vec![]);
    }
    parent
}

pub fn hash_of(h: &BlockHeader) -> String {
    h.hash.to_string()
}
