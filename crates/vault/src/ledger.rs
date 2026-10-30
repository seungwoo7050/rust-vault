//! 복식부기 원장(ADR-001). 분개와 줄은 추가만 하고, 잔액은 줄 금액의 합으로 계산한다.
//!
//! `post`는 호출자가 연 트랜잭션 안에서 savepoint 하나로 분개를 기록하고, 스스로 커밋하지 않는다
//! (결정 8). 거부되면 savepoint만 되돌리므로 호출자는 같은 트랜잭션을 계속 쓰고 커밋할 수 있다.

use std::fmt;

use sha2::{Digest, Sha256};
use sqlx::{Acquire, PgConnection, Postgres, Transaction};

/// 계정 종류(ADR-001 결정 4). 정상 잔액 방향이 종류마다 정해져 있다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AccountKind {
    User,
    PendingDeposit,
    WithdrawalPending,
    HotWallet,
    NetworkFee,
    OperatorCapital,
}

impl AccountKind {
    pub const ALL: [AccountKind; 6] = [
        AccountKind::User,
        AccountKind::PendingDeposit,
        AccountKind::WithdrawalPending,
        AccountKind::HotWallet,
        AccountKind::NetworkFee,
        AccountKind::OperatorCapital,
    ];

    /// DB의 `accounts.kind` 값이자 계정 키의 앞부분.
    pub fn as_str(self) -> &'static str {
        match self {
            AccountKind::User => "user",
            AccountKind::PendingDeposit => "pending_deposit",
            AccountKind::WithdrawalPending => "withdrawal_pending",
            AccountKind::HotWallet => "hot_wallet",
            AccountKind::NetworkFee => "network_fee",
            AccountKind::OperatorCapital => "operator_capital",
        }
    }

    /// 정상 잔액 방향. 자산·비용은 차변, 부채·자본은 대변.
    pub fn normal_side(self) -> Side {
        match self {
            AccountKind::HotWallet | AccountKind::NetworkFee => Side::Debit,
            AccountKind::User
            | AccountKind::PendingDeposit
            | AccountKind::WithdrawalPending
            | AccountKind::OperatorCapital => Side::Credit,
        }
    }

    /// 계정 하나뿐인 종류(`network_fee`, `operator_capital`)는 키에 꼬리가 없다.
    fn singleton(self) -> bool {
        matches!(self, AccountKind::NetworkFee | AccountKind::OperatorCapital)
    }
}

/// 계정 키. `user:42`, `pending_deposit:42`, `withdrawal_pending:42`, `hot_wallet:0xabc`,
/// `network_fee`, `operator_capital`. 종류는 키의 앞부분에서 정해진다.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AccountKey(String);

/// 계정 키 형식이 틀렸다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidAccountKey(pub String);

impl fmt::Display for InvalidAccountKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid account key: {:?}", self.0)
    }
}

impl std::error::Error for InvalidAccountKey {}

impl AccountKey {
    /// 문자열 키를 검사해 받는다.
    pub fn parse(s: &str) -> Result<Self, InvalidAccountKey> {
        let bad = || InvalidAccountKey(s.to_owned());
        let (prefix, rest) = match s.split_once(':') {
            Some((p, r)) => (p, Some(r)),
            None => (s, None),
        };
        let kind = AccountKind::ALL
            .into_iter()
            .find(|k| k.as_str() == prefix)
            .ok_or_else(bad)?;
        match (kind.singleton(), rest) {
            (true, None) => {}
            (false, Some(r)) if !r.is_empty() && !r.contains(char::is_whitespace) => {}
            _ => return Err(bad()),
        }
        Ok(Self(s.to_owned()))
    }

    pub fn user(id: impl fmt::Display) -> Self {
        Self(format!("user:{id}"))
    }
    pub fn pending_deposit(id: impl fmt::Display) -> Self {
        Self(format!("pending_deposit:{id}"))
    }
    pub fn withdrawal_pending(id: impl fmt::Display) -> Self {
        Self(format!("withdrawal_pending:{id}"))
    }
    pub fn hot_wallet(address: impl fmt::Display) -> Self {
        Self(format!("hot_wallet:{address}"))
    }
    pub fn network_fee() -> Self {
        Self("network_fee".to_owned())
    }
    pub fn operator_capital() -> Self {
        Self("operator_capital".to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn kind(&self) -> AccountKind {
        let prefix = self.0.split(':').next().unwrap_or("");
        AccountKind::ALL
            .into_iter()
            .find(|k| k.as_str() == prefix)
            .expect("AccountKey는 생성 때 검사한다")
    }
}

impl fmt::Display for AccountKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 차변·대변.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Debit,
    Credit,
}

/// 줄 금액. 0이 아니고 `i128::MAX` 이하다(DB에는 부호를 붙여 i128로 들어간다).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Amount(u128);

impl Amount {
    pub fn new(v: u128) -> Option<Self> {
        (v != 0 && v <= i128::MAX as u128).then_some(Self(v))
    }
    pub fn get(self) -> u128 {
        self.0
    }
}

/// 분개 줄 하나.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub account: AccountKey,
    pub side: Side,
    pub amount: Amount,
}

impl Line {
    pub fn debit(account: AccountKey, amount: Amount) -> Self {
        Self {
            account,
            side: Side::Debit,
            amount,
        }
    }
    pub fn credit(account: AccountKey, amount: Amount) -> Self {
        Self {
            account,
            side: Side::Credit,
            amount,
        }
    }
    /// 차변 +, 대변 −.
    pub fn signed(&self) -> i128 {
        let v = self.amount.0 as i128;
        match self.side {
            Side::Debit => v,
            Side::Credit => -v,
        }
    }
}

/// 기록할 분개. 멱등 키는 외부 사실로만 만든다(결정 6의 표).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEntry {
    pub idempotency_key: String,
    pub kind: String,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryId(pub i64);

/// 기록 결과. 같은 키·같은 내용의 재기록은 `Existing`이고 아무것도 쓰지 않는다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posted {
    New(EntryId),
    Existing(EntryId),
}

impl Posted {
    pub fn id(self) -> EntryId {
        match self {
            Posted::New(id) | Posted::Existing(id) => id,
        }
    }
}

/// 분개 거부 사유. `Db` 말고는 savepoint를 되돌린 뒤 돌려주므로 트랜잭션은 계속 쓸 수 있다.
#[derive(Debug)]
pub enum PostError {
    /// 줄 합이 0이 아니다.
    Unbalanced,
    /// 줄이 2개 미만이다.
    TooFewLines,
    /// 열지 않은 계정에 줄이 있다.
    UnknownAccount(AccountKey),
    /// 이 분개를 기록하면 계정의 정상 방향 잔액이 음수가 된다.
    InsufficientFunds(AccountKey),
    /// 같은 멱등 키로 내용이 다른 분개가 이미 있다.
    IdempotencyConflict(String),
    Db(sqlx::Error),
}

impl fmt::Display for PostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PostError::Unbalanced => f.write_str("entry is unbalanced"),
            PostError::TooFewLines => f.write_str("entry needs at least two lines"),
            PostError::UnknownAccount(k) => write!(f, "unknown account {k}"),
            PostError::InsufficientFunds(k) => write!(f, "insufficient funds in {k}"),
            PostError::IdempotencyConflict(k) => {
                write!(f, "idempotency key {k} already used with a different body")
            }
            PostError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for PostError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PostError::Db(e) => Some(e),
            _ => None,
        }
    }
}

impl From<sqlx::Error> for PostError {
    fn from(e: sqlx::Error) -> Self {
        PostError::Db(e)
    }
}

/// 계정을 연다. 이미 있으면 아무것도 하지 않는다(멱등).
pub async fn open_account(conn: &mut PgConnection, key: &AccountKey) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO accounts (key, kind) VALUES ($1, $2) ON CONFLICT (key) DO NOTHING")
        .bind(key.as_str())
        .bind(key.kind().as_str())
        .execute(conn)
        .await?;
    Ok(())
}

/// 정상 방향 기준 잔액. 줄이 없으면 0.
pub async fn balance(conn: &mut PgConnection, key: &AccountKey) -> Result<i128, sqlx::Error> {
    let raw = signed_sum(conn, key).await?;
    Ok(match key.kind().normal_side() {
        Side::Debit => raw,
        Side::Credit => -raw,
    })
}

/// 전체 줄 합. 언제나 0이어야 한다(I3).
pub async fn trial_balance(conn: &mut PgConnection) -> Result<i128, sqlx::Error> {
    let s: String = sqlx::query_scalar("SELECT coalesce(sum(amount), 0)::text FROM journal_lines")
        .fetch_one(conn)
        .await?;
    parse_i128(&s)
}

/// 분개를 기록한다. 검사 순서는 ADR-001 결정 8 그대로다.
/// Rust에서 차대 합·줄 수 → savepoint 안에서 분개 INSERT → 기존 키면 body_hash 비교
/// → 줄 INSERT(계정 FK) → 계정 잠금 후 잔액 검사.
pub async fn post(
    tx: &mut Transaction<'_, Postgres>,
    entry: NewEntry,
) -> Result<Posted, PostError> {
    validate(&entry)?;
    let hash = body_hash(&entry);

    // 거부되면 이 분개의 흔적만 되돌린다. FK 위반 같은 DB 오류가 바깥 트랜잭션을 중단시키지 않는다
    let mut sp = tx.begin().await?;
    let result = write_entry(&mut sp, &entry, &hash).await;
    if result.is_ok() {
        sp.commit().await?;
    } else {
        sp.rollback().await?;
    }

    match result {
        Ok(posted) => Ok(posted),
        Err(Written::UnknownAccount) => Err(find_unknown_account(tx, &entry).await?),
        Err(Written::Rejected(e)) => Err(e),
    }
}

/// 쓰기 전에 거르는 것은 차대 합과 줄 수뿐이다.
fn validate(entry: &NewEntry) -> Result<(), PostError> {
    if entry.lines.len() < 2 {
        return Err(PostError::TooFewLines);
    }
    let mut sum: i128 = 0;
    for line in &entry.lines {
        sum = sum
            .checked_add(line.signed())
            .ok_or(PostError::Unbalanced)?;
    }
    if sum != 0 {
        return Err(PostError::Unbalanced);
    }
    Ok(())
}

/// 분개 kind와 (계정 키, 부호 있는 금액)을 계정 키 순서로 정렬해 길이를 붙여 인코딩한 sha256.
/// 멱등 키·시각·난수는 넣지 않는다.
pub fn body_hash(entry: &NewEntry) -> Vec<u8> {
    let mut lines: Vec<(&str, i128)> = entry
        .lines
        .iter()
        .map(|l| (l.account.as_str(), l.signed()))
        .collect();
    lines.sort();
    let mut h = Sha256::new();
    h.update((entry.kind.len() as u64).to_be_bytes());
    h.update(entry.kind.as_bytes());
    h.update((lines.len() as u64).to_be_bytes());
    for (key, amount) in lines {
        h.update((key.len() as u64).to_be_bytes());
        h.update(key.as_bytes());
        h.update(amount.to_be_bytes());
    }
    h.finalize().to_vec()
}

enum Written {
    /// 줄 INSERT가 FK 위반. 어느 계정인지는 savepoint를 되돌린 뒤 찾는다
    UnknownAccount,
    Rejected(PostError),
}

impl From<sqlx::Error> for Written {
    fn from(e: sqlx::Error) -> Self {
        Written::Rejected(PostError::Db(e))
    }
}

async fn write_entry(
    conn: &mut PgConnection,
    entry: &NewEntry,
    hash: &[u8],
) -> Result<Posted, Written> {
    let inserted: Option<i64> = sqlx::query_scalar(
        "INSERT INTO journal_entries (idempotency_key, kind, body_hash) VALUES ($1, $2, $3)
         ON CONFLICT (idempotency_key) DO NOTHING RETURNING id",
    )
    .bind(&entry.idempotency_key)
    .bind(&entry.kind)
    .bind(hash)
    .fetch_optional(&mut *conn)
    .await?;

    let id = match inserted {
        Some(id) => id,
        None => {
            let (id, existing): (i64, Vec<u8>) = sqlx::query_as(
                "SELECT id, body_hash FROM journal_entries WHERE idempotency_key = $1",
            )
            .bind(&entry.idempotency_key)
            .fetch_one(&mut *conn)
            .await?;
            if existing != hash {
                return Err(Written::Rejected(PostError::IdempotencyConflict(
                    entry.idempotency_key.clone(),
                )));
            }
            return Ok(Posted::Existing(EntryId(id)));
        }
    };

    for line in &entry.lines {
        let r = sqlx::query(
            "INSERT INTO journal_lines (entry_id, account_key, amount) VALUES ($1, $2, $3::numeric)",
        )
        .bind(id)
        .bind(line.account.as_str())
        .bind(line.signed().to_string())
        .execute(&mut *conn)
        .await;
        match r {
            Ok(_) => {}
            Err(sqlx::Error::Database(e)) if e.is_foreign_key_violation() => {
                return Err(Written::UnknownAccount);
            }
            Err(e) => return Err(e.into()),
        }
    }

    // 계정 행을 키 순서로 잠근 뒤 잔액을 본다(교착 방지). 줄 INSERT의 FK 검사가 잡은 KEY SHARE 잠금과
    // 충돌하지 않도록 FOR NO KEY UPDATE를 쓴다. 잠금끼리는 FOR UPDATE처럼 서로 배제한다
    let mut keys: Vec<&AccountKey> = entry.lines.iter().map(|l| &l.account).collect();
    keys.sort();
    keys.dedup();
    let names: Vec<String> = keys.iter().map(|k| k.as_str().to_owned()).collect();
    sqlx::query("SELECT key FROM accounts WHERE key = ANY($1) ORDER BY key FOR NO KEY UPDATE")
        .bind(names)
        .fetch_all(&mut *conn)
        .await?;
    for key in keys {
        if balance(conn, key).await? < 0 {
            return Err(Written::Rejected(PostError::InsufficientFunds(key.clone())));
        }
    }

    Ok(Posted::New(EntryId(id)))
}

async fn find_unknown_account(
    conn: &mut PgConnection,
    entry: &NewEntry,
) -> Result<PostError, sqlx::Error> {
    let names: Vec<String> = entry
        .lines
        .iter()
        .map(|l| l.account.as_str().to_owned())
        .collect();
    let known: Vec<String> = sqlx::query_scalar("SELECT key FROM accounts WHERE key = ANY($1)")
        .bind(&names)
        .fetch_all(conn)
        .await?;
    let missing = entry
        .lines
        .iter()
        .map(|l| &l.account)
        .find(|k| !known.iter().any(|n| n == k.as_str()));
    Ok(match missing {
        Some(k) => PostError::UnknownAccount(k.clone()),
        // FK가 거부했는데 지금은 다 있다: 동시에 누가 계정을 열었다. 다시 시도할 수 있는 거부로 돌려준다
        None => PostError::UnknownAccount(entry.lines[0].account.clone()),
    })
}

async fn signed_sum(conn: &mut PgConnection, key: &AccountKey) -> Result<i128, sqlx::Error> {
    let s: String = sqlx::query_scalar(
        "SELECT coalesce(sum(amount), 0)::text FROM journal_lines WHERE account_key = $1",
    )
    .bind(key.as_str())
    .fetch_one(conn)
    .await?;
    parse_i128(&s)
}

fn parse_i128(s: &str) -> Result<i128, sqlx::Error> {
    s.parse::<i128>()
        .map_err(|e| sqlx::Error::Decode(format!("ledger sum {s:?} does not fit i128: {e}").into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn amt(v: u128) -> Amount {
        Amount::new(v).unwrap()
    }

    #[test]
    fn amount_bounds() {
        assert!(Amount::new(0).is_none());
        assert!(Amount::new(i128::MAX as u128).is_some());
        assert!(Amount::new(i128::MAX as u128 + 1).is_none());
    }

    #[test]
    fn account_key_parse() {
        for ok in [
            "user:1",
            "pending_deposit:1",
            "withdrawal_pending:1",
            "hot_wallet:0xab",
            "network_fee",
            "operator_capital",
        ] {
            assert_eq!(AccountKey::parse(ok).unwrap().as_str(), ok);
        }
        for bad in ["user", "user:", "network_fee:1", "bank:1", "", "user:a b"] {
            assert!(AccountKey::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(
            AccountKey::hot_wallet("0xab").kind().normal_side(),
            Side::Debit
        );
        assert_eq!(AccountKey::user(1).kind().normal_side(), Side::Credit);
    }

    #[test]
    fn validate_rejects_before_writing() {
        let one = NewEntry {
            idempotency_key: "k".into(),
            kind: "t".into(),
            lines: vec![Line::debit(AccountKey::network_fee(), amt(1))],
        };
        assert!(matches!(validate(&one), Err(PostError::TooFewLines)));
        let unbalanced = NewEntry {
            lines: vec![
                Line::debit(AccountKey::network_fee(), amt(2)),
                Line::credit(AccountKey::operator_capital(), amt(1)),
            ],
            ..one.clone()
        };
        assert!(matches!(validate(&unbalanced), Err(PostError::Unbalanced)));
        let overflow = NewEntry {
            lines: vec![
                Line::debit(AccountKey::network_fee(), amt(i128::MAX as u128)),
                Line::debit(AccountKey::network_fee(), amt(i128::MAX as u128)),
                Line::credit(AccountKey::operator_capital(), amt(1)),
            ],
            ..one
        };
        assert!(matches!(validate(&overflow), Err(PostError::Unbalanced)));
    }

    #[test]
    fn body_hash_ignores_line_order_and_key() {
        let a = NewEntry {
            idempotency_key: "k1".into(),
            kind: "cap:fund".into(),
            lines: vec![
                Line::debit(AccountKey::hot_wallet("0xa"), amt(5)),
                Line::credit(AccountKey::operator_capital(), amt(5)),
            ],
        };
        let mut b = a.clone();
        b.idempotency_key = "k2".into();
        b.lines.reverse();
        assert_eq!(body_hash(&a), body_hash(&b));
        let mut c = a.clone();
        c.lines[0].amount = amt(6);
        c.lines[1].amount = amt(6);
        assert_ne!(body_hash(&a), body_hash(&c));
        let mut d = a.clone();
        d.kind = "other".into();
        assert_ne!(body_hash(&a), body_hash(&d));
    }
}
