# RustVault

블록체인의 불확실성(이벤트 중복·순서 변경, 체인 분기 교체 reorg, RPC 타임아웃·응답 유실, nonce 충돌, 프로세스 강제 종료·DB 일시 장애) 속에서도 사용자 자산의 이동과 회계 기록이 깨지지 않는 입출금 원장 시스템이다. 기능 나열이 아니라 **6개 시스템 불변조건(I1–I6)**, **8개 장애 시나리오(F1–F8)**, 이를 깨뜨리는 **37개 변형(mutants)**을 실제로 잡아내는 **24개 불변조건 증명 테스트(T1.1–T6.6)**와 **대사(Reconciliation)·Anvil Reorg PoC**가 결과물이다.

- **출처: 사람 설계** — 불변조건 I1–I6, 장애 시나리오 F1–F8, [ADR-002](docs/adr/002-reorg-handling.md)의 reorg 되감기·확정 정책 선택, 워크스페이스 범위(crate 2개 `vault`·`vault-sim`, 성능 최적화·HTTP API 제외).
- **확정: 오케스트레이터** — 판정 가능한 불변조건 문장, [ADR-001](docs/adr/001-ledger-model.md)(원장 모델)·[ADR-003](docs/adr/003-nonce-and-rpc-uncertainty.md)(출금 상태 기계·nonce·RPC 불확실성), 증명 테스트 T1.1–T6.6 및 변형(mutants) 설계, 대사(`vault::recon`) 및 [`paradigmxyz/reth` 오픈소스 비교 분석](docs/analysis/reth-reorg.md).

---

## 불변조건 추적표 (불변조건 → 장애 시나리오 → 증명 테스트 → ADR)

원본은 [docs/SYSTEM_INVARIANTS.md](docs/SYSTEM_INVARIANTS.md)이며, 표와 실제 테스트 함수의 1:1 대응은 [`scripts/harness/check-invariant-tests.sh`](scripts/harness/check-invariant-tests.sh)가 매 검증마다 강제한다.

| 불변조건 (출처: 사람 설계) | 지키는 핵심 장치 | 위협 시나리오 ([F1–F8](docs/FAILURE_SCENARIOS.md)) | 증명 테스트 ([`crates/vault/tests/invariants/`](crates/vault/tests/invariants/)) | 관련 ADR |
| --- | --- | --- | --- | --- |
| **I1 자산 안전성**<br>사용자 잔액은 승인된 원장 거래 없이 변경될 수 없다 | 잔액 열 없음(분개 합으로만 계산), `SELECT ... FOR UPDATE` 잠금 후 음수 검사, 잠정 입금(`pending_deposit:u`)과 확정 잔액(`user:u`) 계정 분리 | F1, F2, F4, F5 | [`i1_asset_safety.rs`](crates/vault/tests/invariants/i1_asset_safety.rs)<br>• `T1.1` `rejected_entry_changes_no_balance`<br>• `T1.2` `pending_deposit_not_spendable`<br>• `T1.3` `hold_rejects_overspend` | [ADR-001](docs/adr/001-ledger-model.md)<br>[ADR-002](docs/adr/002-reorg-handling.md)<br>[ADR-003](docs/adr/003-nonce-and-rpc-uncertainty.md) |
| **I2 멱등성**<br>같은 입금 이벤트나 출금 요청을 여러 번 처리해도 자산이 중복 반영되지 않는다 | 외부 사실(tx 해시·블록 해시·요청 ID)에서 결정적으로 만드는 멱등 키 + `body_hash` 대조, 서명 tx 전송 전 DB 저장 후 동일 raw bytes만 재전송 | F1, F3, F4, F5, F6, F7, F8 | [`i2_idempotency.rs`](crates/vault/tests/invariants/i2_idempotency.rs)<br>• `T2.1` `same_key_reposted_changes_nothing`<br>• `T2.2` `same_key_different_body_rejected`<br>• `T2.3` `duplicate_block_events_credit_once`<br>• `T2.4` `retried_withdrawal_request_holds_once` | [ADR-001](docs/adr/001-ledger-model.md)<br>[ADR-002](docs/adr/002-reorg-handling.md)<br>[ADR-003](docs/adr/003-nonce-and-rpc-uncertainty.md) |
| **I3 원장 정합성**<br>모든 회계 거래는 차변과 대변이 일치한다 | Rust 분개 검증(`sum == 0`, 2줄 이상) + Postgres `DEFERRABLE INITIALLY DEFERRED` 커밋 시점 제약 트리거 이중 방어 | F2, F7 | [`i3_ledger_balance.rs`](crates/vault/tests/invariants/i3_ledger_balance.rs)<br>• `T3.1` `every_posted_entry_sums_to_zero`<br>• `T3.2` `db_rejects_unbalanced_entry`<br>• `T3.3` `trial_balance_zero_after_reorgs` | [ADR-001](docs/adr/001-ledger-model.md)<br>[ADR-002](docs/adr/002-reorg-handling.md) |
| **I4 블록체인 정합성**<br>입금 확정 여부는 현재 canonical chain과 확정 정책에 따라 결정한다 | 블록 헤더(`number`, `hash`, `parent_hash`) 저장과 부모 해시 대조로 reorg 감지, 확정 전 입금은 역분개(`dep:revert`), 확정 후 깊이의 reorg는 원장 보존 + `finality_violations` 기록 후 정지 | F2, F8 | [`i4_chain_consistency.rs`](crates/vault/tests/invariants/i4_chain_consistency.rs)<br>• `T4.1` `reorg_before_confirmation_reverses_pending`<br>• `T4.2` `reorg_beyond_confirmation_keeps_ledger_and_halts`<br>• `T4.3` `reincluded_tx_confirms_once`<br>• `T4.4` `confirmation_counts_canonical_depth`<br>• `T4.5` `event_order_does_not_change_final_ledger` | [ADR-002](docs/adr/002-reorg-handling.md) |
| **I5 복구 가능성**<br>어떤 처리 단계에서 프로세스가 종료되어도 재시작 후 일관된 상태로 복구한다 | 블록 처리·되감기·출금 상태 전이마다 원장 분개와 커서/상태를 단일 Postgres 트랜잭션으로 커밋, `Signed` 출금은 저장된 `signed_txs.raw`를 재전송 | F3, F7 | [`i5_recovery.rs`](crates/vault/tests/invariants/i5_recovery.rs)<br>• `T5.1` `crash_at_each_injection_point_matches_clean_run`<br>• `T5.2` `transient_db_error_leaves_no_partial_state`<br>• `T5.3` `restart_after_sign_resends_same_tx` | [ADR-001](docs/adr/001-ledger-model.md)<br>[ADR-003](docs/adr/003-nonce-and-rpc-uncertainty.md) |
| **I6 외부 불확실성**<br>RPC 타임아웃이나 응답 유실을 곧바로 트랜잭션 실패로 판단하지 않는다 | `RpcOutcome { Ok, Definite, Indeterminate }` 3분류, 전송 불확실 시 `Unknown` 전이 후 홀드 유지, 확정 영수증·확정 계정 nonce로만 결론(`Confirmed`·`Failed`·`NeedsReview`) | F4, F5, F6 | [`i6_external_uncertainty.rs`](crates/vault/tests/invariants/i6_external_uncertainty.rs)<br>• `T6.1` `send_timeout_moves_to_unknown`<br>• `T6.2` `lost_response_resolved_by_receipt`<br>• `T6.3` `nonce_conflict_never_double_pays`<br>• `T6.4` `resend_uses_identical_raw_tx`<br>• `T6.5` `read_timeout_never_skips_block`<br>• `T6.6` `random_send_faults_pay_at_most_once` | [ADR-002](docs/adr/002-reorg-handling.md)<br>[ADR-003](docs/adr/003-nonce-and-rpc-uncertainty.md) |

### 장애 시나리오 역추적표 ([docs/FAILURE_SCENARIOS.md](docs/FAILURE_SCENARIOS.md))

| ID | 장애 시나리오 | 직접 주입·검증하는 증명 테스트 |
| --- | --- | --- |
| **F1** | 블록 이벤트 중복 수신 | `T2.1`, `T2.3`, `T4.3` |
| **F2** | 블록체인 Reorg (확정 전·확정 후·재포함) | `T1.2`, `T3.3`, `T4.1`, `T4.2`, `T4.3`, `T4.4` + [`crates/vault/tests/anvil.rs`](crates/vault/tests/anvil.rs) |
| **F3** | DB 커밋 직전·직후 프로세스 강제 종료 | `T2.3`, `T5.1`, `T5.3`, `T6.6` |
| **F4** | RPC 타임아웃 및 응답 유실 (`LostResponse`) | `T6.1`, `T6.2`, `T6.5`, `T6.6` |
| **F5** | 출금 핫월렛 Nonce 충돌 (외부 선점·`nonce too low`) | `T6.3`, `T6.6` |
| **F6** | 출금 트랜잭션 재전송 | `T2.4`, `T5.3`, `T6.4`, `T6.6` |
| **F7** | DB 일시 장애 (`40001`, `40P01`, 연결 끊김) | `T2.1`, `T3.2`, `T5.2` |
| **F8** | 블록 알림 순서 역전 및 누락 | `T4.5` |

---

## 아키텍처와 모듈 구성

자세한 구조와 저장소 스키마는 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)에 있다.

| Crate | 모듈 | 핵심 책임 |
| --- | --- | --- |
| [`crates/vault`](crates/vault) | [`ledger`](crates/vault/src/ledger.rs) | 추가 전용 복식부기 원장(`post`, `balance`, `trial_balance`), 멱등 키·내용 해시 검증, savepoint 보호 |
| | [`store`](crates/vault/src/store.rs) | Postgres 풀·마이그레이션(`0001`–`0004`), 트랜잭션 커밋 래퍼(`store::commit`), 장애 주입 경계(`Faults`), 일시 DB 오류 분류 |
| | [`chain`](crates/vault/src/chain.rs) | `ChainClient` trait(`head`, `block_by_number`, `send_raw`, `receipt`, `nonce_at`, `balance_at`), `RpcOutcome` 3분류 |
| | [`deposit`](crates/vault/src/deposit.rs) | 끌어오기(pull) 블록 스캐너(`Scanner`): 부모 해시 체인 검증, reorg 되감기(`dep:revert`), 입금 확정(`dep:confirm`), 운영 자금 확정(`cap:fund`), 최종성 위반 정지 |
| | [`withdrawal`](crates/vault/src/withdrawal.rs) | 출금 상태 기계(`Held → Signed → Sent/Unknown → Confirmed/Failed/NeedsReview`), 단일 트랜잭션 nonce 할당·raw tx 저장 |
| | [`signer`](crates/vault/src/signer.rs) | EIP-1559 결정적 서명(`LocalSigner`, `alloy-consensus`) 및 raw tx 복원(`decode`) |
| | [`recon`](crates/vault/src/recon.rs) | 읽기 전용 온체인 대사(`reconcile`): `HotWalletBalance`, `LedgerEquation`, `NegativeAccount`, `DepositMismatch`, `WithdrawalMismatch` 검출 ([`tests/recon.rs`](crates/vault/tests/recon.rs)) |
| [`crates/vault-sim`](crates/vault-sim) | [`chain`](crates/vault-sim/src/chain.rs) / [`faults`](crates/vault-sim/src/faults.rs) / [`anvil`](crates/vault-sim/src/anvil.rs) | 분기 트리·멤풀·응답 유실을 모사하는 `FakeChain`, 주입 지점 계획 `FaultPlan`, 실제 Foundry Anvil 서브프로세스와 JSON-RPC로 통신하는 `AnvilClient`(`--features anvil`, [`tests/anvil.rs`](crates/vault/tests/anvil.rs)) |

---

## 검증 실행 방법

| 명령 | 설명 |
| --- | --- |
| `scripts/harness/verify.sh fast` | `cargo fmt --check`, `clippy`, 단위 테스트, 불변조건 추적표 일치(`check-invariant-tests.sh`), 가드·변형 하네스 자체 테스트(`test-guards.sh`) |
| `scripts/harness/verify.sh full` | `fast` + Docker Compose Postgres 기동 + DB 통합 테스트(`db`, `ledger`, `deposit`, `withdrawal`, `recon`, `invariants` 24개) + (`anvil` 설치 시) `anvil` 실제 노드 Reorg PoC + 공개 경계 검사(`check-public.sh`) |
| `scripts/harness/mutants.sh` | [`mutants/`](mutants/)의 37개 변형 패치를 임시 사본에 하나씩 적용해 모든 변형이 의도한 단언(`.expect`)으로 잡히는지 검증 (`잡힘 37 가려짐 0 기타 0`) |
| `cargo test -p vault --features "db,anvil" --test anvil` | 실제 `anvil` 프로세스를 띄워 `cap:fund` → 입금 → `evm_snapshot`/`evm_revert` reorg 되감기 → 재포함·확정 → 실제 EIP-1559 서명 출금 → `recon::reconcile` 무결성까지 검증 |

---

## 문서 안내

| 문서 | 내용 |
| --- | --- |
| [docs/SYSTEM_INVARIANTS.md](docs/SYSTEM_INVARIANTS.md) | 불변조건 I1–I6 정의, 판정 문장, 지키는 장치, 증명 테스트 표(T1.1–T6.6) |
| [docs/FAILURE_SCENARIOS.md](docs/FAILURE_SCENARIOS.md) | 장애 시나리오 F1–F8과 기대 동작 |
| [docs/REQUIREMENTS.md](docs/REQUIREMENTS.md) | 도메인 용어, 기능·비기능 요구사항(R1–R9), 범위 밖 결정 |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | crate·모듈 경계, Postgres 테이블과 트랜잭션 경계, 처리 흐름 |
| [docs/adr/001-ledger-model.md](docs/adr/001-ledger-model.md) | ADR-001: 복식부기 원장 모델, 계정 체계, 분개 종류(`dep:*`, `wd:*`, `cap:fund`), 커밋 시점 트리거 |
| [docs/adr/002-reorg-handling.md](docs/adr/002-reorg-handling.md) | ADR-002: 블록 해시 사슬 기반 Reorg 감지와 역분개, 확정 수 N, 최종성 위반 정지 정책 |
| [docs/adr/003-nonce-and-rpc-uncertainty.md](docs/adr/003-nonce-and-rpc-uncertainty.md) | ADR-003: 출금 상태 기계, 단일 트랜잭션 Nonce·서명 저장, RPC 불확실성(`Unknown`·`NeedsReview`) |
| [docs/analysis/reth-reorg.md](docs/analysis/reth-reorg.md) | 오픈소스 분석: `paradigmxyz/reth`(`v1.1.0`)의 `BlockchainTree` Reorg·Unwind·최종성·RPC 조회 경로와 `vault` 비교 |
| [AGENTS.md](AGENTS.md) | 에이전트·기여자 작업 규칙 및 자동 검증 훅 안내 |
