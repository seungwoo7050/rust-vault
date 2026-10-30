//! I1 자산 안전성: 사용자 잔액은 승인된 원장 거래 없이 변경될 수 없다

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};
use vault::ledger::{self, AccountKey, Line, NewEntry, PostError, Posted, Side};
use vault_sim::chain::transfer;
use vault_sim::db::TestDb;

use crate::model::{self, Model, Shape};
use crate::scan::{self, Env};

// T1.1 실패해야 할 변형: 분개 기록에서 savepoint를 없앰(없는 계정의 FK 위반이 트랜잭션 전체를 중단시키고, 그 트랜잭션의 커밋은 롤백으로 끝나 정상 분개까지 사라진다)
#[test]
fn rejected_entry_changes_no_balance() {
    let rt = model::runtime();
    let mut runner = TestRunner::new(model::config(32));
    // [자금 입금] + 앞 열 + [없는 계정 분개] + 뒤 열 + [자금 입금]. 없는 계정 분개는 FK로만 걸리므로
    // savepoint가 없으면 트랜잭션이 중단된다. 그 앞뒤에 언제나 받아들여지는 분개를 둔다
    let ops = (
        vec(op(), 0..20),
        0..model::unknown().len(),
        vec(op(), 0..20),
    );
    runner
        .run(&ops, |(head, unknown, tail)| {
            let mut seq = vec![Op::Fund(500)];
            seq.extend(head);
            seq.push(Op::Unknown(
                Shape::Pair {
                    dr: 4,
                    cr: 0,
                    amount: 10,
                },
                unknown,
            ));
            seq.extend(tail);
            seq.push(Op::Fund(1));
            rt.block_on(run_mixed(seq)).map_err(TestCaseError::fail)
        })
        .unwrap();
}

/// 정상 분개와 거부될 분개(없는 계정, 차대 불일치, 사용자 잔액 부족). 받아들일지는 모델이 정한다.
#[derive(Debug, Clone)]
enum Op {
    /// 임의 모양. 잔액이 모자라면 거부된다
    Normal(Shape),
    /// 언제나 받아들여지는 자금 입금
    Fund(u128),
    /// 모양의 첫 줄 계정을 열지 않은 계정으로 바꾼다
    Unknown(Shape, usize),
    /// 대변 줄 금액을 하나 늘린다
    Unbalanced(Shape),
    /// `user:{u}`의 잔액보다 1 많이 출금 홀드
    Overspend(usize),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => model::shape().prop_map(Op::Normal),
        1 => (1u128..=1_000).prop_map(Op::Fund),
        1 => (model::shape(), 0..model::unknown().len()).prop_map(|(s, u)| Op::Unknown(s, u)),
        1 => model::shape().prop_map(Op::Unbalanced),
        1 => (1usize..=2).prop_map(Op::Overspend),
    ]
}

impl Op {
    fn entry(&self, key: String, m: &Model) -> NewEntry {
        match self {
            Op::Normal(s) => model::entry(key, "test", s.lines()),
            Op::Fund(a) => model::funding(key, *a),
            Op::Unknown(s, u) => {
                let mut lines = s.lines();
                lines[0].account = model::unknown()[*u].clone();
                model::entry(key, "test", lines)
            }
            Op::Unbalanced(s) => {
                let mut lines = s.lines();
                let cr = lines.iter_mut().find(|l| l.side == Side::Credit).unwrap();
                cr.amount = model::amt(cr.amount.get() + 1);
                model::entry(key, "test", lines)
            }
            Op::Overspend(u) => {
                let user = AccountKey::user(u);
                let over = model::amt(m.balance(&user) as u128 + 1);
                model::entry(
                    key,
                    "wd:hold",
                    vec![
                        Line::debit(user, over),
                        Line::credit(AccountKey::withdrawal_pending(1), over),
                    ],
                )
            }
        }
    }
}

async fn run_mixed(seq: Vec<Op>) -> Result<(), String> {
    let db = TestDb::create().await;
    let result = async {
        let mut conn = db.pool.acquire().await.map_err(|e| e.to_string())?;
        model::open_all(&mut conn).await;
        let mut m = Model::new();
        let mut ids = Vec::new();

        // 한 트랜잭션 안에 모두 기록하고 커밋한다
        let mut tx = db.pool.begin().await.map_err(|e| e.to_string())?;
        for (i, op) in seq.iter().enumerate() {
            let e = op.entry(format!("t1.1:{i}"), &m);
            let want = m.apply(&e);
            let got = ledger::post(&mut tx, e).await;
            model::check(&got, &want, &mut ids).map_err(|err| format!("{i}번째 {op:?}: {err}"))?;
        }
        tx.commit().await.map_err(|e| format!("커밋 실패: {e}"))?;

        model::check_balances(&mut conn, &m).await?;
        let entries: i64 = sqlx::query_scalar("SELECT count(*) FROM journal_entries")
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| e.to_string())?;
        if entries != m.entries as i64 {
            return Err(format!("분개 수 DB {entries}, 모델 {}", m.entries));
        }
        Ok(())
    }
    .await;
    db.drop_db().await;
    result
}

// T1.2 실패해야 할 변형: 잠정 입금을 사용자 계정에 바로 대변 기록
#[tokio::test]
async fn pending_deposit_not_spendable() {
    let env = Env::new().await;
    let user = AccountKey::user(1);
    let mut conn = env.conn().await;
    ledger::open_account(&mut conn, &AccountKey::withdrawal_pending(1))
        .await
        .unwrap();

    // 입금 블록 1 위로 1개 더: 확정 수 2 = N − 1
    let g = env.chain.genesis().hash;
    let b1 = env
        .chain
        .mine_canonical(g, vec![transfer(1, scan::addr(1), 100)]);
    scan::extend(&env.chain, b1, (scan::N - 2) as usize);
    let mut scanner = env.scanner().await;
    scanner.run_until_idle().await.unwrap();
    assert_eq!(env.cursor().await.number.0, scan::N - 1);

    // 출금 모듈은 05에서 생긴다. 여기서는 ADR-001의 홀드 분개를 원장에 직접 기록한다
    let mut tx = env.db.pool.begin().await.unwrap();
    let early = ledger::post(&mut tx, hold("wd:hold:t1.2-early", 100)).await;
    tx.commit().await.unwrap();
    assert!(
        matches!(&early, Err(PostError::InsufficientFunds(k)) if *k == user),
        "확정 전 홀드는 InsufficientFunds(user:1)여야 한다, 받은 것 {early:?}"
    );
    assert_eq!(env.balance(&user).await, 0, "확정 전 user:1은 0이어야 한다");
    assert_eq!(env.balance(&AccountKey::pending_deposit(1)).await, 100);

    // 블록 하나 더: 확정 수 N. 같은 금액 홀드가 기록된다
    scan::extend(&env.chain, env.chain.tip().hash, 1);
    scanner.run_until_idle().await.unwrap();
    assert_eq!(env.balance(&user).await, 100);
    let mut tx = env.db.pool.begin().await.unwrap();
    let late = ledger::post(&mut tx, hold("wd:hold:t1.2-late", 100)).await;
    tx.commit().await.unwrap();
    assert!(
        matches!(late, Ok(Posted::New(_))),
        "확정 뒤 홀드는 기록되어야 한다, 받은 것 {late:?}"
    );
    assert_eq!(env.balance(&user).await, 0);
    assert_eq!(env.balance(&AccountKey::withdrawal_pending(1)).await, 100);

    drop(conn);
    env.db.drop_db().await;
}

/// 출금 홀드 분개(ADR-001): 차 `user:1` / 대 `withdrawal_pending:1`.
fn hold(key: &str, amount: u128) -> NewEntry {
    model::entry(
        key,
        "wd:hold",
        vec![
            Line::debit(AccountKey::user(1), model::amt(amount)),
            Line::credit(AccountKey::withdrawal_pending(1), model::amt(amount)),
        ],
    )
}

// T1.3 실패해야 할 변형: 홀드 분개의 사용자 잔액 음수 검사 제거
#[test]
#[ignore = "05-withdrawal"]
fn hold_rejects_overspend() {
    unimplemented!(
        "T1.3: 잔액보다 큰 출금, 잔액을 나눠 쓰는 동시 출금 두 건. 단언: 큰 출금 `Rejected`, 동시 두 건 중 잔액 안의 것만 `Held`, `user:u` ≥ 0"
    );
}
