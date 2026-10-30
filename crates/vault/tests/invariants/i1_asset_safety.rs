//! I1 자산 안전성: 사용자 잔액은 승인된 원장 거래 없이 변경될 수 없다

// T1.1 실패해야 할 변형: 분개 기록에서 savepoint를 없앰(없는 계정의 FK 위반이 트랜잭션 전체를 중단시키고, 그 트랜잭션의 커밋은 롤백으로 끝나 정상 분개까지 사라진다)
#[test]
#[ignore = "02-ledger"]
fn rejected_entry_changes_no_balance() {
    unimplemented!(
        "T1.1: 한 DB 트랜잭션 안에서 정상 분개와 거부될 분개(없는 계정, 차대 불일치, 사용자 잔액 부족)를 섞은 임의 열을 기록한 뒤 그 트랜잭션을 커밋(proptest). 단언: 커밋이 성공하고, 계정 잔액이 정상 분개만으로 계산한 모델과 같다"
    );
}

// T1.2 실패해야 할 변형: 잠정 입금을 사용자 계정에 바로 대변 기록
#[test]
#[ignore = "03-deposit-reorg"]
fn pending_deposit_not_spendable() {
    unimplemented!(
        "T1.2: 확정 수 N−1인 입금 직후 같은 금액의 출금 홀드 요청. 단언: 요청 `Rejected`, `user:u` = 0, 확정 뒤 같은 금액 요청은 `Held`"
    );
}

// T1.3 실패해야 할 변형: 홀드 분개의 사용자 잔액 음수 검사 제거
#[test]
#[ignore = "05-withdrawal"]
fn hold_rejects_overspend() {
    unimplemented!(
        "T1.3: 잔액보다 큰 출금, 잔액을 나눠 쓰는 동시 출금 두 건. 단언: 큰 출금 `Rejected`, 동시 두 건 중 잔액 안의 것만 `Held`, `user:u` ≥ 0"
    );
}
