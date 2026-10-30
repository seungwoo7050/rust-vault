//! I3 원장 정합성: 모든 회계 거래는 차변과 대변이 일치한다

// T3.1 실패해야 할 변형: 분개 줄 기록 루프가 마지막 줄을 빠뜨림(커밋 트리거가 거부해 "모두 성공" 단언이 깨진다)
#[test]
#[ignore = "02-ledger"]
fn every_posted_entry_sums_to_zero() {
    unimplemented!(
        "T3.1: 정상 분개만으로 된 임의 열(proptest). 단언: 모든 기록이 성공하고, 분개별 줄 합 0, 시산 0, 계정 잔액이 모델 계산과 같다"
    );
}

// T3.2 실패해야 할 변형: 커밋 시점 제약 트리거 제거
#[test]
#[ignore = "02-ledger"]
fn db_rejects_unbalanced_entry() {
    unimplemented!(
        "T3.2: Rust 검증을 거치지 않고 SQL로 차대 불일치 분개 삽입 후 커밋. 단언: 커밋이 제약 위반으로 거부되고, 그 분개와 줄이 남지 않으며 시산 0"
    );
}

// T3.3 실패해야 할 변형: 역분개가 잠정 입금 계정 줄만 쓰고 핫월렛 줄을 빠뜨림(거부되어 스캔이 멈춘다)
#[test]
#[ignore = "03-deposit-reorg"]
fn trial_balance_zero_after_reorgs() {
    unimplemented!(
        "T3.3: 입금·reorg·재포함·확정이 섞인 임의 체인 이력. 단언: 스캔이 오류 없이 head까지 가고, 시산 0, 고아 블록의 잠정 입금 잔액 0, `hot_wallet:{{입금 주소}}` 잔액 = 가짜 체인 canonical 입금 합"
    );
}
