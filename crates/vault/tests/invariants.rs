//! 불변조건 증명 테스트 진입점. ID·입력·변형의 원본은 docs/SYSTEM_INVARIANTS.md의 증명 테스트 표다.
//! 아직 구현되지 않은 테스트는 `#[ignore = "NN-브랜치"]`를 달고 `unimplemented!`로 실패한다.
//! 표와 이 파일들의 일치는 scripts/harness/check-invariant-tests.sh가 검사한다.
//! DB가 있어야 돌므로 `--features db`일 때만 빌드한다(crates/vault/Cargo.toml의 [[test]]).

#[path = "support/model.rs"]
mod model;

#[path = "invariants/i1_asset_safety.rs"]
mod i1_asset_safety;
#[path = "invariants/i2_idempotency.rs"]
mod i2_idempotency;
#[path = "invariants/i3_ledger_balance.rs"]
mod i3_ledger_balance;
#[path = "invariants/i4_chain_consistency.rs"]
mod i4_chain_consistency;
#[path = "invariants/i5_recovery.rs"]
mod i5_recovery;
#[path = "invariants/i6_external_uncertainty.rs"]
mod i6_external_uncertainty;
