//! 원장 테스트가 함께 쓰는 메모리 원장(모델)과 분개 생성기.
//! 모델은 `vault::ledger::post`와 같은 규칙(검사 순서까지)을 따른다. DB 결과를 이 모델과 대조한다.
#![allow(dead_code)]

use std::collections::BTreeMap;

use proptest::prelude::*;
use sqlx::PgConnection;
use vault::ledger::{self, AccountKey, Amount, Line, NewEntry, PostError, Posted, Side};

/// 테스트가 여는 계정. 종류 6개가 모두 들어 있다.
pub fn opened() -> Vec<AccountKey> {
    vec![
        AccountKey::user(1),
        AccountKey::user(2),
        AccountKey::pending_deposit(1),
        AccountKey::withdrawal_pending(1),
        AccountKey::hot_wallet("0xa"),
        AccountKey::hot_wallet("0xb"),
        AccountKey::network_fee(),
        AccountKey::operator_capital(),
    ]
}

/// 열지 않은 계정. 분개에 넣으면 FK로만 걸린다(Rust 검사는 통과한다).
pub fn unknown() -> Vec<AccountKey> {
    vec![AccountKey::user(99), AccountKey::hot_wallet("0xdead")]
}

pub async fn open_all(conn: &mut PgConnection) {
    for k in opened() {
        ledger::open_account(conn, &k).await.expect("계정 열기");
    }
}

pub fn amt(v: u128) -> Amount {
    Amount::new(v).expect("0이 아닌 금액")
}

pub fn entry(key: impl Into<String>, kind: &str, lines: Vec<Line>) -> NewEntry {
    NewEntry {
        idempotency_key: key.into(),
        kind: kind.to_owned(),
        lines,
    }
}

/// 모델이 예상한 결과. `Existing`은 처음 기록한 분개의 순번(모델 안)이다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    New(usize),
    Existing(usize),
    TooFewLines,
    Unbalanced,
    UnknownAccount(AccountKey),
    InsufficientFunds(AccountKey),
    IdempotencyConflict(String),
}

/// 메모리 원장. 계정별 부호 있는 합(차변 +)과 멱등 키별 (내용, 순번)을 갖는다.
#[derive(Debug, Clone, Default)]
pub struct Model {
    raw: BTreeMap<AccountKey, i128>,
    keys: BTreeMap<String, (Body, usize)>,
    pub entries: usize,
}

type Body = (String, Vec<(AccountKey, i128)>);

fn body(e: &NewEntry) -> Body {
    let mut lines: Vec<(AccountKey, i128)> = e
        .lines
        .iter()
        .map(|l| (l.account.clone(), l.signed()))
        .collect();
    lines.sort();
    (e.kind.clone(), lines)
}

fn normal(key: &AccountKey, raw: i128) -> i128 {
    match key.kind().normal_side() {
        Side::Debit => raw,
        Side::Credit => -raw,
    }
}

impl Model {
    pub fn new() -> Self {
        let mut m = Self::default();
        for k in opened() {
            m.raw.insert(k, 0);
        }
        m
    }

    /// 정상 방향 잔액.
    pub fn balance(&self, key: &AccountKey) -> i128 {
        normal(key, self.raw.get(key).copied().unwrap_or(0))
    }

    /// `post`와 같은 순서로 판정하고, 받아들이면 반영한다.
    pub fn apply(&mut self, e: &NewEntry) -> Expect {
        if e.lines.len() < 2 {
            return Expect::TooFewLines;
        }
        let mut sum: i128 = 0;
        for l in &e.lines {
            match sum.checked_add(l.signed()) {
                Some(s) => sum = s,
                None => return Expect::Unbalanced,
            }
        }
        if sum != 0 {
            return Expect::Unbalanced;
        }
        let b = body(e);
        if let Some((existing, idx)) = self.keys.get(&e.idempotency_key) {
            return if *existing == b {
                Expect::Existing(*idx)
            } else {
                Expect::IdempotencyConflict(e.idempotency_key.clone())
            };
        }
        if let Some(l) = e.lines.iter().find(|l| !self.raw.contains_key(&l.account)) {
            return Expect::UnknownAccount(l.account.clone());
        }
        let mut next = self.raw.clone();
        for l in &e.lines {
            *next.get_mut(&l.account).unwrap() += l.signed();
        }
        let mut touched: Vec<&AccountKey> = e.lines.iter().map(|l| &l.account).collect();
        touched.sort();
        if let Some(k) = touched.into_iter().find(|k| normal(k, next[*k]) < 0) {
            return Expect::InsufficientFunds(k.clone());
        }
        self.raw = next;
        let idx = self.entries;
        self.entries += 1;
        self.keys.insert(e.idempotency_key.clone(), (b, idx));
        Expect::New(idx)
    }
}

/// DB 결과가 모델의 예상과 같은지 본다. 새로 기록된 분개의 ID는 `ids[순번]`에 쌓는다.
pub fn check(
    got: &Result<Posted, PostError>,
    want: &Expect,
    ids: &mut Vec<ledger::EntryId>,
) -> Result<(), String> {
    let ok = match (got, want) {
        (Ok(Posted::New(id)), Expect::New(idx)) => {
            assert_eq!(ids.len(), *idx);
            ids.push(*id);
            true
        }
        (Ok(Posted::Existing(id)), Expect::Existing(idx)) => ids.get(*idx) == Some(id),
        (Err(PostError::TooFewLines), Expect::TooFewLines) => true,
        (Err(PostError::Unbalanced), Expect::Unbalanced) => true,
        (Err(PostError::UnknownAccount(a)), Expect::UnknownAccount(b)) => a == b,
        (Err(PostError::InsufficientFunds(a)), Expect::InsufficientFunds(b)) => a == b,
        (Err(PostError::IdempotencyConflict(a)), Expect::IdempotencyConflict(b)) => a == b,
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(format!(
            "post 결과 {got:?}, 모델 예상 {want:?} (기록된 ID {ids:?})"
        ))
    }
}

/// 열어 둔 모든 계정의 DB 잔액이 모델과 같은지 본다.
pub async fn check_balances(conn: &mut PgConnection, model: &Model) -> Result<(), String> {
    for k in opened() {
        let got = ledger::balance(conn, &k).await.map_err(|e| e.to_string())?;
        if got != model.balance(&k) {
            return Err(format!("{k} 잔액 DB {got}, 모델 {}", model.balance(&k)));
        }
    }
    Ok(())
}

/// 분개 모양. 계정은 `opened()`(그리고 `Unknown`이면 `unknown()`)의 순번으로 고른다.
#[derive(Debug, Clone)]
pub enum Shape {
    /// 차 a / 대 b, 같은 금액. 줄 2개
    Pair { dr: usize, cr: usize, amount: u128 },
    /// 차 a x, 차 b y / 대 c x+y. 줄 3개(출금 지급 꼴)
    Split {
        dr1: usize,
        dr2: usize,
        cr: usize,
        x: u128,
        y: u128,
    },
    /// 두 쌍을 한 분개에. 줄 4개
    Double {
        a: (usize, usize, u128),
        b: (usize, usize, u128),
    },
}

fn idx() -> impl Strategy<Value = usize> {
    0..opened().len()
}

fn money() -> impl Strategy<Value = u128> {
    1u128..=1_000
}

pub fn shape() -> impl Strategy<Value = Shape> {
    prop_oneof![
        3 => (idx(), idx(), money()).prop_map(|(dr, cr, amount)| Shape::Pair { dr, cr, amount }),
        1 => (idx(), idx(), idx(), money(), money())
            .prop_map(|(dr1, dr2, cr, x, y)| Shape::Split { dr1, dr2, cr, x, y }),
        1 => ((idx(), idx(), money()), (idx(), idx(), money()))
            .prop_map(|(a, b)| Shape::Double { a, b }),
    ]
}

impl Shape {
    pub fn lines(&self) -> Vec<Line> {
        let acc = opened();
        match *self {
            Shape::Pair { dr, cr, amount } => vec![
                Line::debit(acc[dr].clone(), amt(amount)),
                Line::credit(acc[cr].clone(), amt(amount)),
            ],
            Shape::Split { dr1, dr2, cr, x, y } => vec![
                Line::debit(acc[dr1].clone(), amt(x)),
                Line::debit(acc[dr2].clone(), amt(y)),
                Line::credit(acc[cr].clone(), amt(x + y)),
            ],
            Shape::Double { a, b } => vec![
                Line::debit(acc[a.0].clone(), amt(a.2)),
                Line::credit(acc[a.1].clone(), amt(a.2)),
                Line::debit(acc[b.0].clone(), amt(b.2)),
                Line::credit(acc[b.1].clone(), amt(b.2)),
            ],
        }
    }
}

/// 언제나 받아들여지는 분개: 핫월렛에 코인이 들어오고 운영 자본이 는다(차변·대변 정상 방향 모두 증가).
pub fn funding(key: impl Into<String>, amount: u128) -> NewEntry {
    entry(
        key,
        "cap:fund",
        vec![
            Line::debit(AccountKey::hot_wallet("0xa"), amt(amount)),
            Line::credit(AccountKey::operator_capital(), amt(amount)),
        ],
    )
}

/// 테스트마다 쓰는 tokio 런타임(proptest 경우마다 block_on).
pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio 런타임")
}

/// DB 테스트라 경우 수를 작게 둔다. 실패하면 축소된 입력이 출력된다(파일로 남기지는 않는다).
pub fn config(cases: u32) -> proptest::test_runner::Config {
    proptest::test_runner::Config {
        cases,
        max_shrink_iters: 256,
        failure_persistence: None,
        ..proptest::test_runner::Config::default()
    }
}
