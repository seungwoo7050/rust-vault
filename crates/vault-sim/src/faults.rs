//! 장애 주입 지점 구현. 지점 이름마다 "다음 n번 실패"를 걸어 둔다.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use vault::store::{Faults, Injected};

/// 걸어 둔 장애 계획. 핸들(`Clone`)을 스캐너에 넘기고 테스트가 다른 핸들로 계획을 바꾼다.
#[derive(Debug, Clone, Default)]
pub struct FaultPlan(Arc<Mutex<HashMap<&'static str, usize>>>);

impl FaultPlan {
    pub fn new() -> Self {
        Self::default()
    }

    /// `point`의 커밋 직후 다음 `n`번을 "결과 모름" 오류로 끝낸다(커밋은 실제로 된다).
    pub fn fail_after_commit(&self, point: &'static str, n: usize) {
        *self.0.lock().unwrap().entry(point).or_default() += n;
    }
}

impl Faults for FaultPlan {
    fn after_commit(&self, point: &'static str) -> Result<(), Injected> {
        let mut plan = self.0.lock().unwrap();
        match plan.get_mut(point) {
            Some(left) if *left > 0 => {
                *left -= 1;
                Err(Injected(point))
            }
            _ => Ok(()),
        }
    }
}
