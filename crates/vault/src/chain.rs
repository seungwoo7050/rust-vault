//! 체인 타입과 `ChainClient` trait. 입금 스캐너가 보는 체인은 이것뿐이다.
//!
//! 체인 타입은 직접 정의한다. 서명·인코딩(alloy)은 `signer` 안에 있고, 이 모듈과는 `Hash32`·`Address`와
//! raw 바이트로만 오간다. 출금 처리기는 `send_raw`·`receipt`·`nonce_at`을 더 쓴다.

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

/// 노드가 거절한 전송(`send_raw`의 `Definite`)의 분류(ADR-003 결정 2).
/// 앞의 셋은 "이 tx가 이미 들어갔을 수 있다"는 뜻이라 실패가 아니다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendRejection {
    /// 같은 tx가 이미 멤풀에 있다
    AlreadyKnown,
    /// 그 nonce는 이미 쓰였다(내 tx일 수도, 다른 tx일 수도 있다)
    NonceTooLow,
    /// 같은 nonce의 다른 tx가 멤풀에 있다
    ReplacementUnderpriced,
    /// 그 밖(잔고 부족, 일시 거절 등). 이것도 앞서 보낸 같은 tx가 다른 곳에 없다는 증거가 아니다
    Other(String),
}

impl RpcError {
    /// 전송 거절 메시지를 분류한다. 메시지는 geth 계열 노드의 문구를 따른다.
    pub fn send_rejection(&self) -> SendRejection {
        let m = self.0.to_ascii_lowercase();
        if m.contains("already known") {
            SendRejection::AlreadyKnown
        } else if m.contains("nonce too low") {
            SendRejection::NonceTooLow
        } else if m.contains("replacement transaction underpriced") {
            SendRejection::ReplacementUnderpriced
        } else {
            SendRejection::Other(self.0.clone())
        }
    }
}

/// tx 영수증 중 출금 처리기가 쓰는 값.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Receipt {
    pub block_hash: Hash32,
    pub block_number: BlockNumber,
    /// 실행 성공 여부(status 1)
    pub success: bool,
    pub gas_used: u64,
    pub effective_gas_price: u128,
}

impl Receipt {
    /// 이 tx가 낸 가스비(wei).
    pub fn fee(&self) -> u128 {
        u128::from(self.gas_used) * self.effective_gas_price
    }
}

/// 새 head 알림. 스캐너는 내용을 쓰지 않고 "스캔하라"는 신호로만 쓴다(ADR-002 결정 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    NewHead(BlockHeader),
}

/// 스캐너와 출금 처리기가 쓰는 체인 호출.
///
/// `block_by_number`의 `Ok(None)`은 "그 번호의 블록이 아직 없다"이다. 빈 블록이 아니다.
/// `receipt`의 `Ok(None)`은 "지금 canonical chain에 그 tx가 없다"이다.
pub trait ChainClient {
    /// 지금 canonical head의 번호.
    fn head(&self) -> impl Future<Output = RpcOutcome<BlockNumber>> + Send;
    /// 지금 canonical chain에서 그 번호의 블록.
    fn block_by_number(
        &self,
        number: BlockNumber,
    ) -> impl Future<Output = RpcOutcome<Option<Block>>> + Send;
    /// 서명한 raw tx를 보낸다(`eth_sendRawTransaction`). `Ok`는 노드가 받은 tx 해시다.
    /// 거절은 `Definite`이고 `RpcError::send_rejection`으로 분류한다.
    fn send_raw(&self, raw: &[u8]) -> impl Future<Output = RpcOutcome<Hash32>> + Send;
    /// tx 영수증. 지금 canonical chain 기준이다.
    fn receipt(&self, tx: Hash32) -> impl Future<Output = RpcOutcome<Option<Receipt>>> + Send;
    /// 그 블록 시점의 주소 nonce(그 주소가 보낸, 채굴된 tx 수).
    fn nonce_at(
        &self,
        address: Address,
        block: BlockNumber,
    ) -> impl Future<Output = RpcOutcome<u64>> + Send;
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

    #[test]
    fn send_rejection_classes() {
        let c = |m: &str| RpcError(m.into()).send_rejection();
        assert_eq!(c("already known"), SendRejection::AlreadyKnown);
        assert_eq!(c("nonce too low: next nonce 5"), SendRejection::NonceTooLow);
        assert_eq!(
            c("replacement transaction underpriced"),
            SendRejection::ReplacementUnderpriced
        );
        assert_eq!(
            c("insufficient funds for gas"),
            SendRejection::Other("insufficient funds for gas".into())
        );
    }
}
