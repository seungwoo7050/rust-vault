//! I4 블록체인 정합성: 입금 확정 여부는 현재 canonical chain과 확정 정책에 따라 결정한다

// T4.1 실패해야 할 변형: reorg 감지에서 부모 해시 비교를 빼고 블록 번호만 봄
#[test]
#[ignore = "03-deposit-reorg"]
fn reorg_before_confirmation_reverses_pending() {
    unimplemented!(
        "T4.1: 입금 블록을 포함한 깊이 1~N−1의 분기 교체. 단언: 그 입금의 역분개 1개, `pending_deposit:u` = 0(새 분기에 그 tx가 없을 때), `user:u` 불변, 커서 = 새 분기 head"
    );
}

// T4.2 실패해야 할 변형: 확정된 입금도 역분개
#[test]
#[ignore = "03-deposit-reorg"]
fn reorg_beyond_confirmation_keeps_ledger_and_halts() {
    unimplemented!(
        "T4.2: 확정된 입금 블록을 빼는 깊이 N 이상의 reorg. 단언: 분개 수·모든 잔액 불변, `finality_violations` 1행, 스캐너 정지"
    );
}

// T4.3 실패해야 할 변형: 잠정 입금 키에서 블록 해시를 뺌
#[test]
#[ignore = "03-deposit-reorg"]
fn reincluded_tx_confirms_once() {
    unimplemented!(
        "T4.3: reorg로 빠진 tx가 새 분기의 다른 블록에 다시 들어가고 확정 수 N에 이름. 단언: 스캔이 오류 없이 head까지 가고, `user:u` += 금액 한 번, `pending_deposit:u` = 0, `hot_wallet:{{입금 주소}}` = canonical 입금 합"
    );
}

// T4.4 실패해야 할 변형: 확정 수를 받은 블록 개수(고아 포함)로 셈
#[test]
#[ignore = "03-deposit-reorg"]
fn confirmation_counts_canonical_depth() {
    unimplemented!(
        "T4.4: 고아 블록이 섞인 상태에서 확정 수 N−1, N 경계. 단언: N−1에서 확정 분개 0개, N에서 1개"
    );
}

// T4.5 실패해야 할 변형: 알림으로 받은 블록을 "부모 해시 = 커서 해시" 검사 없이 바로 처리(끌어오기 대신 밀어넣기)
#[test]
#[ignore = "03-deposit-reorg"]
fn event_order_does_not_change_final_ledger() {
    unimplemented!(
        "T4.5: 같은 블록 집합의 새 head 알림을 임의 순서·중복으로 전달(proptest), reorg 없음. 단언: 전달마다 저장 헤더가 부모 해시로 이어진 한 줄이고 커서는 1씩만 전진, 역분개 0개, 확정 분개는 그 블록 위로 N−1개가 처리된 뒤에만 생김, 최종 원장이 순서대로 전달한 실행과 같다"
    );
}
