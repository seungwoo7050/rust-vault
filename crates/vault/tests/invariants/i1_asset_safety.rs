//! I1 자산 안전성: 사용자 잔액은 승인된 원장 거래 없이 변경될 수 없다

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};
use vault::ledger::{self, AccountKey, Line, NewEntry, Side};
use vault::withdrawal::Status;
use vault_sim::chain::transfer;
use vault_sim::db::TestDb;

use crate::model::{self, Model, Shape};
use crate::scan;
use crate::withdraw::{self, WEnv};

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
    let w = WEnv::new().await;
    let user = AccountKey::user(1);

    // 입금 블록 위로 1개 더: 확정 수 2 = N − 1
    let chain = w.chain();
    let b1 = chain.mine_canonical(chain.tip().hash, vec![transfer(1, scan::addr(1), 100)]);
    scan::extend(chain, b1, (scan::N - 2) as usize);
    w.scan().await;
    assert_eq!(
        w.env.cursor().await.number.0,
        chain.header(b1).number.0 + scan::N - 2,
        "확정 수 N − 1"
    );

    // 확정 전: 같은 금액 출금은 `Rejected`, 분개 없음
    let early = w
        .request("t1.2-early", 1, withdraw::recipient(1), 100)
        .await;
    assert_eq!(
        early.status,
        Status::Rejected,
        "확정 전 출금은 Rejected여야 한다"
    );
    assert_eq!(
        w.entries("wd:hold:").await,
        0,
        "Rejected인데 홀드 분개가 있다"
    );
    assert_eq!(w.balance(&user).await, 0, "확정 전 user:1은 0이어야 한다");
    assert_eq!(w.balance(&AccountKey::pending_deposit(1)).await, 100);

    // 블록 하나 더: 확정 수 N. 같은 금액 출금이 `Held`
    w.mine(1).await;
    assert_eq!(w.balance(&user).await, 100);
    let late = w.request("t1.2-late", 1, withdraw::recipient(2), 100).await;
    assert_eq!(late.status, Status::Held, "확정 뒤 출금은 Held여야 한다");
    assert_eq!(w.balance(&user).await, 0);
    assert_eq!(w.balance(&AccountKey::withdrawal_pending(1)).await, 100);

    w.env.db.drop_db().await;
}

// T1.3 실패해야 할 변형: 홀드 분개의 사용자 잔액 음수 검사 제거
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hold_rejects_overspend() {
    let w = WEnv::new().await;
    let user = AccountKey::user(1);
    w.deposit(1, 100).await;
    assert_eq!(w.balance(&user).await, 100);

    // 잔액보다 큰 출금
    let big = w.request("t1.3-big", 1, withdraw::recipient(1), 101).await;
    assert_eq!(big.status, Status::Rejected, "잔액보다 큰 출금");
    assert_eq!(w.balance(&user).await, 100);

    // 잔액을 나눠 쓰는 동시 출금 두 건(각각은 잔액 안, 합은 넘는다)
    let (a, b) = tokio::join!(
        w.request("t1.3-a", 1, withdraw::recipient(2), 60),
        w.request("t1.3-b", 1, withdraw::recipient(3), 60),
    );
    let held = [&a, &b].iter().filter(|x| x.status == Status::Held).count();
    let rejected = [&a, &b]
        .iter()
        .filter(|x| x.status == Status::Rejected)
        .count();
    assert_eq!(
        (held, rejected),
        (1, 1),
        "동시 두 건 중 하나만 Held: {:?}, {:?}",
        a.status,
        b.status
    );
    assert_eq!(w.entries("wd:hold:").await, 1, "홀드 분개 수");
    let left = w.balance(&user).await;
    assert!(left >= 0, "user:1 = {left}");
    assert_eq!(left, 40);
    assert_eq!(w.balance(&AccountKey::withdrawal_pending(1)).await, 60);

    w.env.db.drop_db().await;
}
