//! 가짜 시계. 테스트가 앞으로 돌려 `NeedsReview` 기한 전후를 만든다.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use vault::clock::Clock;

/// 핸들(`Clone`)을 처리기에 넘기고 테스트가 다른 핸들로 시각을 옮긴다. 저절로 가지 않는다.
#[derive(Debug, Clone)]
pub struct FakeClock(Arc<Mutex<SystemTime>>);

impl Default for FakeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeClock {
    /// 고정된 시각(2026-01-01T00:00:00Z)에서 시작한다.
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(
            UNIX_EPOCH + Duration::from_secs(1_767_225_600),
        )))
    }

    pub fn advance(&self, by: Duration) {
        *self.0.lock().unwrap() += by;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> SystemTime {
        *self.0.lock().unwrap()
    }
}
