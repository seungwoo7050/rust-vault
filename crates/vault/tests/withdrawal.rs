//! 불변조건 표 밖의 출금 테스트(`--features db`). 확정된 출금의 깊은 reorg, `NeedsReview` 뒤의 결론,
//! 영수증 status 0의 가스비·홀드 해제, 그 밖의 전송 거절.

#[path = "support/model.rs"]
mod model;
#[path = "support/scan.rs"]
mod scan;
#[path = "support/withdraw.rs"]
mod withdraw;

use std::time::Duration;

use vault::ledger::AccountKey;
use vault::withdrawal::{Status, Step, WithdrawalError};
use vault_sim::chain::{Call, Fault};

use withdraw::WEnv;

/// (가) 확정된 출금의 tx가 N보다 깊은 reorg로 빠진다(ADR-003 결정 6).
/// 원장은 그대로이고, 사고 기록(`kind = withdrawal`)이 남고, 스캐너와 출금 처리기가 멈춘다.
/// 멈춘 처리기는 새 출금을 서명하지 않고 체인에 아무것도 보내지 않는다.
#[tokio::test]
async fn deep_reorg_of_confirmed_withdrawal_halts() {
    let w = WEnv::new().await;
    w.deposit(1, 1_000).await;
    let to = withdraw::recipient(1);
    let fork_base = w.chain().tip().hash;
    w.request("deep", 1, to, 100).await;
    let mut p = w.processor().await;
    w.settle(&mut p, scan::N as usize + 1).await;
    assert_eq!(w.status("deep").await, Status::Confirmed);
    let keys = w.env.keys().await;
    let balances = w.env.balances().await;

    // 출금 tx가 든 블록 아래에서 갈라진, N보다 긴 분기. 출금 tx는 멤풀에 없어 새 분기에 들어가지 않는다
    scan::extend(w.chain(), fork_base, scan::N as usize + 3);
    assert!(w.payments_to(to).is_empty(), "새 분기에 출금 tx가 있다");
    let scanned = w.env.scanner().await.run_until_idle().await;
    assert!(
        matches!(scanned, Err(vault::deposit::ScanError::Halted(_))),
        "스캐너 정지: {scanned:?}"
    );
    let violations = w.env.violations().await;
    assert_eq!(violations.len(), 1, "사고 기록 {violations:?}");
    assert_eq!(violations[0].0, "withdrawal");
    assert_eq!(violations[0].2, w.stored("deep").await.hash.to_string());
    assert_eq!(violations[0].3, "100");
    assert_eq!(w.env.keys().await, keys, "분개가 바뀌었다");
    assert_eq!(w.env.balances().await, balances, "잔액이 바뀌었다");

    // 출금 처리기도 멈춘다: 새 요청은 받지만(홀드) 서명·전송은 하지 않는다
    assert_eq!(w.request("after", 1, to, 10).await.status, Status::Held);
    let sent = w.chain().received().len();
    for _ in 0..2 {
        let mut p = w.processor().await;
        let got = p.step().await;
        assert!(matches!(got, Err(WithdrawalError::Halted(_))), "{got:?}");
        assert!(matches!(
            p.run_until_idle().await,
            Err(WithdrawalError::Halted(_))
        ));
    }
    assert_eq!(w.chain().received().len(), sent, "정지 중에 보냈다");
    assert_eq!(w.status("after").await, Status::Held);
    assert_eq!(w.status("deep").await, Status::Confirmed);
    w.env.db.drop_db().await;
}

/// (나) 기한이 지나 `NeedsReview`가 된 뒤 영수증이 나오면 결론 낸다(사람 결정 (a)).
/// status 1이면 `Confirmed`, status 0이면 `Failed`.
#[tokio::test]
async fn needs_review_concludes_on_receipt() {
    let w = WEnv::new().await;
    w.deposit(1, 1_000).await;
    let ok = withdraw::recipient(1);
    let bad = withdraw::recipient(2);
    w.chain().revert_to(bad);
    w.request("nr-ok", 1, ok, 100).await;
    w.request("nr-bad", 1, bad, 200).await;

    // 둘 다 응답 유실 → `Unknown`. 채굴 전에 기한이 지나 `NeedsReview`
    w.chain().fail_next(Call::SendRaw, 2, Fault::LostResponse);
    let mut p = w.processor().await;
    p.run_until_idle().await.unwrap();
    w.clock
        .advance(withdraw::REVIEW_AFTER + Duration::from_secs(1));
    p.run_until_idle().await.unwrap();
    for id in ["nr-ok", "nr-bad"] {
        assert_eq!(w.status(id).await, Status::NeedsReview, "{id}");
    }
    assert_eq!(w.balance(&AccountKey::withdrawal_pending(1)).await, 300);

    w.settle(&mut p, scan::N as usize + 1).await;
    assert_eq!(w.status("nr-ok").await, Status::Confirmed);
    assert_eq!(w.status("nr-bad").await, Status::Failed);
    assert_eq!(w.balance(&AccountKey::withdrawal_pending(1)).await, 0);
    assert_eq!(w.balance(&AccountKey::user(1)).await, 900);
    w.env.db.drop_db().await;
}

/// (다) 영수증 status 0: `wd:fee` + `wd:release`. 사용자 잔액은 요청 전으로 돌아오고,
/// 가스비는 `network_fee`로 가고 핫월렛에서 빠진다. 성공한 출금의 가스비도 같이 본다.
#[tokio::test]
async fn failed_receipt_posts_fee_and_release() {
    let w = WEnv::new().await;
    w.deposit(1, 1_000).await;
    let bad = withdraw::recipient(1);
    let ok = withdraw::recipient(2);
    w.chain().revert_to(bad);
    w.request("fail", 1, bad, 300).await;
    let mut p = w.processor().await;
    w.settle(&mut p, scan::N as usize + 1).await;

    assert_eq!(w.status("fail").await, Status::Failed);
    let hash = w.stored("fail").await.hash;
    assert_eq!(
        w.entries(&format!("wd:fee:{}:{hash}", scan::CHAIN_ID))
            .await,
        1
    );
    assert_eq!(w.entries("wd:release:fail").await, 1);
    assert_eq!(w.entries("wd:settle:").await, 0);
    assert_eq!(
        w.balance(&AccountKey::user(1)).await,
        1_000,
        "사용자 잔액 복구"
    );
    assert_eq!(w.balance(&AccountKey::withdrawal_pending(1)).await, 0);
    assert_eq!(w.balance(&AccountKey::network_fee()).await, withdraw::FEE);
    let capital = withdraw::CAPITAL as i128;
    assert_eq!(
        w.balance(&AccountKey::hot_wallet(withdraw::hot())).await,
        capital - withdraw::FEE
    );

    // 같은 사용자의 성공 출금: 금액 + 가스비가 핫월렛에서 빠진다
    w.request("ok", 1, ok, 400).await;
    w.settle(&mut p, scan::N as usize + 1).await;
    assert_eq!(w.status("ok").await, Status::Confirmed);
    assert_eq!(w.balance(&AccountKey::user(1)).await, 600);
    assert_eq!(
        w.balance(&AccountKey::network_fee()).await,
        2 * withdraw::FEE
    );
    assert_eq!(
        w.balance(&AccountKey::hot_wallet(withdraw::hot())).await,
        capital - 400 - 2 * withdraw::FEE
    );
    assert_eq!(w.env.trial_balance().await, 0);
    w.env.db.drop_db().await;
}

/// (라) 그 밖의 `Definite` 전송 거절도 `Unknown`이다(ADR-003 결정 2). 홀드는 유지하고,
/// 조회 루틴의 재전송이 받아들여지면 `Sent`를 거쳐 확정된다.
#[tokio::test]
async fn definite_other_send_error_moves_to_unknown() {
    let w = WEnv::new().await;
    w.deposit(1, 1_000).await;
    let to = withdraw::recipient(1);
    w.request("other", 1, to, 100).await;

    w.chain().fail_next(Call::SendRaw, 1, Fault::Definite);
    let mut p = w.processor().await;
    assert!(matches!(
        p.step().await,
        Ok(Step::Moved {
            to: Status::Signed,
            ..
        })
    ));
    let got = p.step().await;
    assert!(
        matches!(
            got,
            Ok(Step::Moved {
                from: Status::Signed,
                to: Status::Unknown,
                ..
            })
        ),
        "{got:?}"
    );
    assert!(
        w.chain().pool().is_empty(),
        "노드가 거절한 tx가 멤풀에 있다"
    );
    assert_eq!(w.balance(&AccountKey::withdrawal_pending(1)).await, 100);
    assert_eq!(w.entries("wd:release:").await, 0);

    let got = p.step().await;
    assert!(
        matches!(
            got,
            Ok(Step::Moved {
                from: Status::Unknown,
                to: Status::Sent,
                ..
            })
        ),
        "재전송이 받아들여지면 Sent: {got:?}"
    );
    w.settle(&mut p, scan::N as usize + 1).await;
    assert_eq!(w.status("other").await, Status::Confirmed);
    assert_eq!(w.payments_to(to).len(), 1);
    w.env.db.drop_db().await;
}
