//! I6 외부 불확실성: RPC 타임아웃이나 응답 유실을 곧바로 트랜잭션 실패로 판단하지 않는다

use vault::chain::BlockNumber;
use vault::deposit::{ScanError, Step};
use vault::ledger::AccountKey;
use vault_sim::chain::{Call, Fault, transfer};

use crate::scan::{self, Env};

// T6.1 실패해야 할 변형: 타임아웃을 `Failed`로 매핑하고 홀드 해제
#[test]
#[ignore = "05-withdrawal"]
fn send_timeout_moves_to_unknown() {
    unimplemented!(
        "T6.1: 전송 요청은 반영하고 응답만 버림. 단언: 상태 `Unknown`, 홀드 유지, 새 서명 tx 0개"
    );
}

// T6.2 실패해야 할 변형: 기한이 지나면 `Unknown`·`NeedsReview`를 `Failed`로 바꾸고 홀드 해제((나)에서 깨진다)
#[test]
#[ignore = "05-withdrawal"]
fn lost_response_resolved_by_receipt() {
    unimplemented!(
        "T6.2: 응답 유실 뒤 조회가 몇 번 타임아웃되고 tx가 채굴됨. (가) 기한 전에 채굴 (나) 가짜 시계를 기한 너머로 돌려 `NeedsReview`가 된 뒤 채굴. 단언: 둘 다 `Confirmed`, 지급 분개 1개, 홀드 해제 분개 0개"
    );
}

// T6.3 실패해야 할 변형: "nonce too low"에서 새 nonce로 다시 서명((가)에서 이중 지급, (나)에서 `NeedsReview`가 아님)
#[test]
#[ignore = "05-withdrawal"]
fn nonce_conflict_never_double_pays() {
    unimplemented!(
        "T6.3: (가) 전송 직후 종료해 `Signed`로 남은 사이 내 tx가 채굴되고, 재시작 뒤 재전송이 \"nonce too low\"를 받음 (나) 외부 tx가 같은 nonce를 먼저 씀. 단언: (가) 지급 1회, `Confirmed` (나) `NeedsReview`, 홀드 유지, 우리 지급 0회"
    );
}

// T6.4 실패해야 할 변형: 재전송 때마다 nonce를 새로 할당해 다시 서명
#[test]
#[ignore = "05-withdrawal"]
fn resend_uses_identical_raw_tx() {
    unimplemented!(
        "T6.4: 첫 전송은 반영하고 응답만 버린 뒤 `Unknown`에서 여러 번 재전송. 단언: 노드가 받은 raw tx가 모두 저장본과 같고 지급 1회"
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
        assert!(matches!(got, Ok(Step::Idle)), "{i}번째 빈 응답: {got:?}");
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
#[ignore = "05-withdrawal"]
fn random_send_faults_pay_at_most_once() {
    unimplemented!(
        "T6.6: 응답 유실·타임아웃·nonce 충돌·재시작을 무작위로 섞은 출금 열(proptest). 출금 요청마다 체인 지급 수 ≤ 1, 원장 출금액 = 체인 지급액"
    );
}
