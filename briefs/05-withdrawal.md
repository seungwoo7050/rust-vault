# 05-withdrawal

```
상태: 확정(오케스트레이터)
브랜치: 05-withdrawal (앞 브랜치: 04-crash-recovery @ 33479db)
문제: 출금 tx 전송의 응답이 유실되거나 타임아웃될 때 실패로 보고 다시 서명하면 이중 지급이 나고,
      홀드를 풀면 사용자가 같은 돈을 다시 쓴다. nonce가 겹치거나 재시작 뒤 재전송해도 마찬가지다.
지킬 불변조건: I6, I2, I1, I5 (증명 테스트 T1.3, T2.4, T5.3, T6.1–T6.4, T6.6. T1.2는 출금 API로 다시 겨눈다)
```

설계 원본은 `docs/adr/003-nonce-and-rpc-uncertainty.md`(사람 결정 (a): `NeedsReview`는 홀드 유지, 영수증이 나오면 결론, 시간만으로 `Failed` 없음)와 ADR-001의 출금 분개 키다. 이 브리프는 ADR이 정하지 않은 구현 결정과 완료 기준만 더한다. 구현이 ADR과 달라지면 같은 브랜치에서 ADR을 고친다.

## 결정

- **서명과 tx 인코딩은 alloy**(ARCHITECTURE 결정): `vault::signer::Signer` trait(`address()`, `sign(TxRequest) -> SignedTx { raw: Vec<u8>, hash: Hash32 }`)와 로컬 키 구현. EIP-1559 전송 tx, `gas_limit` 21000, 수수료 상한은 설정값. 서명은 결정적(RFC6979). 테스트 키는 코드에 상수로 둔다(실제 자산 없는 테스트 전용 키라고 주석). 03의 자체 체인 타입과는 `Hash32`·`Address`로만 오간다
  / 대안: 가짜 서명(필드 해시) / 이유: T5.3·T6.4의 "결정적 서명이 변형을 가린다" 문제가 진짜 서명에서만 의미가 있다 / 바꿀 신호: alloy 컴파일 시간이 verify full을 10분 넘게 만들면 기능 플래그 뒤로 옮긴다
- **`ChainClient`에 더할 것**: `send_raw(raw) -> RpcOutcome<Hash32>`(`Definite`는 `AlreadyKnown`, `NonceTooLow`, `ReplacementUnderpriced`, `Other(String)`로 분류), `receipt(hash) -> RpcOutcome<Option<Receipt { block_hash, block_number, success, gas_used, effective_gas_price }>>`, `nonce_at(address, BlockNumber) -> RpcOutcome<u64>`
- **확정 수는 스캐너 기준**: 출금 tx의 확정 수 = 스캐너 커서 번호 − 영수증 블록 번호 + 1이고, 영수증의 블록 해시가 저장된 canonical 헤더에 있어야 한다(ADR-002와 같은 head 정의). "주소의 확정된 nonce"는 `nonce_at(주소, 커서 − N + 1)`. 스캐너가 늦으면 출금 결론도 늦다
  / 대안: 체인 head로 따로 셈 / 이유: 입금과 출금이 서로 다른 head를 쓰면 대사가 어긋난다 / 바꿀 신호: 스캐너 지연이 출금 결론을 운영상 문제될 만큼 늦출 때
- **마이그레이션 `0004_withdrawal.sql`**: `withdrawals(request_id pk, user_id, to_address, amount, status, created_at, status_changed_at)`, `signed_txs(request_id unique, from_address, nonce, raw bytea, tx_hash unique)`, `wallet_nonces(address pk, next_nonce)`. 상태는 ADR-003 결정 2의 `Held, Rejected, Signed, Sent, Unknown, Confirmed, Failed, NeedsReview`
- **출금 API**(`vault::withdrawal`): `request(pool, WithdrawalRequest { request_id, user, to, amount }) -> Result<Withdrawal>`(같은 `request_id`면 기존 행), `Processor::new(pool, chain, signer, clock, faults, config)`, `step() -> Result<Step>`(한 번에 출금 하나의 전이 하나), `run_until_idle()`. 상태는 모두 DB에 있고 새로 만들면 이어 간다(ADR-003 결정 7)
- **`Clock` trait**(`vault::clock`): `now()`. 운영은 시스템 시계, 테스트는 `vault-sim`의 가짜 시계. `NeedsReview` 기한과 04의 재시도 간격이 이것을 쓴다
- **운영 자금**: 출금 주소의 코인은 `cap:fund:{chain_id}:{tx_hash}` 분개(ADR-001)로 들어온다. 이 브랜치에서는 테스트 도우미가 가짜 체인에 자금 tx를 넣고 같은 분개를 직접 기록한다. 스캐너가 운영 자금 입금을 감지하는 것은 06의 대사와 함께 본다
- **`FakeChain` 확장**: 계정별 nonce, 멤풀, `mine()`이 멤풀에서 nonce 순서대로 담는다. 같은 해시를 다시 보내면 멤풀에 있으면 `AlreadyKnown`, 이미 채굴됐으면 `NonceTooLow`. 장애: "반영하고 응답만 버림"(`Indeterminate`), 조회 타임아웃, 외부 tx로 같은 nonce 선점, 영수증이 생겼다 reorg로 사라짐. 노드가 받은 raw tx를 모두 기록해 테스트가 "같은 바이트만 보냈다"를 단언할 수 있게 한다
- **장애 주입**: 출금 상태 전이의 커밋도 04의 커밋 도우미를 거치고 `Site::Withdrawal`을 더한다. 전송 직후 지점 `FaultPoint::AfterSend`(전송 응답을 받고 상태를 쓰기 전)를 더한다(T5.3)
- **확정된 출금의 깊은 reorg**(ADR-003 결정 6): 스캐너의 되감기가 고아가 될 블록의 tx 해시를 `Confirmed` 출금의 `signed_txs.tx_hash`와 대조해, 있으면 `finality_violations`(kind withdrawal) + 정지. `finality_violations`가 하나라도 있으면 `Processor`는 서명·전송을 하지 않는다
- **T1.2**: 03의 직접 홀드 분개를 출금 API `request`로 바꾸고 단언을 표의 원래 문구("요청 `Rejected`")로 돌린다
- **변형 파일**: 8개 증명 테스트마다 표의 "실패해야 할 변형" 그대로. T1.2 변형은 새 경로에 맞게 다시 만든다. 표 밖: `WD-FINALITY`(되감기에서 출금 대조를 뺌), `WD-NEEDSREVIEW-STUCK`(조회 루틴이 `NeedsReview`를 건너뜀 → 결정 (a) 위반)

## 완료 기준

1. `scripts/harness/verify.sh full` 통과. 증명 테스트 24개 모두 ignore 없이 통과(ignored 0)
2. `scripts/harness/mutants.sh` 전체 잡힘(04까지 22 + 이 브랜치)
3. 표 밖 테스트: (가) 확정된 출금 tx가 N보다 깊은 reorg로 빠지면 원장 불변, 사고 기록, 출금 정지 (나) `NeedsReview` 뒤 영수증이 나오면 `Confirmed`/`Failed`로 결론 (다) 영수증 status 0이면 `wd:fee` + `wd:release`, 사용자 잔액 복구, 가스비는 `network_fee` (라) 전송 `Definite(Other)`도 `Unknown`으로 간다
4. ADR-003·ADR-001·docs 표가 구현과 같다(T1.2 문구 복원 포함)
5. `scripts/check-public.sh` 통과(테스트 키가 공개 금지 패턴에 걸리지 않는지 포함)

## 범위 밖

가스비 올려 재전송(replacement), 취소 tx, 여러 출금 주소, 사용자 출금 수수료, sweep, anvil(06)

## 외부 의존

docker(Postgres 17). 새 crate 의존: alloy 계열(consensus, signer-local, primitives 등 필요한 최소). 그 밖에 더하면 보고한다
