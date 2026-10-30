# 오픈소스 분석: `paradigmxyz/reth`의 Reorg 감지·되감기·최종성 처리와 `vault` 비교

확정: 오케스트레이터.

이 문서는 이더리움 실행 클라이언트(Execution Layer) 오픈소스인 [`paradigmxyz/reth`](https://github.com/paradigmxyz/reth)가 체인 분기 교체(reorg)를 어떻게 감지·되감기(unwind)하고, 최종성(`safe`·`finalized`) 경계와 RPC 상태 조회(`eth_getBalance`, `eth_getTransactionReceipt`, `eth_getTransactionCount`)에서 어떤 정합성 규칙을 지키는지를 코드 경로 단위로 추적한다. 끝에서는 실행 노드 내부의 상태 관리 방식과, 그 노드를 RPC 너머 외부 시스템으로 바라보는 `vault`(`deposit::Scanner`, `withdrawal::Processor`, `recon::reconcile`)의 설계 차이를 대조한다.

- **분석 대상 저장소**: [`https://github.com/paradigmxyz/reth`](https://github.com/paradigmxyz/reth)
- **고정 커밋 SHA**: [`dfcaad4608797ed05a8b896fcfad83a83a6292af`](https://github.com/paradigmxyz/reth/tree/dfcaad4608797ed05a8b896fcfad83a83a6292af) (`v1.1.0` 릴리스 태그)

---

## 1. 체인 트리(`BlockchainTree` / `EngineApiTreeHandler`)의 Reorg 감지와 되감기(Unwind)

이더리움 PoS(The Merge 이후)에서 실행 클라이언트는 스스로 정본 사슬(canonical chain)을 고르지 않고, 합의 클라이언트(Consensus Layer)가 Engine API의 `engine_forkchoiceUpdatedV*`로 알려 주는 `headBlockHash`·`safeBlockHash`·`finalizedBlockHash`를 따라 정본 사슬을 바꾼다. `reth`에서는 이 로직이 메모리 내 부속 사슬(sidechain) 버퍼와 디스크 DB(MDBX) 위에서 돌아간다.

### 1.1 관련 파일과 심볼

- [`crates/engine/tree/src/tree/mod.rs`](https://github.com/paradigmxyz/reth/blob/dfcaad4608797ed05a8b896fcfad83a83a6292af/crates/engine/tree/src/tree/mod.rs):
  - `EngineApiTreeHandler::on_forkchoice_updated` — `ForkchoiceState` 수신 시 현재 canonical tip과 `state.head_block_hash`를 비교하고, 새 head가 다른 분기에 있으면 공통 조상부터 이어지는 체인 교체 경로를 호출한다.
  - `EngineApiTreeHandler::on_new_head` / `find_canonical_header` — 대상 블록이 이미 실행되어 메모리 트리에 있는지 확인하고, 옛 canonical 구간(`old_blocks`)과 새 분기 구간(`new_blocks`)을 나눈다.
- [`crates/blockchain-tree/src/blockchain_tree.rs`](https://github.com/paradigmxyz/reth/blob/dfcaad4608797ed05a8b896fcfad83a83a6292af/crates/blockchain-tree/src/blockchain_tree.rs):
  - `BlockchainTree::make_canonical` — 지정한 블록 해시를 새 canonical head로 전환한다.
  - `BlockchainTree::find_fork_point` — 부속 사슬 블록들의 `parent_hash`를 거슬러 올라가 현재 canonical 사슬과 만나는 공통 조상(`BlockNumHash`)을 찾는다.
  - `BlockchainTree::unwind` — 디스크(MDBX)에 이미 내려간 canonical 블록들을 공통 조상 번호까지 되감는다.
- [`crates/chain-state/src/notifications.rs`](https://github.com/paradigmxyz/reth/blob/dfcaad4608797ed05a8b896fcfad83a83a6292af/crates/chain-state/src/notifications.rs):
  - `CanonStateNotification::Commit { new: Arc<Chain> }` — 단순 사슬 연장.
  - `CanonStateNotification::Reorg { old: Arc<Chain>, new: Arc<Chain> }` — 분기 교체 시 빠져나간 옛 사슬(`old`)과 새로 정본이 된 사슬(`new`)을 함께 구독자(RPC 캐시, 트랜잭션 풀, ExEx 등)에게 브로드캐스트한다.

### 1.2 처리 순서: 공통 조상 탐색 → 옛 분기 되감기 → 새 분기 적용

1. **부모 해시 연결 검사**:
   새로 지정된 `head_block_hash`의 부모가 현재 canonical head와 같으면 단순 연장(`CanonStateNotification::Commit`)이다. 그렇지 않으면 reorg 경로로 들어간다.
2. **공통 조상(`fork_point`) 탐색**:
   `find_fork_point`는 새 분기 블록의 `parent_hash` 사슬을 따라 내려가면서 (a) 메모리 내 canonical 사슬 인덱스(`CanonicalChain`)에 그 해시가 있는지, 없으면 (b) 디스크 DB(`HeaderProvider::header_by_hash`)의 정본 헤더에 있는지를 확인해 분기 시작점(`fork_point`)을 특정한다.
3. **되감기(Unwind)와 상태 복원**:
   `reth`는 최근 블록들의 실행 결과(`BundleState`, 계정 잔액·nonce·스토리지 변경 전후 값인 revert 정보 포함)를 메모리에 들고 있다.
   - 만약 `fork_point`가 이미 디스크 DB에 커밋된 높이보다 아래에 있다면, `ProviderFactory`의 쓰기 트랜잭션 안에서 `unwind_to(fork_point.number)`를 호출해 계정·스토리지 변경분(Changesets)을 역적용하고 정본 헤더·트랜잭션 번호 인덱스를 지운다.
   - 메모리 트리 안에만 있던 블록들은 옛 canonical 사슬에서 떼어 내 부속 사슬로 되돌리고, 그 위에 새 분기 블록들의 `BundleState`를 정본 상태로 승격한다.
4. **트랜잭션 풀(Mempool) 및 구독자 통지**:
   `CanonStateNotification::Reorg { old, new }`가 발행되면 트랜잭션 풀 유지 태스크(`maintain_transaction_pool`)는 `old` 사슬에만 들어 있던 트랜잭션들을 다시 풀에 넣고(`reinserted`), `new` 사슬에서 이미 채굴된 트랜잭션들을 풀에서 제거한다.

---

## 2. 최종성(`safe` / `finalized`) 추적과 비가역 경계

### 2.1 관련 파일과 심볼

- [`crates/blockchain-tree/src/canonical_chain.rs`](https://github.com/paradigmxyz/reth/blob/dfcaad4608797ed05a8b896fcfad83a83a6292af/crates/blockchain-tree/src/canonical_chain.rs) 및 [`crates/blockchain-tree/src/state.rs`](https://github.com/paradigmxyz/reth/blob/dfcaad4608797ed05a8b896fcfad83a83a6292af/crates/blockchain-tree/src/state.rs):
  - `TreeState::remove_old_blocks` / `BlockchainTree::finalize_block`
- [`crates/blockchain-tree-api/src/error.rs`](https://github.com/paradigmxyz/reth/blob/dfcaad4608797ed05a8b896fcfad83a83a6292af/crates/blockchain-tree-api/src/error.rs):
  - `BlockchainTreeError::PendingBlockIsFinalized { last_finalized: BlockNumber }`

### 2.2 최종성 경계 규칙

- 합의 클라이언트가 `finalizedBlockHash`를 갱신하면 `reth`는 그 블록 높이(`finalized_number`) 이하의 canonical 블록들을 디스크 DB에 영구 반영(persist)하고, 메모리 내 부속 사슬(`sidechains`)에서 `finalized_number` 이하의 대안 분기들을 모두 정리(`remove_old_blocks`)한다.
- 만약 어떤 새 블록이나 `forkchoiceUpdated` 요청이 `last_finalized` 블록 높이 이하로 되감기를 시도하면, `BlockchainTreeError::PendingBlockIsFinalized` 오류를 돌려주어 finalized 경계 아래의 reorg를 거부한다.

---

## 3. RPC 상태·영수증·Nonce 조회 경로(`eth_getBalance`, `eth_getTransactionReceipt`, `eth_getTransactionCount`)

외부 입출금 시스템(`vault`)은 노드 내부 메모리를 직접 읽지 못하고 JSON-RPC(`eth_*`)로만 체인을 본다. `reth`의 RPC 계층이 블록 지정자(`BlockId`)와 reorg를 다루는 방식은 외부 시스템이 왜 확정 수(confirmations)와 블록 해시 검증을 직접 해야 하는지를 보여 준다.

### 3.1 관련 파일과 심볼

- [`crates/rpc/rpc/src/eth/helpers/state.rs`](https://github.com/paradigmxyz/reth/blob/dfcaad4608797ed05a8b896fcfad83a83a6292af/crates/rpc/rpc/src/eth/helpers/state.rs):
  - `LoadState::balance` (`eth_getBalance`) — `state_at_block_id_or_latest(block_id)`로 해당 높이의 `StateProviderBox`를 열어 `basic_account(&address)`의 `balance`를 돌려준다.
  - `LoadState::transaction_count` (`eth_getTransactionCount`) — `block_id`가 `Pending`이면 멤풀의 pending nonce를 합산하고, 그 밖의 블록 번호·태그면 해당 블록 시점 상태의 `nonce`를 돌려준다.
- [`crates/rpc/rpc/src/eth/helpers/receipt.rs`](https://github.com/paradigmxyz/reth/blob/dfcaad4608797ed05a8b896fcfad83a83a6292af/crates/rpc/rpc/src/eth/helpers/receipt.rs):
  - `LoadReceipt::transaction_receipt` (`eth_getTransactionReceipt`) — `transaction_and_receipt(hash)`를 통해 현재 canonical 사슬(메모리 `CanonStateTracker` + 디스크 정본 인덱스)에 포함된 트랜잭션의 영수증만 조립해 돌려준다.
- [`crates/storage/provider/src/providers/state/latest.rs`](https://github.com/paradigmxyz/reth/blob/dfcaad4608797ed05a8b896fcfad83a83a6292af/crates/storage/provider/src/providers/state/latest.rs) & `historical.rs`:
  - `LatestStateProviderRef` / `HistoricalStateProviderRef` — 특정 블록 높이(`BlockNumber`)의 계정 잔액·nonce를 현재 정본 상태 또는 Changeset 역산으로 조회한다.

### 3.2 외부 시스템 관점에서의 시사점

1. **블록 번호 조회는 언제나 "지금 시점의 canonical 사슬" 기준이다**:
   `eth_getBlockByNumber(h)`, `eth_getBalance(addr, h)`, `eth_getTransactionCount(addr, h)`는 호출 시점의 canonical 사슬에서 높이 `h`를 읽는다. 따라서 직전 조회와 지금 조회 사이에 reorg가 일어났다면 같은 블록 번호 `h`가 전혀 다른 블록 해시와 잔액을 가리킬 수 있다.
2. **고아 블록의 영수증은 사라지거나 다른 블록 해시로 재포함된다**:
   `eth_getTransactionReceipt(tx_hash)`는 트랜잭션이 고아 분기로 밀려나면 `null`(`None`)을 돌려주거나, 새 분기의 다른 블록에 재포함된 뒤에는 바뀐 `blockHash`·`blockNumber`가 담긴 영수증을 돌려준다.
3. **연속된 두 RPC 호출은 원자적 스냅샷이 아니다**:
   `eth_blockNumber`를 읽은 직후 `eth_getBlockByNumber`나 `eth_getBalance`를 부르는 사이에도 노드의 canonical tip은 바뀔 수 있다. 따라서 클라이언트 쪽에서 각 블록의 `parent_hash == prev.hash` 불변식을 직접 확인해야 한다.

---

## 4. `reth`(실행 노드 내부) 대 `vault`(외부 원장·입출금 시스템) 비교

| 비교 항목 | `paradigmxyz/reth` (실행 클라이언트 내부) | `vault` (외부 커스터디·원장 시스템) |
| --- | --- | --- |
| **시스템 경계와 역할** | P2P와 합의 클라이언트(`Engine API`)로부터 블록을 받아 EVM 상태를 직접 실행·저장하는 정본 상태 제공자 | JSON-RPC(`ChainClient`) 너머에서 블록·영수증을 끌어와(pull) 자체 Postgres 복식부기 원장과 출금 상태 기계를 운영하는 클라이언트 |
| **Reorg 감지 메커니즘** | `on_forkchoice_updated`에서 새 `headBlockHash`가 현재 canonical tip의 자손이 아님을 확인하고 `find_fork_point`로 메모리 부속 사슬·디스크 헤더를 역추적 | `deposit::Scanner`가 `cursor.number + 1` 블록의 `parent_hash`를 DB에 저장된 `scan_cursor.hash`와 대조(`ADR-002` 결정 2). 불일치 시 `block_headers`와 `chain.block_at(h)`를 거슬러 공통 조상을 탐색 |
| **상태 되감기(Rollback) 방식** | **파괴적 되감기(State Unwind)**: 옛 분기의 `BundleState`·MDBX Changeset을 역적용해 계정 잔액·nonce·스토리지를 과거 시점으로 직접 덮어씀 | **비파괴적 역분개(Compensating Journal Entry)**: 이미 쓴 분개 줄은 절대 지우거나 고치지 않는다(`I1`, `I3`). `deposits.status = 'reverted'` 표시와 함께 `dep:revert:{chain_id}:{tx_hash}:{block_hash}` 역분개를 추가 기록하고 헤더를 `orphaned = true`로 표시 |
| **빠진 트랜잭션의 재포함(Reinclusion)** | `CanonStateNotification::Reorg`가 빠진 tx를 트랜잭션 풀에 다시 넣고, 새 분기 블록 실행 시 새 블록 컨텍스트로 다시 실행 | 잠정 입금·역분개 멱등 키에 `block_hash`를 포함(`dep:pending:...:block_hash`)해 다른 블록에서의 재포함을 허용하고, 확정 키(`dep:confirm:{chain_id}:{tx_hash}`)에는 `block_hash`를 빼서 생애주기 전체에서 최대 한 번만 확정(`T4.3`, `ADR-002` 결정 5) |
| **최종성(Finality) 경계와 위반 대응** | `finalizedBlockHash` 이하의 부속 사슬을 삭제하고, 그 아래로 되감는 요청은 `PendingBlockIsFinalized` 오류로 거부 | 확정 수 `N` 이상 쌓여 `dep:confirm` 또는 `cap:fund`·출금 `Confirmed`가 나간 블록이 reorg로 빠지면 원장을 되돌리지 않고 `finality_violations`에 사고를 기록한 뒤 `scanner_state.halted = true`로 스캐너를 즉시 정지(`T4.2`, `ADR-002` 결정 6) |
| **출금·Nonce 및 RPC 불확실성** | 노드는 멤풀과 정본 상태의 nonce를 직접 알고 서명 검증만 수행 | RPC 타임아웃·응답 유실을 `RpcOutcome::Indeterminate`로 분리(`I6`). 서명한 raw tx를 전송 전에 DB(`signed_txs`)에 커밋하고(`T5.3`), 재전송은 언제나 같은 raw tx만 보내며(`T6.4`, `ADR-003`), 결론은 확정 영수증·확정 계정 nonce로만 판정 |
| **대사(Reconciliation) 기준점** | `StateProvider`가 특정 `BlockId`의 계정 잔액·영수증을 제공 | `recon::reconcile`이 읽기 전용으로 입금 주소는 `cursor` 블록 높이의 온체인 잔액과, 출금 핫월렛은 `confirmed_block`(`cursor − N + 1`) 높이의 온체인 잔액(미결 채굴 출금 차감 보정 포함)과 원장 `hot_wallet:*` 잔액·분개·영수증을 대조 |

---

## 5. `FakeChain`·`AnvilClient` 모델링과의 연결

1. **부모 해시 사슬과 분기 교체(`FakeChain` & `AnvilClient`)**:
   `reth`가 `forkchoiceUpdated`에서 옛 분기를 내리고 새 분기를 정본으로 삼듯, `vault-sim::FakeChain`은 블록 트리와 canonical 경로를 유지하며, `vault-sim::anvil::AnvilClient`(`--features anvil`)는 실제 EVM 노드(`anvil`)의 `evm_snapshot` / `evm_revert`와 더 긴 분기 채굴로 같은 높이에서 블록 해시가 바뀌는 실제 JSON-RPC reorg를 재현한다(`crates/vault/tests/anvil.rs`).
2. **대사(`vault::recon`)의 높이 분리**:
   `reth`의 `eth_getBalance(address, block_number)`가 정본 사슬의 특정 블록 높이 스냅샷을 돌려주는 성질을 이용해, `vault::recon::reconcile`은 스캐너가 잠정 입금을 즉시 기록하는 입금 주소는 `cursor` 높이에서, 확정 수 `N` 뒤에 정산하는 출금 핫월렛(`cap:fund`, `wd:settle`, `wd:fee`)은 `confirmed_block` 높이에서 온체인 잔액과 원장 잔액을 정확히 맞춘다(`crates/vault/tests/recon.rs`).
