//! 출금 tx 서명(ADR-003 결정 4). `Signer` trait와 로컬 키 구현, 그리고 raw tx를 다시 읽는 `decode`.
//!
//! 서명과 인코딩은 alloy가 한다(ARCHITECTURE 결정). 이 모듈 밖으로는 `Hash32`·`Address`와 raw 바이트만 나간다.
//! tx는 EIP-1559 코인 전송 하나뿐이다(가스 한도 21000, 수수료 상한은 설정값).
//! 서명은 RFC6979라 결정적이다: 같은 `TxRequest`면 같은 바이트, 같은 해시다.

use std::fmt;

use alloy_consensus::transaction::SignerRecoverable;
use alloy_consensus::{SignableTransaction, Transaction, TxEip1559, TxEnvelope};
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{B256, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;

use crate::chain::{Address, Hash32};

/// 코인 전송의 가스 한도.
pub const TRANSFER_GAS: u64 = 21_000;

/// 서명할 전송 tx.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxRequest {
    pub chain_id: u64,
    pub nonce: u64,
    pub to: Address,
    pub value: u128,
    pub gas_limit: u64,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

/// 서명한 tx. `raw`는 노드에 보내는 EIP-2718 바이트, `hash`는 그 tx 해시다.
#[derive(Clone, PartialEq, Eq)]
pub struct SignedTx {
    pub raw: Vec<u8>,
    pub hash: Hash32,
}

impl fmt::Debug for SignedTx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SignedTx({}, {} bytes)", self.hash, self.raw.len())
    }
}

/// 서명 실패.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignError(pub String);

impl fmt::Display for SignError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sign: {}", self.0)
    }
}

impl std::error::Error for SignError {}

/// 출금 주소의 키. 출금 처리기는 `Held → Signed` 전이에서만 `sign`을 부른다(ADR-003 결정 4).
pub trait Signer {
    /// 이 키의 주소(출금 주소).
    fn address(&self) -> Address;
    fn sign(&self, tx: TxRequest) -> Result<SignedTx, SignError>;
}

/// 메모리에 든 secp256k1 키.
pub struct LocalSigner(PrivateKeySigner);

impl LocalSigner {
    /// 32바이트 비밀 키로 만든다.
    pub fn from_bytes(key: [u8; 32]) -> Result<Self, SignError> {
        PrivateKeySigner::from_bytes(&B256::from(key))
            .map(Self)
            .map_err(|e| SignError(e.to_string()))
    }
}

impl fmt::Debug for LocalSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LocalSigner({})", self.address())
    }
}

impl Signer for LocalSigner {
    fn address(&self) -> Address {
        Address(self.0.address().into_array())
    }

    fn sign(&self, req: TxRequest) -> Result<SignedTx, SignError> {
        let tx = TxEip1559 {
            chain_id: req.chain_id,
            nonce: req.nonce,
            gas_limit: req.gas_limit,
            max_fee_per_gas: req.max_fee_per_gas,
            max_priority_fee_per_gas: req.max_priority_fee_per_gas,
            to: TxKind::Call(alloy_primitives::Address::from(req.to.0)),
            value: U256::from(req.value),
            access_list: Default::default(),
            input: Default::default(),
        };
        let sig = self
            .0
            .sign_hash_sync(&tx.signature_hash())
            .map_err(|e| SignError(e.to_string()))?;
        let envelope = TxEnvelope::from(tx.into_signed(sig));
        Ok(SignedTx {
            raw: envelope.encoded_2718(),
            hash: Hash32(envelope.tx_hash().0),
        })
    }
}

/// raw tx를 읽은 결과. 가짜 체인이 받은 바이트를 해석할 때 쓴다(서명자 복구 포함).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedTx {
    pub hash: Hash32,
    pub from: Address,
    pub chain_id: u64,
    pub nonce: u64,
    pub to: Address,
    pub value: u128,
    pub gas_limit: u64,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

/// EIP-1559 코인 전송 raw tx를 읽고 서명자를 복구한다. 그 밖의 tx나 망가진 바이트는 `None`.
pub fn decode(raw: &[u8]) -> Option<DecodedTx> {
    let envelope = TxEnvelope::decode_2718_exact(raw).ok()?;
    let from = envelope.recover_signer().ok()?;
    let tx = envelope.as_eip1559()?.tx();
    let to = tx.to.to()?;
    Some(DecodedTx {
        hash: Hash32(envelope.tx_hash().0),
        from: Address(from.into_array()),
        chain_id: tx.chain_id,
        nonce: tx.nonce,
        to: Address(to.into_array()),
        value: u128::try_from(tx.value()).ok()?,
        gas_limit: tx.gas_limit,
        max_fee_per_gas: tx.max_fee_per_gas,
        max_priority_fee_per_gas: tx.max_priority_fee_per_gas,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // 실제 자산이 없는 테스트 전용 키(공개된 개발 체인 기본 계정 0의 키). 운영 키가 아니다
    const KEY: [u8; 32] = [
        0xac, 0x09, 0x74, 0xbe, 0xc3, 0x9a, 0x17, 0xe3, 0x6b, 0xa4, 0xa6, 0xb4, 0xd2, 0x38, 0xff,
        0x94, 0x4b, 0xac, 0xb4, 0x78, 0xcb, 0xed, 0x5e, 0xfc, 0xae, 0x78, 0x4d, 0x7b, 0xf4, 0xf2,
        0xff, 0x80,
    ];

    fn req(nonce: u64) -> TxRequest {
        TxRequest {
            chain_id: 1,
            nonce,
            to: Address([7; 20]),
            value: 1_000,
            gas_limit: TRANSFER_GAS,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 2,
        }
    }

    #[test]
    fn signing_is_deterministic_and_decodes() {
        let s = LocalSigner::from_bytes(KEY).unwrap();
        assert_eq!(
            s.address().to_string(),
            "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
        );
        let a = s.sign(req(3)).unwrap();
        let b = s.sign(req(3)).unwrap();
        assert_eq!(a, b, "같은 요청은 같은 바이트(RFC6979)");
        let c = s.sign(req(4)).unwrap();
        assert_ne!(a.hash, c.hash, "nonce가 다르면 해시가 다르다");

        let d = decode(&a.raw).expect("decode");
        assert_eq!(d.hash, a.hash);
        assert_eq!(d.from, s.address());
        assert_eq!((d.nonce, d.to, d.value), (3, Address([7; 20]), 1_000));
        assert_eq!((d.max_fee_per_gas, d.max_priority_fee_per_gas), (100, 2));
        assert!(decode(&a.raw[1..]).is_none());
    }
}
