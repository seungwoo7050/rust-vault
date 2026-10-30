//! 가짜 체인. 블록 트리를 들고 있고, 그중 한 tip까지의 경로가 canonical chain이다.
//!
//! - 분기: 아무 블록 위에나 `mine`으로 블록을 붙인다. 해시는 (부모, 번호, 소금)의 sha256이라 결정적이다.
//!   소금은 지금까지 캔 블록 수라 같은 부모 위의 두 블록도 해시가 다르다.
//! - reorg: `set_canonical(tip)`으로 canonical을 바꾼다. 새로 canonical이 된 블록마다 새 head 알림이 쌓인다.
//! - 장애: `fail_next`(다음 n번 호출을 `Indeterminate`/`Definite`/`LostResponse`로), `none_at`(그 번호를 n번 `None`으로).
//! - 계정과 멤풀: `send_raw`로 받은 tx는 서명자를 복구해 멤풀에 넣고, `mine`이 부모까지의 경로에서 센 계정 nonce
//!   순서대로 멤풀의 tx를 블록에 담는다(담은 tx는 멤풀에서 뺀다). reorg로 빠진 tx는 멤풀로 돌아가지 않는다
//!   (노드가 잊은 경우. 다시 보내야 들어간다). `external_send`는 이 시스템 밖에서 같은 키로 보낸 tx를 넣는다.
//!   노드가 받은 raw tx는 `received`에 모두 남는다.
//!
//! 핸들(`Clone`)을 여럿 둘 수 있다. 스캐너에 하나를 넘기고 테스트가 다른 하나로 체인을 바꾼다.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use vault::chain::{
    Address, Block, BlockHeader, BlockNumber, ChainClient, ChainTx, Hash32, Notification, Receipt,
    RpcError, RpcOutcome,
};
use vault::signer::{self, TRANSFER_GAS};

/// 장애를 넣을 조회.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    Head,
    /// 번호와 상관없이 블록 조회
    Block,
    /// 이 번호의 블록 조회
    BlockAt(BlockNumber),
    /// raw tx 전송
    SendRaw,
    /// 영수증 조회
    Receipt,
    /// 계정 nonce 조회
    NonceAt,
    /// 계정 잔액 조회
    BalanceAt,
}

/// 넣을 장애.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// 타임아웃처럼 결과를 모름
    Indeterminate,
    /// 노드가 거절함
    Definite,
    /// 요청은 반영하고 응답만 버림(결과는 `Indeterminate`). 전송에만 뜻이 있고, 조회에서는 `Indeterminate`와 같다
    LostResponse,
}

/// 멤풀이나 블록에 든 계정 tx의 보낸 쪽 정보.
#[derive(Debug, Clone, Copy)]
struct Sender {
    from: Address,
    nonce: u64,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
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
    /// 계정 tx(보낸 쪽이 있는 tx)의 보낸 쪽 정보. 입금 테스트의 `transfer`에는 없다
    senders: HashMap<Hash32, Sender>,
    /// 멤풀(받은 순서). 블록에 담기면 뺀다
    pool: Vec<ChainTx>,
    /// 노드가 받은 raw tx 전부(받은 순서). 응답을 버린 것과 거절한 것도 들어 있다
    received: Vec<Vec<u8>>,
    /// 이 수취인에게 가는 계정 tx는 실행이 실패한다(status 0, 가스비는 낸다)
    reverts: HashSet<Address>,
    base_fee: u128,
    /// 대사 불일치 주입용 주소별 온체인 잔액 보정값
    balance_adjustments: HashMap<Address, i128>,
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
            senders: HashMap::new(),
            pool: vec![],
            received: vec![],
            reverts: HashSet::new(),
            base_fee: BASE_FEE,
            balance_adjustments: HashMap::new(),
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
    /// 블록에는 `txs` 뒤에 멤풀의 tx가 계정별 nonce 순서로 들어간다(부모까지의 경로에서 다음 nonce인 것만).
    pub fn mine(&self, parent: Hash32, txs: Vec<ChainTx>) -> Hash32 {
        let mut g = self.inner();
        let parent_number = g.blocks.get(&parent).expect("없는 부모 블록").header.number;
        let mut txs = txs;
        g.take_from_pool(parent, &mut txs);
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

    /// 걸어 두고 아직 쓰이지 않은 조회·전송 장애를 모두 지운다.
    pub fn clear_faults(&self) {
        let mut g = self.inner();
        g.rules.clear();
        g.nones.clear();
    }

    /// 다음 `n`번의 `number` 블록 조회에 `Ok(None)`("아직 없음")을 돌려준다. head는 그대로다.
    pub fn none_at(&self, number: BlockNumber, n: usize) {
        *self.inner().nones.entry(number).or_default() += n;
    }

    /// 쌓인 새 head 알림을 꺼낸다.
    pub fn notifications(&self) -> Vec<Notification> {
        std::mem::take(&mut self.inner().notifications)
    }

    /// 지금까지 받은 호출 수(head, 블록·영수증·nonce 조회, 전송 합).
    pub fn calls(&self) -> usize {
        self.inner().calls
    }

    /// 노드가 받은 raw tx 전부(받은 순서).
    pub fn received(&self) -> Vec<Vec<u8>> {
        self.inner().received.clone()
    }

    /// 앞으로 `to`에게 가는 계정 tx는 실행이 실패한다(status 0).
    pub fn revert_to(&self, to: Address) {
        self.inner().reverts.insert(to);
    }

    /// 대사 불일치 주입용으로 주소의 온체인 잔액에 보정값(`delta`)을 더한다.
    pub fn adjust_balance(&self, address: Address, delta: i128) {
        *self.inner().balance_adjustments.entry(address).or_default() += delta;
    }

    /// 이 시스템 밖에서 같은 키로 보낸 tx를 멤풀에 넣는다(F5). 해시는 (from, nonce, to, value)로 정해진다.
    pub fn external_send(&self, from: Address, nonce: u64, to: Address, value: u128) -> Hash32 {
        let mut h = Sha256::new();
        h.update(b"external");
        h.update(from.0);
        h.update(nonce.to_be_bytes());
        h.update(to.0);
        h.update(value.to_be_bytes());
        let hash = Hash32(h.finalize().into());
        let mut g = self.inner();
        let fee = g.base_fee;
        g.senders.insert(
            hash,
            Sender {
                from,
                nonce,
                max_fee_per_gas: fee,
                max_priority_fee_per_gas: 0,
            },
        );
        g.pool.push(ChainTx {
            hash,
            to,
            value,
            success: true,
        });
        hash
    }

    /// 멤풀까지 친 다음 nonce(canonical tip의 nonce와 멤풀에서 이어지는 tx 수).
    pub fn pending_nonce(&self, from: Address) -> u64 {
        let g = self.inner();
        let tip = *g.canonical.last().unwrap();
        let mut next = g.nonce_on_path(tip, from);
        while g.pool.iter().any(|t| {
            g.senders
                .get(&t.hash)
                .is_some_and(|s| s.from == from && s.nonce == next)
        }) {
            next += 1;
        }
        next
    }

    /// 지금 멤풀에 있는 tx 해시.
    pub fn pool(&self) -> Vec<Hash32> {
        self.inner().pool.iter().map(|t| t.hash).collect()
    }
}

/// 가짜 체인의 기본 base fee(wei). 실효 가스 가격은 min(최대 수수료, base fee + 우선 수수료)다.
pub const BASE_FEE: u128 = 7;

impl Inner {
    /// genesis부터 `tip`까지의 경로.
    fn path_to(&self, tip: Hash32) -> Vec<Hash32> {
        let mut path = vec![];
        let mut cur = tip;
        loop {
            path.push(cur);
            let b = &self.blocks[&cur];
            if b.header.number.0 == 0 {
                break;
            }
            cur = b.header.parent_hash;
        }
        path.reverse();
        path
    }

    /// `tip`까지의 경로에서 `from`이 보낸 계정 tx 수(= 다음 nonce. 담을 때 nonce 순서를 지킨다).
    fn nonce_on_path(&self, tip: Hash32, from: Address) -> u64 {
        self.path_to(tip)
            .iter()
            .flat_map(|h| self.blocks[h].txs.iter())
            .filter(|t| self.senders.get(&t.hash).is_some_and(|s| s.from == from))
            .count() as u64
    }

    /// `tip`까지의 경로에서 `address`의 코인 잔액(받은 코인 − 보낸 코인·가스비 + 보정값, 최소 0).
    fn balance_on_path(&self, tip: Hash32, address: Address) -> u128 {
        let mut bal: i128 = 0;
        for h in self.path_to(tip) {
            for t in &self.blocks[&h].txs {
                if t.success && t.to == address {
                    bal = bal.saturating_add(t.value as i128);
                }
                if let Some(s) = self.senders.get(&t.hash)
                    && s.from == address
                {
                    let price = s
                        .max_fee_per_gas
                        .min(self.base_fee + s.max_priority_fee_per_gas);
                    let fee = u128::from(TRANSFER_GAS) * price;
                    let spent = if t.success { t.value + fee } else { fee };
                    bal = bal.saturating_sub(spent as i128);
                }
            }
        }
        if let Some(&delta) = self.balance_adjustments.get(&address) {
            bal = bal.saturating_add(delta);
        }
        bal.max(0) as u128
    }

    /// 멤풀에서 `parent` 다음 블록에 들어갈 tx를 꺼내 `txs` 뒤에 붙인다. 실행 실패 수취인이면 status 0이다.
    fn take_from_pool(&mut self, parent: Hash32, txs: &mut Vec<ChainTx>) {
        let mut next: HashMap<Address, u64> = HashMap::new();
        loop {
            let pick = self.pool.iter().position(|t| {
                let s = self.senders[&t.hash];
                let n = *next
                    .entry(s.from)
                    .or_insert_with(|| self.nonce_on_path(parent, s.from));
                s.nonce == n
            });
            let Some(i) = pick else { break };
            let mut t = self.pool.remove(i);
            let s = self.senders[&t.hash];
            *next.get_mut(&s.from).unwrap() += 1;
            t.success = !self.reverts.contains(&t.to);
            txs.push(t);
        }
    }

    fn canonical_tx(&self, hash: Hash32) -> Option<(BlockHeader, ChainTx)> {
        self.canonical.iter().find_map(|h| {
            let b = &self.blocks[h];
            b.txs
                .iter()
                .find(|t| t.hash == hash)
                .map(|t| (b.header, *t))
        })
    }

    /// 받은 raw tx를 반영한다. 노드의 판단 순서: 이미 쓰인 nonce → 멤풀에 같은 tx → 같은 nonce의 다른 tx.
    fn accept(&mut self, raw: &[u8]) -> RpcOutcome<Hash32> {
        let reject = |m: &str| RpcOutcome::Definite(RpcError(m.into()));
        let Some(d) = signer::decode(raw) else {
            return reject("invalid transaction");
        };
        let tip = *self.canonical.last().unwrap();
        if self.canonical_tx(d.hash).is_some() || d.nonce < self.nonce_on_path(tip, d.from) {
            return reject("nonce too low");
        }
        if self.pool.iter().any(|t| t.hash == d.hash) {
            return reject("already known");
        }
        let taken = self.pool.iter().any(|t| {
            self.senders
                .get(&t.hash)
                .is_some_and(|s| s.from == d.from && s.nonce == d.nonce)
        });
        if taken {
            return reject("replacement transaction underpriced");
        }
        self.senders.insert(
            d.hash,
            Sender {
                from: d.from,
                nonce: d.nonce,
                max_fee_per_gas: d.max_fee_per_gas,
                max_priority_fee_per_gas: d.max_priority_fee_per_gas,
            },
        );
        self.pool.push(ChainTx {
            hash: d.hash,
            to: d.to,
            value: d.value,
            success: true,
        });
        RpcOutcome::Ok(d.hash)
    }

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
        Fault::Indeterminate | Fault::LostResponse => {
            RpcOutcome::Indeterminate("injected timeout".into())
        }
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

    async fn send_raw(&self, raw: &[u8]) -> RpcOutcome<Hash32> {
        let mut g = self.inner();
        g.calls += 1;
        let fault = g.take_fault(Call::SendRaw);
        // 연결이 끊겨 노드에 닿지 않았다
        if fault == Some(Fault::Indeterminate) {
            return outcome(Fault::Indeterminate);
        }
        g.received.push(raw.to_vec());
        if fault == Some(Fault::Definite) {
            return outcome(Fault::Definite);
        }
        let result = g.accept(raw);
        // 반영은 했고 응답만 사라졌다
        if fault == Some(Fault::LostResponse) {
            return outcome(Fault::LostResponse);
        }
        result
    }

    async fn receipt(&self, tx: Hash32) -> RpcOutcome<Option<Receipt>> {
        let mut g = self.inner();
        g.calls += 1;
        if let Some(f) = g.take_fault(Call::Receipt) {
            return outcome(f);
        }
        let Some((header, t)) = g.canonical_tx(tx) else {
            return RpcOutcome::Ok(None);
        };
        let price = match g.senders.get(&tx) {
            Some(s) => s
                .max_fee_per_gas
                .min(g.base_fee + s.max_priority_fee_per_gas),
            None => g.base_fee,
        };
        RpcOutcome::Ok(Some(Receipt {
            block_hash: header.hash,
            block_number: header.number,
            success: t.success,
            gas_used: TRANSFER_GAS,
            effective_gas_price: price,
        }))
    }

    async fn nonce_at(&self, address: Address, block: BlockNumber) -> RpcOutcome<u64> {
        let mut g = self.inner();
        g.calls += 1;
        if let Some(f) = g.take_fault(Call::NonceAt) {
            return outcome(f);
        }
        let Some(&h) = g.canonical.get(block.0 as usize) else {
            return RpcOutcome::Definite(RpcError("header not found".into()));
        };
        RpcOutcome::Ok(g.nonce_on_path(h, address))
    }

    async fn balance_at(&self, address: Address, block: BlockNumber) -> RpcOutcome<u128> {
        let mut g = self.inner();
        g.calls += 1;
        if let Some(f) = g.take_fault(Call::BalanceAt) {
            return outcome(f);
        }
        let Some(&h) = g.canonical.get(block.0 as usize) else {
            return RpcOutcome::Definite(RpcError("header not found".into()));
        };
        RpcOutcome::Ok(g.balance_on_path(h, address))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balance_at_tracks_incoming_and_adjustments() {
        let chain = FakeChain::new();
        let addr = address(7);
        let g = chain.genesis().hash;
        let b1 = chain.mine_canonical(
            g,
            vec![transfer(1, addr, 1_000), failed_transfer(2, addr, 500)],
        );
        assert_eq!(chain.inner().balance_on_path(g, addr), 0);
        assert_eq!(chain.inner().balance_on_path(b1, addr), 1_000);
        chain.adjust_balance(addr, -250);
        assert_eq!(chain.inner().balance_on_path(b1, addr), 750);
    }
}
