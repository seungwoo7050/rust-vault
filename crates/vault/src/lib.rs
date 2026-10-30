//! RustVault 입출금 코어. 모듈 구성은 docs/ARCHITECTURE.md, 불변조건은 docs/SYSTEM_INVARIANTS.md.

pub mod chain;
pub mod clock;
pub mod deposit;
pub mod ledger;
pub mod signer;
pub mod store;
pub mod withdrawal;
