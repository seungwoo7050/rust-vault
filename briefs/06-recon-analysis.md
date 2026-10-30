# 06-recon-analysis

```
상태: 확정(오케스트레이터)
브랜치: 06-recon-analysis (앞 브랜치: 05-withdrawal @ a5cb30f)
문제: 원장 내부에서 차대 합이 0(I3)이더라도, 스캐너가 놓친 온체인 이동(운영 자금 입금, 외부 전송)이나
      수동 개입·버그로 온체인 잔액·영수증과 원장 기록이 어긋나면 발견할 수단이 없다.
      또한 가짜 체인에서 증명한 reorg·확정 가정이 실제 노드(reth, anvil)의 동작과 맞는지 대조해야 한다.
지킬 불변조건: I1, I2, I3, I4, I5, I6 전부의 사후 대사(R9) 및 실제 노드 대조
```

설계 원본은 `docs/REQUIREMENTS.md`(R9), `docs/ARCHITECTURE.md`(`recon` 모듈과 `AnvilClient`), `docs/adr/001-ledger-model.md`(계정 종류 6개와 `cap:fund`), `ROADMAP.md`의 06 완료 기준과 README 개편 규칙이다.

## 결정

- **`ChainClient`에 잔액 조회 추가**: `balance_at(&self, address: Address, block: BlockNumber) -> impl Future<Output = RpcOutcome<u128>> + Send`. `FakeChain`은 genesis부터 `block`까지의 canonical 경로(`path_to`)에서 받은 코인(`tx.to == address && tx.success`인 `tx.value`)에서 보낸 코인(`senders`의 `from == address`인 성공 tx의 `value + fee`, 실패 tx의 `fee`)을 빼고, 대사 불일치 주입용 보정값(`adjust_balance(address, delta: i128)`)을 더한 값을 돌려준다
  / 대안: 대사 전용 별도 trait / 이유: 스캐너·출금 처리기와 같은 `ChainClient`를 써야 가짜 체인과 `AnvilClient` 모두에서 대사를 그대로 돌릴 수 있다 / 바꿀 신호: 대사가 아카이브 전용 RPC를 따로 써야 할 때
- **운영 자금 입금 스캔(`cap:fund:{chain_id}:{tx_hash}`)**: `ScanConfig`에 `hot_wallet: Option<Address>`를 더한다(기본 `None`이면 기존과 같음). 설정된 출금 주소로 들어온 성공한 코인 전송(`tx.to == hot_wallet && tx.value > 0 && tx.success`, 입금 주소가 아닌 것)은 그 블록이 확정 수 `N`에 이르렀을 때(`confirm_ready` 시점, canonical 높이가 `cursor.number - N + 1` 이하인 블록) `cap:fund:{chain_id}:{tx_hash}` 분개(차 `hot_wallet:{출금 주소}` / 대 `operator_capital`)를 한 번 기록한다. 확정된 블록이 깊은 reorg로 빠질 때는 `Confirmed` 입금·출금과 마찬가지로 `finality_violations`에 기록하고 정지한다
  / 대안: 블록 감지 즉시 잠정 분개 후 확정 / 이유: ADR-001에 운영 자금 잠정·역분개 계정이 없고, 출금 주소의 코인은 확정된 뒤 쓰는 것이 안전하며 키 모양(`cap:fund:{chain_id}:{tx_hash}`, 블록 해시 없음)과 맞는다 / 바꿀 신호: 미확정 운영 자금도 대사에 즉시 보여야 할 때
- **대사 모듈(`vault::recon`)**: `reconcile(pool, chain, ReconConfig { chain_id, confirmations, hot_wallet }) -> Result<ReconReport, ReconError>`. 대사는 DB를 바꾸지 않는 읽기 전용이다(몇 번을 돌려도 같은 입력이면 같은 `ReconReport`를 돌려준다). 스캐너 커서(`cursor`)가 있으면 확정 높이 `confirmed_block = cursor.number - N + 1`(포화 뺄셈)을 구해 다음 네 범주의 불일치(`Discrepancy`)를 모두 수집한다:
  1. `HotWalletBalance { address, at_block, on_chain: u128, ledger: i128 }`:
     - 등록된 각 사용자 입금 주소(`deposit_addresses`): 스캐너는 `cursor.number`까지의 잠정 입금을 이미 `hot_wallet:{입금 주소}` 차변에 반영하므로 `cursor.number` 시점의 온체인 잔액과 원장 `hot_wallet:{입금 주소}` 차변 잔액을 비교한다(단, `cursor.number`와 `confirmed_block` 사이에 아직 확정되지 않은 잠정 입금이 있으면 `cursor.number` 기준, 둘 다 일치해야 함).
     - 출금 핫월렛 주소(`hot_wallet`): 원장의 출금 정산(`wd:settle`), 실패 가스비(`wd:fee`), 운영 자금 입금(`cap:fund`)은 모두 `confirmed_block` 이하의 블록에 대해서만 반영되므로, `confirmed_block` 시점의 온체인 잔액(단, `confirmed_block` 이하에서 채굴됐으나 아직 `Processor`가 결론 내지 않은 `Sent`/`Unknown`/`NeedsReview` 출금이 있으면 그 온체인 차감액을 고려하거나 `UnconcludedOnChainWithdrawal`로 함께 리포트)과 원장 `hot_wallet:{출금 주소}` 차변 잔액을 비교한다.
  2. `LedgerEquation { assets: i128, liabilities_and_equity_minus_fee: i128 }` 및 `NegativeAccount { account: AccountKey, balance: i128 }`: 모든 `hot_wallet:*` 차변 합 = `user:*` + `pending_deposit:*` + `withdrawal_pending:*` + `operator_capital` 대변 합 − `network_fee` 차변 합 등식 위반이나 정상 방향 음수 잔액 계정을 찾는다.
  3. `DepositMismatch { tx_hash, block_hash, reason }`: `deposits` 테이블의 상태(`detected` → `dep:pending` 존재 및 `dep:revert`·`dep:confirm` 없음, `confirmed` → `dep:pending`·`dep:confirm` 존재 및 금액 일치, `reverted` → `dep:pending`·`dep:revert` 존재)와 원장 분개·온체인 블록의 tx가 어긋난 경우, 또는 스캐너가 처리한 canonical 블록(`..=cursor`)에 입금 주소로 들어온 성공 tx가 있는데 `deposits` 행이 없는 경우를 찾는다.
  4. `WithdrawalMismatch { request_id, tx_hash, reason }`: `Confirmed` 출금인데 `wd:settle:{request_id}` 분개가 없거나 온체인 영수증(canonical 포함, `success == true`, 금액·가스비)과 다른 경우, `Failed` 출금인데 `wd:release:{request_id}`(및 가스비가 0보다 클 때 `wd:fee`)가 없거나 온체인에 성공 영수증이 있는 경우, `Held`/`Signed`/`Sent`/`Unknown`/`NeedsReview`인데 `wd:hold:{request_id}`가 없는 경우를 찾는다.
- **`anvil` 기반 실제 노드 reorg PoC(`--features anvil`)**:
  - `vault` 또는 `vault-sim`에 `anvil` 기능 플래그를 두고, 로컬 `anvil` 프로세스를 띄워 JSON-RPC(`eth_blockNumber`, `eth_getBlockByNumber`, `eth_getBalance`, `eth_getTransactionCount`, `eth_sendRawTransaction`, `eth_getTransactionReceipt`, `evm_snapshot`, `evm_revert`, `evm_mine`, `anvil_setBalance`)로 통신하는 `AnvilClient`(`ChainClient` 구현)를 만든다.
  - 통합 테스트(`crates/vault/tests/anvil.rs`, `--features "db,anvil"`): 실제 `anvil` 노드에서 (1) 운영 자금 입금 및 사용자 입금 전송 → 스캐너 잠정 입금 기록, (2) 확정 전 `evm_revert` + 더 긴 분기 채굴로 실제 체인 reorg 발생 → 스캐너가 부모 해시 불일치로 되감기(`Rewound`)와 역분개(`dep:revert`) 수행, (3) 새 분기에서 입금 재포함 및 확정 수 `N` 도달(`dep:confirm`) → 출금 요청·서명(`alloy`)·실제 `anvil` 전송·영수증 확정(`Confirmed`) → `recon::reconcile` 결과가 불일치 0건(`discrepancies.is_empty()`)임을 검증한다.
  - `scripts/harness/verify.sh`의 `full` 모드에서 `anvil`(`command -v anvil` 또는 `$HOME/.foundry/bin/anvil`)이 있으면 `--features "db,anvil" --test anvil`도 함께 돌린다.
- **`docs/analysis/reth-reorg.md`**:
  - `paradigmxyz/reth`의 실제 커밋 SHA 하나를 고정하고, 모든 주장에 `https://github.com/paradigmxyz/reth/blob/<commit>/<path>#L<start>-L<end>` 형태의 고정 링크를 단다(존재하지 않는 파일이나 추측 줄 번호 금지).
  - 추적할 세 경로와 `vault` 대조:
    1. **체인 분기·reorg 실행 경로**: CL의 `engine_newPayload*` / `engine_forkchoiceUpdated*`에서 트리 핸들러가 새 canonical head가 기존 tip의 자식이 아닐 때 공통 조상을 찾고 구 분기 블록들을 `unwind`(상태 번들 역적용)한 뒤 신 분기 블록들을 실행·커밋하는 경로 ↔ `vault::deposit::Scanner`의 `extends_cursor` → `find_common_ancestor` → `rewind`(`dep:revert`) 대조.
    2. **Finality 처리 경로**: `ForkchoiceState.finalized_block_hash` / `safe_block_hash` 갱신과 finalized 높이 아래로의 reorg 거부/가지치기 ↔ `vault`의 깊이 `N` 확정 정책 및 `finality_violations` 정지(ADR-002 결정 6, ADR-003 결정 6) 대조.
    3. **RPC 조회와 불확실성**: `eth_getBlockByNumber`, `eth_getTransactionReceipt`, `eth_getTransactionCount`(`nonce_at`)가 canonical 체인/상태를 읽는 시점과 reorg 시 영수증이 사라졌다 다시 나타날 수 있는 이유 ↔ `vault::withdrawal::Processor`가 `Unknown`을 체인 조회로만 결론 내고 확정 높이를 스캐너 커서 기준으로 맞추는 이유 대조.
- **`mutants.sh`의 기대 실패 이유 검사(`.expect`)**:
  - `mutants/<ID>.expect` 파일이 있으면(선택), 변형 테스트가 실패했을 때 출력 로그에 그 파일의 패턴(`grep -Fq` 또는 줄별 패턴)이 나오는지도 확인한다. 패턴이 없으면 `"다른 이유로 실패"`(`other`)로 세어 실패 처리한다.
  - 04·05에서 다른 이유로 먼저 실패할 뻔했던 변형들(`T1.3`, `T5.1`, `T5.3`, `T6.4`, `WD-NONCE-SPLIT`, `WD-NONCE-TOO-LOW-FAILED`)과 06의 새 대사 변형에 `.expect` 파일을 둔다.
- **변형 파일 추가**:
  - `RECON-BALANCE`: `recon`에서 온체인 잔액과 원장 `hot_wallet` 잔액 비교를 생략
  - `RECON-WITHDRAWAL`: `recon`에서 출금 영수증·분개 대조를 생략
- **`README.md` 다시 쓰기**:
  - 첫 화면을 **불변조건 추적표(불변조건 I1–I6 → 장애 시나리오 F1–F8 → 증명 테스트 경로 T1.1–T6.6 → ADR)** 중심으로 다시 쓴다.
  - 프로젝트 구조, 검증 명령(`verify.sh fast|full`, `mutants.sh`), 대사·`reth` 분석·`anvil` PoC 포인터, 기여 표기(`출처: 사람 설계` vs `확정: 오케스트레이터`, `ORCHESTRATION.md` 7절 규칙 준수)를 담는다.

## 완료 기준

1. `scripts/harness/verify.sh full` 통과(24개 불변조건 증명 테스트 + 대사 테스트 + `anvil` PoC 테스트 모두 통과, ignored 0)
2. `scripts/harness/mutants.sh` 전체 잡힘(기존 34개 + `RECON-BALANCE`, `RECON-WITHDRAWAL` = 36개, `.expect`가 있는 변형은 기대한 단언 메시지로 실패함을 확인)
3. 대사 테스트(`crates/vault/tests/recon.rs`):
   - 정상 흐름(운영 자금 입금 스캔 `cap:fund` → 사용자 입금·reorg·확정 → 출금 확정 및 실패 tx 가스비) 뒤 `reconcile` 결과가 불일치 0건이고, 연속 두 번 돌려도 결과가 완전히 같다(멱등)
   - 심어 둔 불일치(① 온체인 잔액 변조 `HotWalletBalance`, ② 입금 분개 금액/상태 불일치 또는 누락 `DepositMismatch`, ③ 출금 정산 분개 누락 또는 영수증 불일치 `WithdrawalMismatch`, ④ 원장 등식 불일치)를 모두 빠짐없이 찾아낸다
4. `docs/analysis/reth-reorg.md`의 모든 주장마다 `paradigmxyz/reth`의 고정 커밋 SHA 링크와 실제 파일:줄 번호가 있고, 엔진(`vault`)의 가정과 대조표가 있다
5. `README.md` 첫 화면이 불변조건 추적표(불변조건 → 장애 시나리오 → 증명 테스트 경로 → ADR)이고 기여 표기가 정확하며, `scripts/check-public.sh` 통과

## 범위 밖

아카이브 노드 전용 트레이스 대사, ERC-20 토큰 대사, 자동 보정 분개(대사는 리포트만 하고 원장을 고치지 않는다)

## 외부 의존

docker(Postgres 17), anvil(`~/.foundry/bin/anvil`)
