//! 온체인 잔액·영수증·블록과 원장·상태 테이블의 사후 대사(R9).
//!
//! `reconcile`은 데이터베이스를 바꾸지 않는 읽기 전용 함수다. 같은 입력에 대해 몇 번을 호출해도
//! 똑같은 `ReconReport`를 돌려준다(멱등).
//!
//! 점검하는 네 범주:
//! 1. `HotWalletBalance`: 사용자 입금 주소(`cursor` 높이 기준)와 출금 핫월렛 주소(`confirmed_block` 높이 기준)의
//!    온체인 잔액과 원장 `hot_wallet:*` 차변 잔액 대조
//! 2. `LedgerEquation` · `NegativeAccount`: 자산(`hot_wallet:*` 합) = 부채·자본(`user:*` + `pending_deposit:*` +
//!    `withdrawal_pending:*` + `operator_capital` 합) − 비용(`network_fee` 합) 등식 및 모든 계정의 정상 방향 음수 여부
//! 3. `DepositMismatch`: `deposits` 테이블 상태(`pending`·`confirmed`·`reverted`)와 원장 `dep:*` 분개 및
//!    스캐너 커서 이하 canonical 블록의 온체인 입금 tx 대조
//! 4. `WithdrawalMismatch`: `withdrawals` 상태(`Held`·`Signed`·`Sent`·`Unknown`·`NeedsReview`·`Confirmed`·`Failed`·`Rejected`)와
//!    `signed_txs`, 원장 `wd:*` 분개, 온체인 영수증(`receipt`) 대조

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use sqlx::{PgConnection, PgPool};

use crate::chain::{
    Address, Block, BlockHeader, BlockNumber, ChainClient, Hash32, RpcError, RpcOutcome,
};
use crate::ledger::{self, AccountKey, AccountKind};
use crate::signer;
use crate::withdrawal::Status;

/// 대사 설정.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconConfig {
    pub chain_id: u64,
    /// 스캐너·출금 처리기와 같은 확정 수 N
    pub confirmations: u64,
    /// 출금 핫월렛 주소
    pub hot_wallet: Option<Address>,
}

/// 대사 결과 리포트.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconReport {
    /// 스캐너 커서 헤더(스캐너를 아직 돌리지 않았으면 `None`)
    pub cursor: Option<BlockHeader>,
    /// 확정 높이(`cursor.number - N + 1`, 포화 뺄셈)
    pub confirmed_block: Option<BlockNumber>,
    /// 발견된 불일치 목록
    pub discrepancies: Vec<Discrepancy>,
}

impl ReconReport {
    /// 불일치가 하나도 없는가.
    pub fn is_clean(&self) -> bool {
        self.discrepancies.is_empty()
    }
}

/// 대사에서 발견된 불일치.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discrepancy {
    /// 온체인 잔액과 원장 `hot_wallet:{address}` 차변 잔액이 다르다
    HotWalletBalance {
        address: Address,
        at_block: BlockNumber,
        on_chain: u128,
        ledger: i128,
    },
    /// 원장 자산 합(`hot_wallet:*`)과 청구권 합(`user:*` + `pending_deposit:*` + `withdrawal_pending:*` + `operator_capital` − `network_fee`)이 다르다
    LedgerEquation { assets: i128, claims: i128 },
    /// 정상 잔액 방향 기준으로 음수인 계정이 있다
    NegativeAccount { account: AccountKey, balance: i128 },
    /// 입금 행·분개·온체인 tx 사이에 불일치가 있다
    DepositMismatch {
        tx_hash: Hash32,
        block_hash: Hash32,
        reason: String,
    },
    /// 출금 행·서명 tx·분개·온체인 영수증 사이에 불일치가 있다
    WithdrawalMismatch {
        request_id: String,
        tx_hash: Option<Hash32>,
        reason: String,
    },
}

/// 대사 실행 중 오류.
#[derive(Debug)]
pub enum ReconError {
    /// 체인 조회 결과를 모른다
    Indeterminate(String),
    /// 체인이 오류를 돌려줬다
    Rpc(RpcError),
    /// DB에 저장된 값을 해석할 수 없다
    Corrupt(String),
    /// DB 조회 오류
    Db(sqlx::Error),
}

impl fmt::Display for ReconError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReconError::Indeterminate(m) => write!(f, "chain read indeterminate: {m}"),
            ReconError::Rpc(e) => write!(f, "chain read failed: {e}"),
            ReconError::Corrupt(m) => write!(f, "corrupt row: {m}"),
            ReconError::Db(e) => write!(f, "database: {e}"),
        }
    }
}

impl std::error::Error for ReconError {}

impl From<sqlx::Error> for ReconError {
    fn from(e: sqlx::Error) -> Self {
        ReconError::Db(e)
    }
}

fn rpc<T>(outcome: RpcOutcome<T>) -> Result<T, ReconError> {
    match outcome {
        RpcOutcome::Ok(v) => Ok(v),
        RpcOutcome::Definite(e) => Err(ReconError::Rpc(e)),
        RpcOutcome::Indeterminate(m) => Err(ReconError::Indeterminate(m)),
    }
}

/// 온체인 상태와 DB 원장·입출금 테이블을 읽기 전용으로 대사한다.
pub async fn reconcile<C: ChainClient>(
    pool: &PgPool,
    chain: &C,
    config: &ReconConfig,
) -> Result<ReconReport, ReconError> {
    let mut conn = pool.acquire().await?;
    let cursor = load_cursor_header(&mut conn).await?;
    let confirmed_block =
        cursor.map(|c| BlockNumber((c.number.0 + 1).saturating_sub(config.confirmations)));

    let mut discrepancies = Vec::new();
    check_ledger(&mut conn, &mut discrepancies).await?;
    if let (Some(cur), Some(conf)) = (cursor, confirmed_block) {
        check_hot_wallets(
            &mut conn,
            chain,
            config,
            cur.number,
            conf,
            &mut discrepancies,
        )
        .await?;
    }
    check_deposits(
        &mut conn,
        chain,
        config,
        cursor,
        confirmed_block,
        &mut discrepancies,
    )
    .await?;
    check_withdrawals(
        &mut conn,
        chain,
        config,
        confirmed_block,
        &mut discrepancies,
    )
    .await?;

    Ok(ReconReport {
        cursor,
        confirmed_block,
        discrepancies,
    })
}

async fn load_cursor_header(conn: &mut PgConnection) -> Result<Option<BlockHeader>, ReconError> {
    let row: Option<(i64, String, String)> = sqlx::query_as(
        "SELECT b.number, b.hash, b.parent_hash
           FROM scan_cursor c
           JOIN block_headers b ON b.hash = c.hash",
    )
    .fetch_optional(conn)
    .await?;
    let Some((num, hash, parent)) = row else {
        return Ok(None);
    };
    let number =
        u64::try_from(num).map_err(|_| ReconError::Corrupt(format!("cursor number {num}")))?;
    Ok(Some(BlockHeader {
        number: BlockNumber(number),
        hash: parse_hash(&hash, "cursor hash")?,
        parent_hash: parse_hash(&parent, "cursor parent_hash")?,
    }))
}

/// (범주 2) 원장 내부 등식(`hot_wallet` 자산 = 부채·자본 − 비용)과 정상 방향 음수 잔액 점검.
async fn check_ledger(
    conn: &mut PgConnection,
    out: &mut Vec<Discrepancy>,
) -> Result<(), ReconError> {
    let keys: Vec<String> = sqlx::query_scalar("SELECT key FROM accounts ORDER BY key")
        .fetch_all(&mut *conn)
        .await?;
    let mut assets: i128 = 0;
    let mut liabilities_and_equity: i128 = 0;
    let mut fee: i128 = 0;

    for k in keys {
        let key = AccountKey::parse(&k).map_err(|e| ReconError::Corrupt(e.to_string()))?;
        let bal = ledger::balance(&mut *conn, &key).await?;
        if bal < 0 {
            out.push(Discrepancy::NegativeAccount {
                account: key.clone(),
                balance: bal,
            });
        }
        match key.kind() {
            AccountKind::HotWallet => assets += bal,
            AccountKind::User
            | AccountKind::PendingDeposit
            | AccountKind::WithdrawalPending
            | AccountKind::OperatorCapital => liabilities_and_equity += bal,
            AccountKind::NetworkFee => fee += bal,
        }
    }

    let claims = liabilities_and_equity - fee;
    let trial = ledger::trial_balance(&mut *conn).await?;
    if assets != claims || trial != 0 {
        out.push(Discrepancy::LedgerEquation {
            assets,
            claims: if assets != claims {
                claims
            } else {
                assets + trial
            },
        });
    }
    Ok(())
}

/// (범주 1) 입금 주소(`cursor` 기준)와 출금 핫월렛(`confirmed_block` 기준)의 온체인 잔액 대 원장 잔액 비교.
async fn check_hot_wallets<C: ChainClient>(
    conn: &mut PgConnection,
    chain: &C,
    config: &ReconConfig,
    cursor_block: BlockNumber,
    confirmed_block: BlockNumber,
    out: &mut Vec<Discrepancy>,
) -> Result<(), ReconError> {
    let dep_addrs: Vec<String> =
        sqlx::query_scalar("SELECT address FROM deposit_addresses ORDER BY address")
            .fetch_all(&mut *conn)
            .await?;
    let mut checked = BTreeSet::new();

    for addr_str in dep_addrs {
        let addr = parse_addr(&addr_str, "deposit_addresses.address")?;
        if Some(addr) == config.hot_wallet {
            continue;
        }
        checked.insert(addr);
        let on_chain = rpc(chain.balance_at(addr, cursor_block).await)?;
        let ledger_bal = ledger::balance(&mut *conn, &AccountKey::hot_wallet(addr)).await?;
        if on_chain as i128 != ledger_bal {
            out.push(Discrepancy::HotWalletBalance {
                address: addr,
                at_block: cursor_block,
                on_chain,
                ledger: ledger_bal,
            });
        }
    }

    let mut hw_addrs = BTreeSet::new();
    if let Some(hw) = config.hot_wallet {
        hw_addrs.insert(hw);
    }
    let hw_keys: Vec<String> =
        sqlx::query_scalar("SELECT key FROM accounts WHERE kind = 'hot_wallet' ORDER BY key")
            .fetch_all(&mut *conn)
            .await?;
    for k in hw_keys {
        if let Some(rest) = k.strip_prefix("hot_wallet:")
            && let Some(addr) = Address::parse(rest)
            && !checked.contains(&addr)
        {
            hw_addrs.insert(addr);
        }
    }

    for hw in hw_addrs {
        let on_chain = rpc(chain.balance_at(hw, confirmed_block).await)?;
        let ledger_bal = ledger::balance(&mut *conn, &AccountKey::hot_wallet(hw)).await?;
        let inflight =
            unconcluded_on_chain_deduction(&mut *conn, chain, hw, confirmed_block).await?;
        if (on_chain.saturating_add(inflight)) as i128 != ledger_bal {
            out.push(Discrepancy::HotWalletBalance {
                address: hw,
                at_block: confirmed_block,
                on_chain,
                ledger: ledger_bal,
            });
        }
    }
    Ok(())
}

/// `confirmed_block` 이하에서 이미 채굴됐지만 아직 `Processor`가 결론 내지 않은 출금의 온체인 차감액.
async fn unconcluded_on_chain_deduction<C: ChainClient>(
    conn: &mut PgConnection,
    chain: &C,
    from: Address,
    confirmed_block: BlockNumber,
) -> Result<u128, ReconError> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT s.tx_hash, w.amount::text
           FROM withdrawals w
           JOIN signed_txs s ON s.request_id = w.request_id
          WHERE s.from_address = $1
            AND w.status IN ('sent', 'unknown', 'needs_review')",
    )
    .bind(from.to_string())
    .fetch_all(&mut *conn)
    .await?;
    let mut total: u128 = 0;
    for (tx_str, amt_str) in rows {
        let tx_hash = parse_hash(&tx_str, "signed_txs.tx_hash")?;
        let amount = parse_u128(&amt_str, "withdrawals.amount")?;
        if let Some(r) = rpc(chain.receipt(tx_hash).await)?
            && r.block_number <= confirmed_block
        {
            let spent = if r.success { amount + r.fee() } else { r.fee() };
            total = total.saturating_add(spent);
        }
    }
    Ok(total)
}

#[derive(Debug, Clone)]
struct EntryData {
    kind: String,
    lines: Vec<(String, i128)>,
}

async fn load_entries(
    conn: &mut PgConnection,
    prefix: &str,
) -> Result<BTreeMap<String, EntryData>, ReconError> {
    let entries: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT id, idempotency_key, kind FROM journal_entries
          WHERE idempotency_key LIKE $1 || '%'
          ORDER BY id",
    )
    .bind(prefix)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = BTreeMap::new();
    for (id, key, kind) in entries {
        let raw_lines: Vec<(String, String)> = sqlx::query_as(
            "SELECT account_key, amount::text FROM journal_lines
              WHERE entry_id = $1 ORDER BY account_key",
        )
        .bind(id)
        .fetch_all(&mut *conn)
        .await?;
        let mut lines = Vec::with_capacity(raw_lines.len());
        for (acc, amt) in raw_lines {
            let signed = amt
                .parse::<i128>()
                .map_err(|_| ReconError::Corrupt(format!("journal_lines.amount {amt:?}")))?;
            lines.push((acc, signed));
        }
        out.insert(key, EntryData { kind, lines });
    }
    Ok(out)
}

fn matches_two_lines(
    entry: &EntryData,
    expected_kind: &str,
    debit_acc: &AccountKey,
    credit_acc: &AccountKey,
    amount: u128,
) -> bool {
    if entry.kind != expected_kind || entry.lines.len() != 2 {
        return false;
    }
    let amt = amount as i128;
    let mut want = vec![
        (debit_acc.as_str().to_owned(), amt),
        (credit_acc.as_str().to_owned(), -amt),
    ];
    want.sort();
    entry.lines == want
}

#[derive(sqlx::FromRow)]
struct DepositDbRow {
    tx_hash: String,
    block_hash: String,
    address: String,
    user_id: i64,
    amount: String,
    status: String,
}

/// (범주 3) `deposits` 테이블, `dep:*` 분개, canonical 온체인 블록의 입금 tx 대조.
async fn check_deposits<C: ChainClient>(
    conn: &mut PgConnection,
    chain: &C,
    config: &ReconConfig,
    cursor: Option<BlockHeader>,
    confirmed_block: Option<BlockNumber>,
    out: &mut Vec<Discrepancy>,
) -> Result<(), ReconError> {
    let dep_addr_rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT address, user_id FROM deposit_addresses")
            .fetch_all(&mut *conn)
            .await?;
    let mut dep_addrs = BTreeMap::new();
    for (a, u) in dep_addr_rows {
        dep_addrs.insert(parse_addr(&a, "deposit_addresses.address")?, u);
    }

    let header_rows: Vec<(String, i64, bool)> =
        sqlx::query_as("SELECT hash, number, orphaned FROM block_headers")
            .fetch_all(&mut *conn)
            .await?;
    let mut headers = BTreeMap::new();
    for (h, n, orphaned) in header_rows {
        let num = u64::try_from(n).map_err(|_| ReconError::Corrupt(format!("block number {n}")))?;
        headers.insert(
            parse_hash(&h, "block_headers.hash")?,
            (BlockNumber(num), orphaned),
        );
    }

    let entries = load_entries(&mut *conn, "dep:").await?;
    let rows: Vec<DepositDbRow> = sqlx::query_as(
        "SELECT tx_hash, block_hash, address, user_id, amount::text, status
           FROM deposits ORDER BY tx_hash, block_hash",
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut deposit_map = BTreeMap::new();
    let mut confirmed_txs = BTreeSet::new();
    let mut reported = BTreeSet::new();

    for r in rows {
        let tx_hash = parse_hash(&r.tx_hash, "deposits.tx_hash")?;
        let block_hash = parse_hash(&r.block_hash, "deposits.block_hash")?;
        let address = parse_addr(&r.address, "deposits.address")?;
        let amount = parse_u128(&r.amount, "deposits.amount")?;
        deposit_map.insert(
            (tx_hash, block_hash),
            (address, r.user_id, amount, r.status.clone()),
        );
        if r.status == "confirmed" {
            confirmed_txs.insert(tx_hash);
        }

        let pending_key = format!("dep:pending:{}:{tx_hash}:{block_hash}", config.chain_id);
        let revert_key = format!("dep:revert:{}:{tx_hash}:{block_hash}", config.chain_id);
        let confirm_key = format!("dep:confirm:{}:{tx_hash}", config.chain_id);

        let hw = AccountKey::hot_wallet(address);
        let pd = AccountKey::pending_deposit(r.user_id);
        let user = AccountKey::user(r.user_id);

        let has_valid_pending = entries
            .get(&pending_key)
            .is_some_and(|e| matches_two_lines(e, "dep:pending", &hw, &pd, amount));
        let has_revert = entries.contains_key(&revert_key);
        let has_valid_revert = entries
            .get(&revert_key)
            .is_some_and(|e| matches_two_lines(e, "dep:revert", &pd, &hw, amount));
        let has_confirm = entries.contains_key(&confirm_key);
        let has_valid_confirm = entries
            .get(&confirm_key)
            .is_some_and(|e| matches_two_lines(e, "dep:confirm", &pd, &user, amount));

        let header_info = headers.get(&block_hash).copied();

        let reason = match r.status.as_str() {
            "pending" => {
                if !has_valid_pending {
                    Some("pending deposit missing or mismatched dep:pending entry".to_owned())
                } else if has_revert || has_confirm {
                    Some(
                        "pending deposit has unexpected dep:revert or dep:confirm entry".to_owned(),
                    )
                } else if header_info.is_none_or(|(_, orphaned)| orphaned) {
                    Some("pending deposit is in an orphaned or unknown block".to_owned())
                } else if let (Some((b_num, false)), Some(conf)) = (header_info, confirmed_block)
                    && b_num <= conf
                {
                    Some(format!(
                        "pending deposit at block {b_num} <= confirmed block {conf} is not confirmed"
                    ))
                } else {
                    None
                }
            }
            "confirmed" => {
                if !has_valid_pending {
                    Some("confirmed deposit missing or mismatched dep:pending entry".to_owned())
                } else if !has_valid_confirm {
                    Some("confirmed deposit missing or mismatched dep:confirm entry".to_owned())
                } else if has_revert {
                    Some("confirmed deposit has unexpected dep:revert entry".to_owned())
                } else if header_info.is_none_or(|(_, orphaned)| orphaned) {
                    Some("confirmed deposit is in an orphaned or unknown block".to_owned())
                } else if let (Some((b_num, false)), Some(conf)) = (header_info, confirmed_block)
                    && b_num > conf
                {
                    Some(format!(
                        "confirmed deposit at block {b_num} > confirmed block {conf}"
                    ))
                } else {
                    None
                }
            }
            "reverted" => {
                if !has_valid_pending {
                    Some("reverted deposit missing or mismatched dep:pending entry".to_owned())
                } else if !has_valid_revert {
                    Some("reverted deposit missing or mismatched dep:revert entry".to_owned())
                } else if header_info.is_none_or(|(_, orphaned)| !orphaned) {
                    Some("reverted deposit block is not marked orphaned".to_owned())
                } else {
                    None
                }
            }
            other => Some(format!("unknown deposit status {other:?}")),
        };

        if let Some(reason) = reason {
            reported.insert((tx_hash, block_hash));
            out.push(Discrepancy::DepositMismatch {
                tx_hash,
                block_hash,
                reason,
            });
        }
    }

    // `deposits` 행이 없는 고아 `dep:*` 분개 점검
    let pending_prefix = format!("dep:pending:{}:", config.chain_id);
    let revert_prefix = format!("dep:revert:{}:", config.chain_id);
    let confirm_prefix = format!("dep:confirm:{}:", config.chain_id);
    for key in entries.keys() {
        if let Some(rest) = key
            .strip_prefix(&pending_prefix)
            .or_else(|| key.strip_prefix(&revert_prefix))
            && let Some((tx_s, bh_s)) = rest.split_once(':')
            && let (Some(tx_hash), Some(block_hash)) = (Hash32::parse(tx_s), Hash32::parse(bh_s))
            && !deposit_map.contains_key(&(tx_hash, block_hash))
            && reported.insert((tx_hash, block_hash))
        {
            out.push(Discrepancy::DepositMismatch {
                tx_hash,
                block_hash,
                reason: format!("journal entry {key} has no deposits row"),
            });
        } else if let Some(tx_s) = key.strip_prefix(&confirm_prefix)
            && let Some(tx_hash) = Hash32::parse(tx_s)
            && !confirmed_txs.contains(&tx_hash)
            && reported.insert((tx_hash, Hash32::ZERO))
        {
            out.push(Discrepancy::DepositMismatch {
                tx_hash,
                block_hash: Hash32::ZERO,
                reason: format!("journal entry {key} has no confirmed deposit row"),
            });
        }
    }

    let Some(cur) = cursor else {
        return Ok(());
    };

    // 커서 이하 canonical 체인 블록들을 읽어 온체인 입금 tx와 `deposits` 행을 양방향 대조한다
    let mut on_chain_deposits = BTreeMap::new();
    for n in 0..=cur.number.0 {
        let Some(block): Option<Block> = rpc(chain.block_by_number(BlockNumber(n)).await)? else {
            out.push(Discrepancy::DepositMismatch {
                tx_hash: Hash32::ZERO,
                block_hash: Hash32::ZERO,
                reason: format!("canonical block {n} missing on chain"),
            });
            continue;
        };
        for t in &block.txs {
            if !t.success || t.value == 0 {
                continue;
            }
            let Some(&owner_id) = dep_addrs.get(&t.to) else {
                continue;
            };
            on_chain_deposits.insert((t.hash, block.header.hash), (t.to, owner_id, t.value));
            match deposit_map.get(&(t.hash, block.header.hash)) {
                None => {
                    reported.insert((t.hash, block.header.hash));
                    out.push(Discrepancy::DepositMismatch {
                        tx_hash: t.hash,
                        block_hash: block.header.hash,
                        reason: "on-chain deposit missing from deposits table".into(),
                    });
                }
                Some((addr, uid, amt, status)) => {
                    if *addr != t.to || *uid != owner_id || *amt != t.value || status == "reverted"
                    {
                        reported.insert((t.hash, block.header.hash));
                        out.push(Discrepancy::DepositMismatch {
                            tx_hash: t.hash,
                            block_hash: block.header.hash,
                            reason: "deposits row does not match on-chain tx".into(),
                        });
                    }
                }
            }
        }
    }

    for (&(tx_hash, block_hash), (_, _, _, status)) in &deposit_map {
        if status != "reverted"
            && !on_chain_deposits.contains_key(&(tx_hash, block_hash))
            && reported.insert((tx_hash, block_hash))
        {
            out.push(Discrepancy::DepositMismatch {
                tx_hash,
                block_hash,
                reason: format!("{status} deposit not found in canonical chain up to cursor"),
            });
        }
    }

    Ok(())
}

#[derive(sqlx::FromRow)]
struct WithdrawalDbRow {
    request_id: String,
    user_id: i64,
    to_address: String,
    amount: String,
    status: String,
    block_hash: Option<String>,
}

#[derive(sqlx::FromRow)]
struct SignedDbRow {
    request_id: String,
    from_address: String,
    nonce: i64,
    raw: Vec<u8>,
    tx_hash: String,
}

/// (범주 4) `withdrawals`, `signed_txs`, 원장 `wd:*` 분개, 온체인 영수증 대조.
async fn check_withdrawals<C: ChainClient>(
    conn: &mut PgConnection,
    chain: &C,
    config: &ReconConfig,
    confirmed_block: Option<BlockNumber>,
    out: &mut Vec<Discrepancy>,
) -> Result<(), ReconError> {
    let entries = load_entries(&mut *conn, "wd:").await?;
    let w_rows: Vec<WithdrawalDbRow> = sqlx::query_as(
        "SELECT request_id, user_id, to_address, amount::text, status, block_hash
           FROM withdrawals ORDER BY request_id",
    )
    .fetch_all(&mut *conn)
    .await?;
    let s_rows: Vec<SignedDbRow> = sqlx::query_as(
        "SELECT request_id, from_address, nonce, raw, tx_hash
           FROM signed_txs ORDER BY request_id",
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut signed_map = BTreeMap::new();
    for s in s_rows {
        let from = parse_addr(&s.from_address, "signed_txs.from_address")?;
        let nonce = u64::try_from(s.nonce)
            .map_err(|_| ReconError::Corrupt(format!("signed_txs.nonce {}", s.nonce)))?;
        let hash = parse_hash(&s.tx_hash, "signed_txs.tx_hash")?;
        signed_map.insert(s.request_id, (from, nonce, s.raw, hash));
    }

    let mut known_requests = BTreeSet::new();

    for w in w_rows {
        known_requests.insert(w.request_id.clone());
        let to = parse_addr(&w.to_address, "withdrawals.to_address")?;
        let amount = parse_u128(&w.amount, "withdrawals.amount")?;
        let Some(status) = Status::ALL.into_iter().find(|s| s.as_str() == w.status) else {
            out.push(Discrepancy::WithdrawalMismatch {
                request_id: w.request_id,
                tx_hash: None,
                reason: format!("unknown withdrawal status {:?}", w.status),
            });
            continue;
        };

        let signed = signed_map.get(&w.request_id);
        let tx_hash = signed.map(|s| s.3);

        let hold_key = format!("wd:hold:{}", w.request_id);
        let settle_key = format!("wd:settle:{}", w.request_id);
        let release_key = format!("wd:release:{}", w.request_id);

        let user_acc = AccountKey::user(w.user_id);
        let wp_acc = AccountKey::withdrawal_pending(w.user_id);

        let has_hold = entries.contains_key(&hold_key);
        let has_valid_hold = entries
            .get(&hold_key)
            .is_some_and(|e| matches_two_lines(e, "wd:hold", &user_acc, &wp_acc, amount));
        let has_settle = entries.contains_key(&settle_key);
        let has_release = entries.contains_key(&release_key);
        let has_valid_release = entries
            .get(&release_key)
            .is_some_and(|e| matches_two_lines(e, "wd:release", &wp_acc, &user_acc, amount));

        if status == Status::Rejected {
            if has_hold || has_settle || has_release || signed.is_some() {
                out.push(Discrepancy::WithdrawalMismatch {
                    request_id: w.request_id,
                    tx_hash,
                    reason: "rejected withdrawal has journal entries or signed tx".into(),
                });
            }
            continue;
        }

        if !has_valid_hold {
            out.push(Discrepancy::WithdrawalMismatch {
                request_id: w.request_id,
                tx_hash,
                reason: "withdrawal missing or mismatched wd:hold entry".into(),
            });
            continue;
        }

        match status {
            Status::Held => {
                if signed.is_some() || has_settle || has_release {
                    out.push(Discrepancy::WithdrawalMismatch {
                        request_id: w.request_id,
                        tx_hash,
                        reason: "held withdrawal has signed tx, wd:settle, or wd:release".into(),
                    });
                }
            }
            Status::Signed | Status::Sent | Status::Unknown | Status::NeedsReview => {
                let Some((from, nonce, raw, hash)) = signed else {
                    out.push(Discrepancy::WithdrawalMismatch {
                        request_id: w.request_id,
                        tx_hash: None,
                        reason: format!("{status} withdrawal missing signed_txs row"),
                    });
                    continue;
                };
                if !raw_matches(raw, *hash, *from, *nonce, to, amount) {
                    out.push(Discrepancy::WithdrawalMismatch {
                        request_id: w.request_id,
                        tx_hash: Some(*hash),
                        reason: "signed_txs raw bytes do not match withdrawal".into(),
                    });
                } else if has_settle || has_release {
                    out.push(Discrepancy::WithdrawalMismatch {
                        request_id: w.request_id,
                        tx_hash: Some(*hash),
                        reason: format!(
                            "{status} withdrawal has premature settle or release entry"
                        ),
                    });
                }
            }
            Status::Confirmed => {
                let Some((from, nonce, raw, hash)) = signed else {
                    out.push(Discrepancy::WithdrawalMismatch {
                        request_id: w.request_id,
                        tx_hash: None,
                        reason: "confirmed withdrawal missing signed_txs row".into(),
                    });
                    continue;
                };
                if !raw_matches(raw, *hash, *from, *nonce, to, amount) {
                    out.push(Discrepancy::WithdrawalMismatch {
                        request_id: w.request_id,
                        tx_hash: Some(*hash),
                        reason: "signed_txs raw bytes do not match withdrawal".into(),
                    });
                    continue;
                }
                if has_release {
                    out.push(Discrepancy::WithdrawalMismatch {
                        request_id: w.request_id,
                        tx_hash: Some(*hash),
                        reason: "confirmed withdrawal has unexpected wd:release entry".into(),
                    });
                    continue;
                }
                let Some(receipt) = rpc(chain.receipt(*hash).await)? else {
                    out.push(Discrepancy::WithdrawalMismatch {
                        request_id: w.request_id,
                        tx_hash: Some(*hash),
                        reason: "confirmed withdrawal has no canonical on-chain receipt".into(),
                    });
                    continue;
                };
                if !receipt.success {
                    out.push(Discrepancy::WithdrawalMismatch {
                        request_id: w.request_id,
                        tx_hash: Some(*hash),
                        reason: "confirmed withdrawal has failed on-chain receipt (status 0)"
                            .into(),
                    });
                    continue;
                }
                if let Some(conf) = confirmed_block
                    && receipt.block_number > conf
                {
                    out.push(Discrepancy::WithdrawalMismatch {
                        request_id: w.request_id,
                        tx_hash: Some(*hash),
                        reason: format!(
                            "confirmed withdrawal receipt at block {} > confirmed block {conf}",
                            receipt.block_number
                        ),
                    });
                    continue;
                }
                if let Some(bh_str) = &w.block_hash {
                    let bh = parse_hash(bh_str, "withdrawals.block_hash")?;
                    if bh != receipt.block_hash {
                        out.push(Discrepancy::WithdrawalMismatch {
                            request_id: w.request_id,
                            tx_hash: Some(*hash),
                            reason: "withdrawals.block_hash does not match receipt block_hash"
                                .into(),
                        });
                        continue;
                    }
                }
                let valid_settle = entries.get(&settle_key).is_some_and(|e| {
                    matches_settle_lines(e, &wp_acc, *from, amount, receipt.fee())
                });
                if !valid_settle {
                    out.push(Discrepancy::WithdrawalMismatch {
                        request_id: w.request_id,
                        tx_hash: Some(*hash),
                        reason: "confirmed withdrawal missing or mismatched wd:settle entry".into(),
                    });
                }
            }
            Status::Failed => {
                let Some((from, nonce, raw, hash)) = signed else {
                    out.push(Discrepancy::WithdrawalMismatch {
                        request_id: w.request_id,
                        tx_hash: None,
                        reason: "failed withdrawal missing signed_txs row".into(),
                    });
                    continue;
                };
                if !raw_matches(raw, *hash, *from, *nonce, to, amount) {
                    out.push(Discrepancy::WithdrawalMismatch {
                        request_id: w.request_id,
                        tx_hash: Some(*hash),
                        reason: "signed_txs raw bytes do not match withdrawal".into(),
                    });
                    continue;
                }
                if has_settle {
                    out.push(Discrepancy::WithdrawalMismatch {
                        request_id: w.request_id,
                        tx_hash: Some(*hash),
                        reason: "failed withdrawal has unexpected wd:settle entry".into(),
                    });
                    continue;
                }
                if !has_valid_release {
                    out.push(Discrepancy::WithdrawalMismatch {
                        request_id: w.request_id,
                        tx_hash: Some(*hash),
                        reason: "failed withdrawal missing or mismatched wd:release entry".into(),
                    });
                    continue;
                }
                let Some(receipt) = rpc(chain.receipt(*hash).await)? else {
                    out.push(Discrepancy::WithdrawalMismatch {
                        request_id: w.request_id,
                        tx_hash: Some(*hash),
                        reason: "failed withdrawal has no canonical on-chain receipt".into(),
                    });
                    continue;
                };
                if receipt.success {
                    out.push(Discrepancy::WithdrawalMismatch {
                        request_id: w.request_id,
                        tx_hash: Some(*hash),
                        reason: "failed withdrawal has successful on-chain receipt (status 1)"
                            .into(),
                    });
                    continue;
                }
                let fee = receipt.fee();
                let fee_key = format!("wd:fee:{}:{hash}", config.chain_id);
                if fee > 0 {
                    let valid_fee = entries.get(&fee_key).is_some_and(|e| {
                        matches_two_lines(
                            e,
                            "wd:fee",
                            &AccountKey::network_fee(),
                            &AccountKey::hot_wallet(*from),
                            fee,
                        )
                    });
                    if !valid_fee {
                        out.push(Discrepancy::WithdrawalMismatch {
                            request_id: w.request_id,
                            tx_hash: Some(*hash),
                            reason: "failed withdrawal missing or mismatched wd:fee entry".into(),
                        });
                    }
                }
            }
            Status::Rejected => {}
        }
    }

    // `withdrawals` 행이 없는 고아 `wd:*` 분개 점검
    for key in entries.keys() {
        let req_id = key
            .strip_prefix("wd:hold:")
            .or_else(|| key.strip_prefix("wd:settle:"))
            .or_else(|| key.strip_prefix("wd:release:"));
        if let Some(id) = req_id
            && !known_requests.contains(id)
        {
            out.push(Discrepancy::WithdrawalMismatch {
                request_id: id.to_owned(),
                tx_hash: None,
                reason: format!("journal entry {key} has no withdrawals row"),
            });
        }
    }

    Ok(())
}

fn raw_matches(
    raw: &[u8],
    hash: Hash32,
    from: Address,
    nonce: u64,
    to: Address,
    value: u128,
) -> bool {
    signer::decode(raw).is_some_and(|d| {
        d.hash == hash && d.from == from && d.nonce == nonce && d.to == to && d.value == value
    })
}

fn matches_settle_lines(
    entry: &EntryData,
    wp_acc: &AccountKey,
    from: Address,
    amount: u128,
    fee: u128,
) -> bool {
    if entry.kind != "wd:settle" {
        return false;
    }
    let hw_acc = AccountKey::hot_wallet(from);
    let total = (amount + fee) as i128;
    let mut want = vec![
        (wp_acc.as_str().to_owned(), amount as i128),
        (hw_acc.as_str().to_owned(), -total),
    ];
    if fee > 0 {
        want.push((AccountKey::network_fee().as_str().to_owned(), fee as i128));
    }
    want.sort();
    entry.lines == want
}

fn parse_hash(s: &str, what: &str) -> Result<Hash32, ReconError> {
    Hash32::parse(s).ok_or_else(|| ReconError::Corrupt(format!("{what}: {s:?}")))
}

fn parse_addr(s: &str, what: &str) -> Result<Address, ReconError> {
    Address::parse(s).ok_or_else(|| ReconError::Corrupt(format!("{what}: {s:?}")))
}

fn parse_u128(s: &str, what: &str) -> Result<u128, ReconError> {
    s.parse::<u128>()
        .map_err(|_| ReconError::Corrupt(format!("{what}: {s:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_is_clean_only_when_discrepancies_empty() {
        let mut r = ReconReport {
            cursor: None,
            confirmed_block: None,
            discrepancies: vec![],
        };
        assert!(r.is_clean());
        r.discrepancies.push(Discrepancy::LedgerEquation {
            assets: 10,
            claims: 20,
        });
        assert!(!r.is_clean());
    }
}
