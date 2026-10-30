//! I5 복구 가능성: 어떤 처리 단계에서 프로세스가 종료되어도 재시작 후 일관된 상태로 복구한다

// T5.1 실패해야 할 변형: 커서 전진을 원장 반영보다 먼저 별도 트랜잭션으로 커밋("커서 커밋 직후" 지점에서 죽으면 그 블록의 입금이 빠진다)
#[test]
#[ignore = "04-crash-recovery"]
fn crash_at_each_injection_point_matches_clean_run() {
    unimplemented!(
        "T5.1: 주입 지점(이벤트 수신 직후, 각 DB 커밋 직전, 각 DB 커밋 직후)마다 종료 후 재시작. 단언: 최종 원장·커서·`deposits`가 장애 없는 실행과 같다"
    );
}

// T5.2 실패해야 할 변형: DB 오류를 "처리할 수 없는 블록"으로 분류해 재시도하지 않고, DB 커서를 그 블록으로 직접 전진
#[test]
#[ignore = "04-crash-recovery"]
fn transient_db_error_leaves_no_partial_state() {
    unimplemented!(
        "T5.2: 블록 처리의 분개 기록 중간과 커밋에서 일시 DB 오류 주입, 재시작 없이 같은 프로세스가 재시도. 단언: 그 블록의 입금이 정확히 한 번 반영되고 최종 상태가 장애 없는 실행과 같다"
    );
}

// T5.3 실패해야 할 변형: 재시작 때 `Signed` 출금에 nonce를 새로 할당해 다시 서명(같은 nonce로 다시 서명하면 결정적 서명이라 같은 바이트가 나와 변형이 안 된다)
#[test]
#[ignore = "05-withdrawal"]
fn restart_after_sign_resends_same_tx() {
    unimplemented!(
        "T5.3: 서명 tx 커밋 직후, 전송 직후 종료 후 재시작. 단언: 출금마다 체인 지급 1회, 보낸 raw tx가 저장본과 같다"
    );
}
