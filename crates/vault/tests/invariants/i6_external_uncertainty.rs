//! I6 외부 불확실성: RPC 타임아웃이나 응답 유실을 곧바로 트랜잭션 실패로 판단하지 않는다

use std::time::Duration;

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};

use vault::chain::BlockNumber;
use vault::deposit::ScanError;
use vault::ledger::AccountKey;
use vault::store::{FaultPoint, Injected, Site};
use vault::withdrawal::{Status, Step, WithdrawalError};
use vault_sim::chain::{Call, Fault, transfer};

use crate::model;
use crate::scan::{self, Env};
use crate::withdraw::{self, WEnv};

// T6.1 실패해야 할 변형: 타임아웃을 `Failed`로 매핑하고 홀드 해제
#[tokio::test]
async fn send_timeout_moves_to_unknown() {
    let w = WEnv::new().await;
    w.deposit(1, 1_000).await;
    let to = withdraw::recipient(1);
    w.request("t6.1", 1, to, 100).await;

    // 전송 요청은 반영하고 응답만 버린다
    w.chain().fail_next(Call::SendRaw, 1, Fault::LostResponse);
    let mut p = w.processor().await;
    assert_moved(p.step().await, Status::Held, Status::Signed);
    assert_moved(p.step().await, Status::Signed, Status::Unknown);
    assert_eq!(w.chain().pool().len(), 1, "노드는 tx를 받았다");
    assert_held(&w, "t6.1", 100, 900).await;

    // 그 뒤 재전송과 조회도 결과를 모른다. 여전히 `Unknown`이고 새 서명은 없다
    w.chain().fail_next(Call::SendRaw, 3, Fault::Indeterminate);
    w.chain().fail_next(Call::Receipt, 1, Fault::Indeterminate);
    for _ in 0..4 {
        p.run_until_idle().await.unwrap();
    }
    assert_eq!(w.status("t6.1").await, Status::Unknown);
    assert_held(&w, "t6.1", 100, 900).await;
    assert_eq!(w.signed_rows().await, 1, "새 서명 tx가 생겼다");
    let stored = w.stored("t6.1").await;
    assert!(
        w.chain().received().iter().all(|r| *r == stored.raw),
        "저장본과 다른 raw tx를 보냈다"
    );
    w.env.db.drop_db().await;
}

// T6.2 실패해야 할 변형: 기한이 지나면 `Unknown`·`NeedsReview`를 `Failed`로 바꾸고 홀드 해제((나)에서 깨진다)
#[tokio::test]
async fn lost_response_resolved_by_receipt() {
    for past_deadline in [false, true] {
        let case = if past_deadline { "(나)" } else { "(가)" };
        let w = WEnv::new().await;
        w.deposit(1, 1_000).await;
        let to = withdraw::recipient(1);
        w.request("t6.2", 1, to, 100).await;

        // 응답 유실 → `Unknown`, 그 뒤 영수증 조회가 몇 번 타임아웃된다
        w.chain().fail_next(Call::SendRaw, 1, Fault::LostResponse);
        let mut p = w.processor().await;
        assert_moved(p.step().await, Status::Held, Status::Signed);
        assert_moved(p.step().await, Status::Signed, Status::Unknown);
        w.chain().fail_next(Call::Receipt, 3, Fault::Indeterminate);
        if past_deadline {
            // 가짜 시계를 기한 너머로 돌린다. 조회가 결과를 모르는 동안 기한이 지나 `NeedsReview`
            w.clock
                .advance(withdraw::REVIEW_AFTER + Duration::from_secs(1));
            assert_moved(p.step().await, Status::Unknown, Status::NeedsReview);
            assert_held(&w, "t6.2", 100, 900).await;
            p.run_until_idle().await.unwrap();
            assert_eq!(w.status("t6.2").await, Status::NeedsReview, "{case}");
        } else {
            for _ in 0..3 {
                assert_eq!(
                    p.step().await.unwrap(),
                    Step::Waiting {
                        request_id: "t6.2".into(),
                        status: Status::Unknown
                    },
                    "{case}: 조회 타임아웃"
                );
            }
        }
        assert_held(&w, "t6.2", 100, 900).await;

        // tx가 채굴되고 확정 수 N에 이른다
        w.settle(&mut p, scan::N as usize + 1).await;
        assert_eq!(w.status("t6.2").await, Status::Confirmed, "{case}");
        assert_eq!(w.entries("wd:settle:").await, 1, "{case}: 지급 분개 수");
        assert_eq!(
            w.entries("wd:release:").await,
            0,
            "{case}: 홀드 해제 분개 수"
        );
        assert_eq!(w.payments_to(to).len(), 1, "{case}: 체인 지급 수");
        assert_eq!(w.balance(&AccountKey::withdrawal_pending(1)).await, 0);
        w.env.db.drop_db().await;
    }
}

// T6.3 실패해야 할 변형: "nonce too low"에서 새 nonce로 다시 서명((가)에서 이중 지급, (나)에서 `NeedsReview`가 아님)
#[tokio::test]
async fn nonce_conflict_never_double_pays() {
    let w = WEnv::new().await;
    w.deposit(1, 1_000).await;

    // (가) 전송 직후 종료해 `Signed`로 남은 사이 내 tx가 채굴된다
    let a = withdraw::recipient(1);
    w.request("t6.3-a", 1, a, 100).await;
    w.env
        .faults
        .fail_next(FaultPoint::AfterSend, 1, Injected::Crash);
    let mut p = w.processor().await;
    assert!(matches!(
        p.run_until_idle().await,
        Err(WithdrawalError::Crashed(FaultPoint::AfterSend))
    ));
    assert_eq!(w.status("t6.3-a").await, Status::Signed);
    w.mine(scan::N as usize).await;
    assert_eq!(w.payments_to(a).len(), 1);
    // 재시작 뒤 같은 바이트를 다시 보내면 "nonce too low"
    let before = w.chain().received().len();
    let mut p = w.processor().await;
    assert_moved(p.step().await, Status::Signed, Status::Sent);
    w.settle(&mut p, scan::N as usize + 1).await;
    assert_eq!(w.payments_to(a).len(), 1, "(가) 체인 지급 수");
    assert_eq!(w.status("t6.3-a").await, Status::Confirmed, "(가)");
    let resent = &w.chain().received()[before..];
    assert_eq!(resent, [w.stored("t6.3-a").await.raw], "재시작 뒤 재전송");

    // (나) 외부 tx가 다음 nonce를 먼저 쓰고 채굴된다. 그 뒤 우리 출금이 같은 nonce로 서명된다
    let nonce = w.chain().pending_nonce(withdraw::hot());
    w.chain()
        .external_send(withdraw::hot(), nonce, scan::stranger(), 5);
    w.mine(scan::N as usize).await;
    let b = withdraw::recipient(2);
    w.request("t6.3-b", 1, b, 200).await;
    w.settle(&mut p, scan::N as usize + 1).await;
    assert_eq!(w.status("t6.3-b").await, Status::NeedsReview, "(나)");
    assert_held(&w, "t6.3-b", 200, 700).await;
    assert_eq!(w.payments_to(b).len(), 0, "(나) 우리 지급 수");
    assert_eq!(
        w.stored("t6.3-b").await.nonce,
        nonce,
        "외부 tx와 같은 nonce"
    );
    assert_eq!(w.signed_rows().await, 2, "새 서명 tx가 생겼다");
    w.env.db.drop_db().await;
}

// T6.4 실패해야 할 변형: 재전송 때마다 nonce를 새로 할당해 다시 서명
#[tokio::test]
async fn resend_uses_identical_raw_tx() {
    let w = WEnv::new().await;
    w.deposit(1, 1_000).await;
    let to = withdraw::recipient(1);
    w.request("t6.4", 1, to, 100).await;

    // 첫 전송은 반영하고 응답만 버린다 → `Unknown`
    w.chain().fail_next(Call::SendRaw, 1, Fault::LostResponse);
    let mut p = w.processor().await;
    assert_moved(p.step().await, Status::Held, Status::Signed);
    assert_moved(p.step().await, Status::Signed, Status::Unknown);
    // `Unknown`에서 여러 번 재전송: 두 번은 결과를 모르고, 그 뒤는 "already known"
    w.chain().fail_next(Call::SendRaw, 2, Fault::Indeterminate);
    for _ in 0..4 {
        p.run_until_idle().await.unwrap();
    }
    assert_eq!(w.status("t6.4").await, Status::Sent);
    w.settle(&mut p, scan::N as usize + 1).await;

    let stored = w.stored("t6.4").await;
    let received = w.chain().received();
    assert!(received.len() >= 4, "재전송 수 {}", received.len() - 1);
    assert!(
        received.iter().all(|r| *r == stored.raw),
        "노드가 받은 raw tx {}개 중 저장본과 다른 것이 있다",
        received.len()
    );
    assert_eq!(w.payments_to(to).len(), 1, "체인 지급 수");
    assert_eq!(w.status("t6.4").await, Status::Confirmed);
    w.env.db.drop_db().await;
}

fn assert_moved(got: Result<Step, WithdrawalError>, from: Status, to: Status) {
    match got {
        Ok(Step::Moved { from: f, to: t, .. }) if (f, t) == (from, to) => {}
        other => panic!("{from} → {to}이어야 한다, 받은 것 {other:?}"),
    }
}

/// 홀드가 유지된다: `withdrawal_pending:1` = 출금액, `user:1` = 남은 잔액, 해제·지급 분개 없음.
async fn assert_held(w: &WEnv, id: &str, amount: i128, user_left: i128) {
    assert_eq!(
        w.balance(&AccountKey::withdrawal_pending(1)).await,
        amount,
        "{id}: 홀드"
    );
    assert_eq!(
        w.balance(&AccountKey::user(1)).await,
        user_left,
        "{id}: user:1"
    );
    assert_eq!(
        w.entries(&format!("wd:release:{id}")).await,
        0,
        "{id}: 홀드 해제"
    );
}

// T6.5 실패해야 할 변형: 조회 실패 블록을 빈 블록으로 보고 커서 전진
#[tokio::test]
async fn read_timeout_never_skips_block() {
    let env = Env::new().await;
    let chain = &env.chain;
    // genesis → b1(빈 블록)까지 스캔한 뒤 b2(입금) → b3가 보인다
    let b1 = chain.mine_canonical(chain.genesis().hash, vec![]);
    let mut scanner = env.scanner().await;
    scanner.run_until_idle().await.unwrap();
    let b2 = chain.mine_canonical(b1, vec![transfer(1, scan::addr(1), 100)]);
    chain.mine_canonical(b2, vec![]);
    let before = env.cursor().await;
    assert_eq!(before.hash, b1);

    // 입금 블록 조회: 타임아웃 2번, 빈 응답(아직 없음) 2번, head 조회 타임아웃 1번
    chain.fail_next(Call::BlockAt(BlockNumber(2)), 2, Fault::Indeterminate);
    chain.none_at(BlockNumber(2), 2);
    chain.fail_next(Call::Head, 1, Fault::Indeterminate);
    let head_timeout = scanner.step().await;
    assert!(
        matches!(head_timeout, Err(ScanError::Indeterminate(_))),
        "head 타임아웃: {head_timeout:?}"
    );
    assert_eq!(env.cursor().await, before, "head 타임아웃 뒤 커서");
    for i in 0..2 {
        let got = scanner.step().await;
        assert!(
            matches!(got, Err(ScanError::Indeterminate(_))),
            "{i}번째 블록 조회 타임아웃: {got:?}"
        );
        assert_eq!(env.cursor().await, before, "{i}번째 타임아웃 뒤 커서");
    }
    for i in 0..2 {
        let got = scanner.step().await;
        assert!(
            matches!(got, Ok(vault::deposit::Step::Idle)),
            "{i}번째 빈 응답: {got:?}"
        );
        assert_eq!(env.cursor().await, before, "{i}번째 빈 응답 뒤 커서");
    }
    assert_eq!(env.balance(&AccountKey::pending_deposit(1)).await, 0);
    assert_eq!(env.entries("dep:").await, 0, "조회 실패 중 분개가 생겼다");

    // 조회가 회복되면 입금이 한 번 반영된다
    scanner.run_until_idle().await.unwrap();
    assert_eq!(env.cursor().await.hash, chain.tip().hash);
    assert_eq!(env.balance(&AccountKey::pending_deposit(1)).await, 100);
    assert_eq!(env.entries("dep:pending:").await, 1, "잠정 입금 분개 수");
    assert_eq!(env.entries("dep:revert:").await, 0, "역분개 수");
    env.db.drop_db().await;
}

// T6.6 실패해야 할 변형: T6.1, T6.3, T6.4의 변형 중 어느 하나
#[test]
fn random_send_faults_pay_at_most_once() {
    let rt = model::runtime();
    let mut runner = TestRunner::new(model::config(24));
    runner
        .run(&vec(op(), 1..24), |ops| {
            rt.block_on(pay_at_most_once(ops))
                .map_err(TestCaseError::fail)
        })
        .unwrap();
}

/// T6.6의 걸음. 장애는 다음 호출(또는 다음 통과) 하나에 걸린다.
#[derive(Debug, Clone)]
enum Op {
    /// 새 출금 요청(금액)
    Request(u128),
    /// 처리기를 돌린다. 주입된 종료가 나면 처리기를 새로 만든다(재시작)
    Process,
    /// 블록을 캐고 스캔한다
    Mine(usize),
    /// 다음 전송: 반영하고 응답만 버림
    LostSend,
    /// 다음 전송: 노드에 닿지 않고 타임아웃
    SendTimeout,
    /// 다음 전송: 노드가 거절("insufficient funds" 같은 그 밖의 거절)
    SendRejected,
    /// 다음 n번 영수증 조회 타임아웃
    ReceiptTimeout(usize),
    /// 다음 계정 nonce 조회 타임아웃
    NonceTimeout,
    /// 외부에서 같은 키로 다음 nonce를 쓴다(F5)
    External,
    /// 다음 전송 직후 종료
    CrashAfterSend,
    /// 다음 출금 커밋 직후 종료
    CrashAfterCommit,
    /// 다음 출금 커밋 직전 종료
    CrashBeforeCommit,
    /// 가짜 시계를 기한 너머로
    PastDeadline,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (1u128..=50).prop_map(Op::Request),
        4 => Just(Op::Process),
        2 => (1usize..=3).prop_map(Op::Mine),
        2 => Just(Op::LostSend),
        1 => Just(Op::SendTimeout),
        1 => Just(Op::SendRejected),
        1 => (1usize..=3).prop_map(Op::ReceiptTimeout),
        1 => Just(Op::NonceTimeout),
        1 => Just(Op::External),
        1 => Just(Op::CrashAfterSend),
        1 => Just(Op::CrashAfterCommit),
        1 => Just(Op::CrashBeforeCommit),
        1 => Just(Op::PastDeadline),
    ]
}

async fn pay_at_most_once(ops: Vec<Op>) -> Result<(), String> {
    let w = WEnv::new().await;
    let result = drive(&w, &ops).await;
    w.env.db.drop_db().await;
    result
}

async fn drive(w: &WEnv, ops: &[Op]) -> Result<(), String> {
    w.deposit(1, 1_000_000).await;
    let chain = w.chain();
    let mut p = w.processor().await;
    let mut requests = 0u64;
    for op in ops {
        match op {
            Op::Request(a) => {
                requests += 1;
                w.request(
                    &format!("t6.6-{requests}"),
                    1,
                    withdraw::recipient(requests),
                    *a,
                )
                .await;
            }
            Op::Process => {
                withdraw::run_restarting(w, &mut p)
                    .await
                    .map_err(|e| format!("{op:?}: {e}"))?;
            }
            Op::Mine(n) => w.mine(*n).await,
            Op::LostSend => chain.fail_next(Call::SendRaw, 1, Fault::LostResponse),
            Op::SendTimeout => chain.fail_next(Call::SendRaw, 1, Fault::Indeterminate),
            Op::SendRejected => chain.fail_next(Call::SendRaw, 1, Fault::Definite),
            Op::ReceiptTimeout(n) => chain.fail_next(Call::Receipt, *n, Fault::Indeterminate),
            Op::NonceTimeout => chain.fail_next(Call::NonceAt, 1, Fault::Indeterminate),
            Op::External => {
                let nonce = chain.pending_nonce(withdraw::hot());
                chain.external_send(withdraw::hot(), nonce, scan::stranger(), 1);
            }
            Op::CrashAfterSend => w
                .env
                .faults
                .fail_next(FaultPoint::AfterSend, 1, Injected::Crash),
            Op::CrashAfterCommit => w.env.faults.fail_next(
                FaultPoint::AfterCommit(Site::Withdrawal),
                1,
                Injected::Crash,
            ),
            Op::CrashBeforeCommit => w.env.faults.fail_next(
                FaultPoint::BeforeCommit(Site::Withdrawal),
                1,
                Injected::Crash,
            ),
            Op::PastDeadline => w
                .clock
                .advance(withdraw::REVIEW_AFTER + Duration::from_secs(1)),
        }
    }
    // 끝: 장애 없이 블록을 캐고 처리해 들어갈 수 있는 tx는 모두 확정 수 N에 이르게 한다
    for _ in 0..(scan::N as usize + 2) {
        withdraw::run_restarting(w, &mut p)
            .await
            .map_err(|e| format!("마무리: {e}"))?;
        w.mine(1).await;
    }
    withdraw::run_restarting(w, &mut p)
        .await
        .map_err(|e| format!("마무리: {e}"))?;

    let mut paid = 0i128;
    for n in 1..=requests {
        let payments = w.payments_to(withdraw::recipient(n));
        if payments.len() > 1 {
            return Err(format!("출금 {n}: 체인 지급 {}회", payments.len()));
        }
        paid += payments.iter().map(|t| t.value as i128).sum::<i128>();
    }
    let settled = w.settled_amount().await;
    if settled != paid {
        return Err(format!("원장 출금액 {settled}, 체인 지급액 {paid}"));
    }
    Ok(())
}
