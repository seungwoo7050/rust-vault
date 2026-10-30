//! 시각(`Clock`). `NeedsReview` 기한이 이것으로 잰다(ADR-003 결정 3).
//! 운영은 `SystemClock`, 테스트는 `vault-sim`의 가짜 시계를 넣어 기한 전후를 만든다.

use std::time::SystemTime;

pub trait Clock {
    fn now(&self) -> SystemTime;
}

/// 운영 시계.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}
