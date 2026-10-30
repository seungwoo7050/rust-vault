//! mutants.sh 자체 테스트용. 출금 한 번의 잔액 계산에 검사가 두 겹이다.
//! 변형 D1(0원 검사 제거)은 테스트가 잡고, D2(잔액 검사 제거)는 둘째 겹인 checked_sub에 가려진다.

#[derive(Debug, PartialEq, Eq)]
pub enum Rejected {
    ZeroAmount,
    Overdraft,
}

pub fn withdraw(balance: u64, amount: u64) -> Result<u64, Rejected> {
    if amount == 0 {
        return Err(Rejected::ZeroAmount);
    }
    // 첫 겹: 잔액보다 큰 출금을 거부한다
    if amount > balance {
        return Err(Rejected::Overdraft);
    }
    // 둘째 겹: 뺄셈도 음수가 되면 거부한다
    balance.checked_sub(amount).ok_or(Rejected::Overdraft)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_amount_rejected() {
        assert_eq!(withdraw(10, 0), Err(Rejected::ZeroAmount));
    }

    #[test]
    fn overdraft_rejected() {
        assert_eq!(withdraw(10, 11), Err(Rejected::Overdraft));
        assert_eq!(withdraw(10, 10), Ok(0));
    }
}
