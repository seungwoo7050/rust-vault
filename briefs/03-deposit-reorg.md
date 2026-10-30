# 03-deposit-reorg

```
상태: 확정(오케스트레이터)
브랜치: 03-deposit-reorg (앞 브랜치: 02-ledger @ 4b14e37)
문제: canonical에서 빠진 블록의 입금이 남거나, 같은 입금이 두 번 들어가거나, 확정 수를 고아 블록까지 세면
      사용자가 없는 돈을 출금한다. 조회 실패를 빈 블록으로 넘기면 입금을 영영 놓친다.
지킬 불변조건: I4, I1, I2, I3, I6 (증명 테스트 T1.2, T2.3, T3.3, T4.1–T4.5, T6.5)
```

설계 원본은 `docs/adr/002-reorg-handling.md`와 ADR-001의 멱등 키 표다. 이 브리프는 ADR이 정하지 않은 구현 결정과 완료 기준만 더한다. 구현이 ADR과 달라지면 같은 브랜치에서 ADR을 고친다.

## 결정

- **체인 타입은 자체 정의**(`vault::chain`): `BlockNumber(u64)`, `Hash32([u8; 32])`, `Address([u8; 20])`, `BlockHeader { number, hash, parent_hash }`, `Block { header, txs }`, `ChainTx { hash, to, value: Amount, success: bool }`. 로그·트레이스·컨트랙트 입금은 없다
  / 대안: alloy 타입 / 이유: 가짜 체인과 스캐너에는 서명·인코딩이 필요 없고 의존이 크다. alloy는 05의 서명과 06의 anvil 클라이언트에서 이 타입으로 바꿔 넣는다 / 바꿀 신호: 05에서 두 타입을 오가는 변환이 세 곳을 넘으면 alloy 타입으로 통일한다(ARCHITECTURE의 alloy 결정은 그대로)
- **`ChainClient` trait**: `async fn head(&self) -> RpcOutcome<BlockNumber>`, `async fn block_by_number(&self, n) -> RpcOutcome<Option<Block>>`. `RpcOutcome<T> { Ok(T), Definite(RpcError), Indeterminate(String) }`(ADR-003 결정 1). 제네릭으로 받는다(dyn 불필요). `Ok(None)`은 "그 번호의 블록이 아직 없다"이고, head 아래 번호에서 `None`이 오면 기다린다(빈 블록이 아니다)
- **알림**: `Notification::NewHead(BlockHeader)`. 스캐너는 알림의 내용을 쓰지 않고 "스캔하라"는 신호로만 쓴다(ADR-002 결정 2). T4.5의 변형("알림의 블록을 바로 처리")이 걸릴 자리가 여기다
- **스캐너 API**(`vault::deposit::Scanner<C>`): `Scanner::new(pool, chain, config { chain_id, confirmations: N, start: BlockHeader })`, `async fn step(&mut self) -> Result<Step, ScanError>`(블록 하나 처리 또는 되감기 한 번, 할 일이 없으면 `Step::Idle`), `async fn on_notification(&mut self, n)`, `async fn run_until_idle(&mut self)`. 상태는 모두 DB에 있고 `Scanner`를 새로 만들면 DB에서 이어 간다
- **마이그레이션 `0003_deposit.sql`**: `deposit_addresses(address pk, user_id)`, `block_headers(hash pk, number, parent_hash, orphaned bool)`, `scan_cursor`(한 행: number, hash), `deposits(tx_hash, block_hash, address, user_id, amount, status pending|confirmed|reverted, pk(tx_hash, block_hash))`, `finality_violations(id, kind deposit|withdrawal, block_hash, tx_hash, amount, recorded_at)`, `scanner_state`(한 행: halted bool, reason text)
- **커서 시작**: 설정의 `start` 헤더(가짜 체인의 genesis 등)를 처음 한 번 저장하고 커서로 삼는다. 그 아래로는 되감지 않는다(공통 조상을 못 찾으면 ADR-002 결정 8로 정지)
- **블록 처리 트랜잭션**(ADR-002 결정 3): 헤더 INSERT `ON CONFLICT DO NOTHING` → 입금 주소로 온 `success && value > 0` tx마다 `deposits` 행과 잠정 분개 → 이 블록 번호 h 기준 확정 수 N에 이른(번호 ≤ h−N+1, 고아 아님) `pending` 입금의 확정 분개와 상태 갱신 → 커서 조건부 전진(`WHERE (number = h−1 AND hash = parent) OR (number = h AND hash = this)`, 0행이면 오류). 커밋 결과를 모르는 오류는 같은 블록을 다시 처리한다
- **되감기**: 커서에서 아래로 체인의 해시와 저장 해시를 비교한다. 조회 중 `Indeterminate`가 하나라도 나오면 아무것도 쓰지 않고 `ScanError::Indeterminate`로 그 step을 끝낸다. 공통 조상을 찾으면 한 트랜잭션에서: 고아가 될 블록의 `confirmed` 입금이 하나라도 있으면 `finality_violations` 기록 + `scanner_state.halted = true`만 하고 커밋(원장·커서·헤더 불변). 없으면 그 블록들을 고아로 표시하고 `pending` 입금마다 역분개(`dep:revert:…:{block_hash}`), 상태 `reverted`, 커서를 공통 조상으로. 정지 상태에서는 `step`이 `ScanError::Halted`만 돌려준다(재시작해도 유지)
- **확정된 출금의 깊은 reorg 감지**(ADR-002 결정 7의 출금 쪽)는 출금 테이블이 생기는 05에서 더한다
- **장애 주입 자리**: `vault::store::Faults` trait(`fn after_commit(&self, point: &'static str) -> Result<(), Injected>` 등 최소한). 운영 기본값은 아무것도 하지 않는 구현. 스캐너가 블록 커밋 직후 이것을 부르고, 오류면 "커밋 결과 모름"으로 처리한다(T2.3 (나)). 04가 이 trait를 넓혀 재시작 검사를 만든다. `cfg(test)` 분기를 두지 않는다
- **`FakeChain`**(`vault-sim`): 블록 트리. `genesis()`, `mine(parent, txs) -> Hash32`, `set_canonical(tip)`, 번호별 canonical 조회, 해시는 (부모, 번호, 소금)의 sha256으로 결정적. 장애: `fail_next(call, n, Indeterminate)`, 특정 번호의 `None` 응답, 알림 목록 꺼내기(`notifications()`)와 섞기. `ChainClient`를 구현한다
- **T1.2의 출금 홀드**: 출금 모듈은 05에서 생긴다. 이 브랜치에서는 ADR-001의 홀드 분개(`wd:hold:{id}`: 차 `user:u` / 대 `withdrawal_pending:u`)를 원장에 직접 기록해 `InsufficientFunds`를 단언한다. 표의 단언 "요청 `Rejected`"를 "홀드 분개 `InsufficientFunds`(05에서 출금 API의 `Rejected`로 바꾼다)"로 같은 커밋에서 고친다
- **변형 파일**: 9개 증명 테스트마다 `mutants/<ID>.{patch,test}`, 표의 "실패해야 할 변형" 그대로. 그 밖에 표 밖 변형 하나: `mutants/SCAN-HALT`(정지 상태 확인을 뺌 → 정지 뒤에도 처리 계속)

## 완료 기준

1. `scripts/harness/verify.sh full` 통과. T1.2, T2.3, T3.3, T4.1–T4.5, T6.5가 ignore 없이 통과하고, 02의 5개와 합쳐 증명 테스트 14개 통과, 10개 ignored
2. `scripts/harness/mutants.sh`(전체)가 모두 "잡힘": 02의 6개 + 이 브랜치의 10개
3. 표 밖 테스트(`tests/deposit.rs`): (가) 정지 뒤 `Scanner`를 새로 만들어도 정지가 유지되고 원장이 바뀌지 않는다 (나) 되감기 도중 조회가 `Indeterminate`면 헤더·원장·커서가 하나도 바뀌지 않고, 회복 뒤 같은 결과로 되감긴다 (다) value 0 tx와 실패 tx는 입금이 아니다
4. ADR-002·ADR-001·docs 표가 구현과 같다(T1.2 문구 포함). 평가가 대조한다
5. `scripts/check-public.sh` 통과

## 범위 밖

프로세스 종료·재시작 주입 검사(04, 이 브랜치는 `Faults` 자리와 "커밋 결과 모름" 한 지점만), 출금(05), anvil(06), finalized 태그 정책

## 외부 의존

docker(Postgres 17). 새 crate 의존이 생기면 보고한다
