//! 체인 타입과 `ChainClient` trait. 입금 스캐너가 보는 체인은 이것뿐이다.
//!
//! 가짜 체인(`vault-sim::FakeChain`)과 스캐너에는 서명·인코딩이 필요 없어서 타입을 직접 정의한다.
//! 05의 서명과 06의 anvil 클라이언트에서 alloy 타입으로 바꿔 넣는다.

use std::fmt;
use std::future::Future;

/// 블록 번호.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockNumber(pub u64);

/// 32바이트 해시(블록·tx). 화면과 DB에는 `0x` + 소문자 16진수 64자로 나간다.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash32(pub [u8; 32]);

/// 20바이트 주소. 화면과 DB에는 `0x` + 소문자 16진수 40자로 나간다.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Address(pub [u8; 20]);

/// 블록 헤더 중 스캐너가 쓰는 세 값. reorg 감지는 부모 해시로만 한다(ADR-002 결정 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockHeader {
    pub number: BlockNumber,
    pub hash: Hash32,
    pub parent_hash: Hash32,
}

/// 블록과 그 안의 tx(블록 안 순서 그대로).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub header: BlockHeader,
    pub txs: Vec<ChainTx>,
}

/// 코인 이체 tx 하나. 로그·트레이스·컨트랙트 입금은 다루지 않는다.
///
/// `value`는 체인 값 그대로라 0일 수 있다. 0이면 입금이 아니다(원장의 `Amount`는 0을 받지 않는다).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChainTx {
    pub hash: Hash32,
    pub to: Address,
    pub value: u128,
    /// 실행 성공 여부(영수증 status). 실패한 tx는 코인을 옮기지 않는다
    pub success: bool,
}

/// 노드가 판단해 돌려준 오류.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError(pub String);

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// RPC 결과는 셋이다(ADR-003 결정 1). `Indeterminate`(타임아웃, 연결 끊김, 응답 유실)는
/// "결과를 모른다"이고, 호출자는 이것을 실패나 빈 결과로 바꾸지 않는다(I6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcOutcome<T> {
    Ok(T),
    Definite(RpcError),
    Indeterminate(String),
}

/// 새 head 알림. 스캐너는 내용을 쓰지 않고 "스캔하라"는 신호로만 쓴다(ADR-002 결정 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    NewHead(BlockHeader),
}

/// 스캐너가 쓰는 체인 조회 두 가지.
///
/// `block_by_number`의 `Ok(None)`은 "그 번호의 블록이 아직 없다"이다. 빈 블록이 아니다.
pub trait ChainClient {
    /// 지금 canonical head의 번호.
    fn head(&self) -> impl Future<Output = RpcOutcome<BlockNumber>> + Send;
    /// 지금 canonical chain에서 그 번호의 블록.
    fn block_by_number(
        &self,
        number: BlockNumber,
    ) -> impl Future<Output = RpcOutcome<Option<Block>>> + Send;
}

impl Hash32 {
    pub const ZERO: Hash32 = Hash32([0; 32]);
}

/// `0x` + 소문자 16진수.
fn write_hex(f: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    f.write_str("0x")?;
    for b in bytes {
        write!(f, "{b:02x}")?;
    }
    Ok(())
}

/// `0x` + 소문자 16진수 `N`바이트를 읽는다. DB에서 읽은 값을 되돌릴 때 쓴다.
fn parse_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let hex = s.strip_prefix("0x")?;
    if hex.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

impl Hash32 {
    pub fn parse(s: &str) -> Option<Self> {
        parse_hex::<32>(s).map(Self)
    }
}

impl Address {
    pub fn parse(s: &str) -> Option<Self> {
        parse_hex::<20>(s).map(Self)
    }
}

impl fmt::Display for Hash32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(f, &self.0)
    }
}

impl fmt::Debug for Hash32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(f, &self.0)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(f, &self.0)
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(f, &self.0)
    }
}

impl fmt::Display for BlockNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let mut h = [0u8; 32];
        h[0] = 0xab;
        h[31] = 0x01;
        let s = Hash32(h).to_string();
        assert_eq!(s.len(), 66);
        assert!(s.starts_with("0xab00"));
        assert_eq!(Hash32::parse(&s), Some(Hash32(h)));
        let a = Address([0x0f; 20]);
        assert_eq!(Address::parse(&a.to_string()), Some(a));
        assert_eq!(Hash32::parse("0x12"), None);
        assert_eq!(Address::parse(&"0xZZ".repeat(10)), None);
        assert_eq!(Address::parse(&format!("{}", Hash32(h))), None);
    }
}
