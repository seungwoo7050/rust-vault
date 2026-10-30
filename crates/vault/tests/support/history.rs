//! 체인 이력 한 걸음씩: 입금 블록, 확정 전 깊이의 reorg(재포함 포함), 스캔. T3.3·T5.1·T5.2가 같이 쓴다.
#![allow(dead_code)]

use proptest::collection::vec;
use proptest::prelude::*;
use vault::chain::ChainTx;
use vault_sim::chain::{FakeChain, transfer};

use crate::scan;

/// 체인 이력 한 걸음.
#[derive(Debug, Clone)]
pub enum Op {
    /// tip 위에 블록 하나. (사용자 순번, 금액) 입금이 들어 있다
    Mine(Vec<(usize, u128)>),
    /// tip에서 `depth`(1..N) 아래 블록에서 갈라진 `depth + extra`개짜리 분기를 canonical로 삼는다.
    /// 분기의 첫 블록에 새 입금과, canonical에서 빠진 tx 중 앞 `reinclude`개를 넣는다(재포함).
    /// 분기가 예전보다 짧지 않아 tip 번호가 줄지 않는다. 그래서 확정된 입금은 빠지지 않는다(깊이 < N)
    Reorg {
        depth: u64,
        extra: u64,
        deposits: Vec<(usize, u128)>,
        reinclude: usize,
    },
    Scan,
}

pub fn op() -> impl Strategy<Value = Op> {
    let deposits = || vec((0..scan::USERS.len(), 1u128..=1_000), 0..3);
    prop_oneof![
        3 => deposits().prop_map(Op::Mine),
        2 => (1..scan::N, 0u64..=2, deposits(), 0usize..3).prop_map(
            |(depth, extra, deposits, reinclude)| Op::Reorg { depth, extra, deposits, reinclude }
        ),
        2 => Just(Op::Scan),
    ]
}

/// 걸음을 체인에 거는 쪽. 지금까지 만든 tx를 기억해 재포함에 쓴다.
#[derive(Debug, Default)]
pub struct Script {
    all_txs: Vec<ChainTx>,
    next_id: u64,
}

impl Script {
    pub fn new() -> Self {
        Self::default()
    }

    fn fresh(&mut self, deps: &[(usize, u128)]) -> Vec<ChainTx> {
        deps.iter()
            .map(|(u, v)| {
                self.next_id += 1;
                let t = transfer(self.next_id, scan::addr(scan::USERS[*u]), *v);
                self.all_txs.push(t);
                t
            })
            .collect()
    }

    /// 체인을 바꾸는 걸음을 건다. `Op::Scan`은 아무것도 하지 않는다(스캔은 호출자가 한다).
    pub fn apply(&mut self, chain: &FakeChain, op: &Op) {
        match op {
            Op::Mine(deps) => {
                let txs = self.fresh(deps);
                chain.mine_canonical(chain.tip().hash, txs);
            }
            Op::Reorg {
                depth,
                extra,
                deposits,
                reinclude,
            } => {
                let tip = chain.tip().number.0;
                let depth = (*depth).min(tip);
                let canonical = chain.canonical_blocks();
                let fork = canonical[(tip - depth) as usize].header.hash;
                // 갈라진 곳까지의 canonical에 없는 tx는 모두 "빠진 tx"다(고아 블록에 있었거나 재포함됐다 빠진 것)
                let kept: Vec<_> = canonical[..=(tip - depth) as usize]
                    .iter()
                    .flat_map(|b| b.txs.iter().map(|t| t.hash))
                    .collect();
                let mut first = self.fresh(deposits);
                first.extend(
                    self.all_txs
                        .iter()
                        .filter(|t| !kept.contains(&t.hash))
                        .filter(|t| !first.iter().any(|f| f.hash == t.hash))
                        .take(*reinclude)
                        .copied()
                        .collect::<Vec<_>>(),
                );
                let mut h = chain.mine(fork, first);
                for _ in 1..(depth + extra).max(1) {
                    h = chain.mine(h, vec![]);
                }
                chain.set_canonical(h);
            }
            Op::Scan => {}
        }
    }
}
