//! 장애 주입 지점 구현. 지점마다 통과 횟수를 세고, "몇 번째 통과에 어떤 장애"를 걸어 둔다.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use vault::store::{FaultPoint, Faults, Injected, Site};

/// 걸어 둔 장애 계획. 핸들(`Clone`)을 스캐너에 넘기고 테스트가 다른 핸들로 계획을 바꾸거나 통과 횟수를 읽는다.
/// 재시작 뒤 새 스캐너에 같은 핸들을 넘기면 통과 횟수는 이어서 센다.
#[derive(Debug, Clone, Default)]
pub struct FaultPlan(Arc<Mutex<Inner>>);

#[derive(Debug, Default)]
struct Inner {
    /// 지점별 통과 횟수(장애를 낸 통과도 센다)
    hits: BTreeMap<FaultPoint, usize>,
    /// (지점, 몇 번째 통과(1부터)) → 장애
    at: BTreeMap<(FaultPoint, usize), Injected>,
    /// 실제로 장애를 낸 횟수
    fired: usize,
}

impl FaultPlan {
    pub fn new() -> Self {
        Self::default()
    }

    /// `point`의 다음 `n`번 통과를 `fault`로 끝낸다.
    pub fn fail_next(&self, point: FaultPoint, n: usize, fault: Injected) {
        let mut g = self.0.lock().unwrap();
        let done = g.hits.get(&point).copied().unwrap_or(0);
        for k in done + 1..=done + n {
            g.at.insert((point, k), fault);
        }
    }

    /// `point`의 `k`번째 통과(처음부터 센다, 1부터)를 `fault`로 끝낸다.
    pub fn fail_at(&self, point: FaultPoint, k: usize, fault: Injected) {
        self.0.lock().unwrap().at.insert((point, k), fault);
    }

    /// 블록 커밋 직후 다음 `n`번을 "결과 모름" 오류로 끝낸다(커밋은 실제로 된다).
    pub fn fail_after_block_commit(&self, n: usize) {
        self.fail_next(
            FaultPoint::AfterCommit(Site::Block),
            n,
            Injected::CommitOutcomeUnknown,
        );
    }

    /// 지금까지 실제로 장애를 낸 횟수(걸어 두고 아직 통과하지 않은 것은 세지 않는다).
    pub fn fired(&self) -> usize {
        self.0.lock().unwrap().fired
    }

    /// 지금까지의 지점별 통과 횟수.
    pub fn hits(&self) -> BTreeMap<FaultPoint, usize> {
        self.0.lock().unwrap().hits.clone()
    }
}

impl Faults for FaultPlan {
    fn hit(&self, point: FaultPoint) -> Result<(), Injected> {
        let mut g = self.0.lock().unwrap();
        let k = {
            let n = g.hits.entry(point).or_default();
            *n += 1;
            *n
        };
        match g.at.remove(&(point, k)) {
            Some(fault) => {
                g.fired += 1;
                Err(fault)
            }
            None => Ok(()),
        }
    }
}
