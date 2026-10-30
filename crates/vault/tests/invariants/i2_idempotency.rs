//! I2 멱등성: 같은 입금 이벤트나 출금 요청을 여러 번 처리해도 자산이 중복 반영되지 않는다

// T2.1 실패해야 할 변형: 멱등 키 유일 제약과 기존 키 조회 제거
#[test]
#[ignore = "02-ledger"]
fn same_key_reposted_changes_nothing() {
    unimplemented!(
        "T2.1: 임의 분개 열에 같은 키 재기록을 무작위 위치로 끼움(proptest). 단언: 잔액·분개 수가 중복 없는 열과 같고, 재기록은 기존 분개 ID를 돌려준다"
    );
}

// T2.2 실패해야 할 변형: 기존 키가 있으면 내용 비교 없이 성공 반환
#[test]
#[ignore = "02-ledger"]
fn same_key_different_body_rejected() {
    unimplemented!(
        "T2.2: 같은 키, 다른 금액·계정의 분개. 단언: `IdempotencyConflict` 오류, 기존 분개와 잔액 불변"
    );
}

// T2.3 실패해야 할 변형: 입금 키를 tx 해시 대신 수신 순번으로 만듦((나)의 재처리가 원장까지 닿아 두 번 기록된다)
#[test]
#[ignore = "03-deposit-reorg"]
fn duplicate_block_events_credit_once() {
    unimplemented!(
        "T2.3: (가) 같은 블록을 2~3번 전달 (나) 블록 커밋 직후 \"결과 모름\" 오류를 주입해 같은 프로세스가 그 블록을 다시 처리 (다) 재시작 후 커서 직전 블록 재전달. 단언: 입금마다 `pending_deposit:u`·`user:u`에 한 번씩만 반영"
    );
}

// T2.4 실패해야 할 변형: `withdrawals.request_id` 유일 제약과 기존 요청 조회를 제거(홀드 키는 그대로 두어도 두 번째 출금 행이 따로 서명·지급된다)
#[test]
#[ignore = "05-withdrawal"]
fn retried_withdrawal_request_holds_once() {
    unimplemented!(
        "T2.4: 같은 출금 요청 ID를 여러 번 제출(재시작 사이에도). 단언: 출금 행 1개, 홀드 분개 1개, 체인 지급 1회"
    );
}
