//! 가짜 체인. 블록 트리를 들고 있고, 그중 한 tip까지의 경로가 canonical chain이다.
//!
//! - 분기: 아무 블록 위에나 `mine`으로 블록을 붙인다. 해시는 (부모, 번호, 소금)의 sha256이라 결정적이다.
//!   소금은 지금까지 캔 블록 수라 같은 부모 위의 두 블록도 해시가 다르다.
//! - reorg: `set_canonical(tip)`으로 canonical을 바꾼다. 새로 canonical이 된 블록마다 새 head 알림이 쌓인다.
//! - 장애: `fail_next`(다음 n번 조회를 `Indeterminate`/`Definite`로), `none_at`(그 번호를 n번 `None`으로).
//!
//! 핸들(`Clone`)을 여럿 둘 수 있다. 스캐너에 하나를 넘기고 테스트가 다른 하나로 체인을 바꾼다.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use vault::chain::{
    Address, Block, BlockHeader, BlockNumber, ChainClient, ChainTx, Hash32, Notification, RpcError,
    RpcOutcome,
};

/// 장애를 넣을 조회.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    Head,
    /// 번호와 상관없이 블록 조회
    Block,
    /// 이 번호의 블록 조회
    BlockAt(BlockNumber),
}

/// 넣을 장애.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// 타임아웃처럼 결과를 모름
    Indeterminate,
    /// 노드가 거절함
    Definite,
}

#[derive(Debug)]
struct Rule {
    call: Call,
    left: usize,
    fault: Fault,
}

#[derive(Debug)]
struct Inner {
    blocks: HashMap<Hash32, Block>,
    genesis: Hash32,
    /// canonical chain. 인덱스가 블록 번호다
    canonical: Vec<Hash32>,
    mined: u64,
    rules: Vec<Rule>,
    nones: HashMap<BlockNumber, usize>,
    notifications: Vec<Notification>,
    calls: usize,
}

/// 가짜 체인 핸들.
#[derive(Debug, Clone)]
pub struct FakeChain(Arc<Mutex<Inner>>);

impl Default for FakeChain {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeChain {
    /// genesis(번호 0, 부모 해시 0) 하나만 있는 체인.
    pub fn new() -> Self {
        let header = BlockHeader {
            number: BlockNumber(0),
            hash: block_hash(Hash32::ZERO, 0, 0),
            parent_hash: Hash32::ZERO,
        };
        let genesis = header.hash;
        let mut blocks = HashMap::new();
        blocks.insert(
            genesis,
            Block {
                header,
                txs: vec![],
            },
        );
        Self(Arc::new(Mutex::new(Inner {
            blocks,
            genesis,
            canonical: vec![genesis],
            mined: 1,
            rules: vec![],
            nones: HashMap::new(),
            notifications: vec![],
            calls: 0,
        })))
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().expect("FakeChain 잠금")
    }

    pub fn genesis(&self) -> BlockHeader {
        let g = self.inner();
        g.blocks[&g.genesis].header
    }

    /// `parent` 위에 블록을 하나 캔다. canonical은 바꾸지 않는다.
    pub fn mine(&self, parent: Hash32, txs: Vec<ChainTx>) -> Hash32 {
        let mut g = self.inner();
        let parent_number = g.blocks.get(&parent).expect("없는 부모 블록").header.number;
        let number = parent_number.0 + 1;
        let hash = block_hash(parent, number, g.mined);
        g.mined += 1;
        g.blocks.insert(
            hash,
            Block {
                header: BlockHeader {
                    number: BlockNumber(number),
                    hash,
                    parent_hash: parent,
                },
                txs,
            },
        );
        hash
    }

    /// `parent` 위에 블록을 캐고 canonical tip으로 삼는다.
    pub fn mine_canonical(&self, parent: Hash32, txs: Vec<ChainTx>) -> Hash32 {
        let h = self.mine(parent, txs);
        self.set_canonical(h);
        h
    }

    /// canonical chain을 genesis에서 `tip`까지의 경로로 바꾼다.
    /// 예전 canonical과 갈라진 곳 위의 블록마다 새 head 알림을 쌓는다.
    pub fn set_canonical(&self, tip: Hash32) {
        let mut g = self.inner();
        let mut path = vec![];
        let mut cur = tip;
        loop {
            path.push(cur);
            let b = g.blocks.get(&cur).expect("없는 블록");
            if b.header.number.0 == 0 {
                break;
            }
            cur = b.header.parent_hash;
        }
        path.reverse();
        let same = path
            .iter()
            .zip(g.canonical.iter())
            .take_while(|(a, b)| a == b)
            .count();
        for h in &path[same..] {
            let header = g.blocks[h].header;
            g.notifications.push(Notification::NewHead(header));
        }
        g.canonical = path;
    }

    pub fn tip(&self) -> BlockHeader {
        let g = self.inner();
        g.blocks[g.canonical.last().unwrap()].header
    }

    pub fn header(&self, hash: Hash32) -> BlockHeader {
        self.inner().blocks.get(&hash).expect("없는 블록").header
    }

    /// genesis부터 tip까지 canonical 블록.
    pub fn canonical_blocks(&self) -> Vec<Block> {
        let g = self.inner();
        g.canonical.iter().map(|h| g.blocks[h].clone()).collect()
    }

    /// 다음 `n`번의 `call` 조회를 `fault`로 끝낸다.
    pub fn fail_next(&self, call: Call, n: usize, fault: Fault) {
        self.inner().rules.push(Rule {
            call,
            left: n,
            fault,
        });
    }

    /// 다음 `n`번의 `number` 블록 조회에 `Ok(None)`("아직 없음")을 돌려준다. head는 그대로다.
    pub fn none_at(&self, number: BlockNumber, n: usize) {
        *self.inner().nones.entry(number).or_default() += n;
    }

    /// 쌓인 새 head 알림을 꺼낸다.
    pub fn notifications(&self) -> Vec<Notification> {
        std::mem::take(&mut self.inner().notifications)
    }

    /// 지금까지 받은 조회 수(head, 블록 조회 합).
    pub fn calls(&self) -> usize {
        self.inner().calls
    }
}

impl Inner {
    /// 이 조회에 걸리는 장애가 있으면 하나 쓰고 돌려준다.
    fn take_fault(&mut self, call: Call) -> Option<Fault> {
        let rule = self.rules.iter_mut().find(|r| {
            r.left > 0
                && (r.call == call || (r.call == Call::Block && matches!(call, Call::BlockAt(_))))
        })?;
        rule.left -= 1;
        Some(rule.fault)
    }
}

fn outcome<T>(fault: Fault) -> RpcOutcome<T> {
    match fault {
        Fault::Indeterminate => RpcOutcome::Indeterminate("injected timeout".into()),
        Fault::Definite => RpcOutcome::Definite(RpcError("injected rejection".into())),
    }
}

impl ChainClient for FakeChain {
    async fn head(&self) -> RpcOutcome<BlockNumber> {
        let mut g = self.inner();
        g.calls += 1;
        if let Some(f) = g.take_fault(Call::Head) {
            return outcome(f);
        }
        RpcOutcome::Ok(BlockNumber(g.canonical.len() as u64 - 1))
    }

    async fn block_by_number(&self, number: BlockNumber) -> RpcOutcome<Option<Block>> {
        let mut g = self.inner();
        g.calls += 1;
        if let Some(f) = g.take_fault(Call::BlockAt(number)) {
            return outcome(f);
        }
        if let Some(left) = g.nones.get_mut(&number)
            && *left > 0
        {
            *left -= 1;
            return RpcOutcome::Ok(None);
        }
        let block = g
            .canonical
            .get(number.0 as usize)
            .map(|h| g.blocks[h].clone());
        RpcOutcome::Ok(block)
    }
}

fn block_hash(parent: Hash32, number: u64, salt: u64) -> Hash32 {
    let mut h = Sha256::new();
    h.update(b"block");
    h.update(parent.0);
    h.update(number.to_be_bytes());
    h.update(salt.to_be_bytes());
    Hash32(h.finalize().into())
}

/// 테스트용 주소. 같은 `n`이면 같은 주소다.
pub fn address(n: u64) -> Address {
    let mut h = Sha256::new();
    h.update(b"address");
    h.update(n.to_be_bytes());
    let d: [u8; 32] = h.finalize().into();
    let mut a = [0u8; 20];
    a.copy_from_slice(&d[..20]);
    Address(a)
}

/// 성공한 코인 이체 tx. 해시는 `id`로만 정해지므로, 같은 `id`의 tx를 다른 블록에 넣으면 "다시 들어간 tx"다.
pub fn transfer(id: u64, to: Address, value: u128) -> ChainTx {
    let mut h = Sha256::new();
    h.update(b"tx");
    h.update(id.to_be_bytes());
    ChainTx {
        hash: Hash32(h.finalize().into()),
        to,
        value,
        success: true,
    }
}

/// 실행에 실패한 이체 tx(코인을 옮기지 않는다).
pub fn failed_transfer(id: u64, to: Address, value: u128) -> ChainTx {
    ChainTx {
        success: false,
        ..transfer(id, to, value)
    }
}

/// 알림 섞기: `seed`로 정해지는 결정적 순열(Fisher–Yates, splitmix64).
pub fn shuffle<T>(items: &mut [T], seed: u64) {
    let mut s = seed;
    let mut next = move || {
        s = s.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    };
    for i in (1..items.len()).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}
