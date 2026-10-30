//! I6 외부 불확실성: RPC 타임아웃이나 응답 유실을 곧바로 트랜잭션 실패로 판단하지 않는다

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
#[test]
#[ignore = "03-deposit-reorg"]
fn read_timeout_never_skips_block() {
    unimplemented!(
        "T6.5: 블록 조회 타임아웃·빈 응답을 입금 블록에 주입. 단언: 커서가 그 블록 앞에 머물고, 조회가 회복되면 입금이 한 번 반영되고, 역분개 0개"
    );
}

// T6.6 실패해야 할 변형: T6.1, T6.3, T6.4의 변형 중 어느 하나
#[test]
#[ignore = "05-withdrawal"]
fn random_send_faults_pay_at_most_once() {
    unimplemented!(
        "T6.6: 응답 유실·타임아웃·nonce 충돌·재시작을 무작위로 섞은 출금 열(proptest). 출금 요청마다 체인 지급 수 ≤ 1, 원장 출금액 = 체인 지급액"
    );
}
