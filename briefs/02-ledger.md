# 02-ledger

```
상태: 확정(오케스트레이터)
브랜치: 02-ledger (앞 브랜치: 01-harness @ 6025d0c)
문제: 원장이 분개 없이 잔액을 바꾸거나, 같은 사실을 두 번 기록하거나, 차대가 맞지 않는 거래를 받아들이면
      입금·출금 이후 단계가 아무리 맞아도 자산 기록이 틀어진다.
지킬 불변조건: I1, I2, I3 (증명 테스트 T1.1, T2.1, T2.2, T3.1, T3.2)
```

설계 원본은 `docs/adr/001-ledger-model.md`다. 이 브리프는 ADR이 정하지 않은 구현 결정과 완료 기준만 더한다. 구현이 ADR과 달라지면 같은 브랜치에서 ADR을 고친다.

## 결정

- **마이그레이션 `0002_ledger.sql`**: `accounts(key text primary key, kind text check(...6종), created_at)`, `journal_entries(id bigserial, idempotency_key text unique not null, kind text, body_hash bytea not null, created_at)`, `journal_lines(id bigserial, entry_id fk, account_key fk → accounts, amount numeric(78,0) not null check(amount <> 0))`
  / 대안: 계정 id를 정수로 / 이유: `user:42` 같은 키가 테스트·로그·대사 리포트에서 그대로 읽힌다 / 바꿀 신호: 줄 수가 커져 텍스트 FK 인덱스가 느릴 때
- **차대 검사 트리거**: `journal_lines`에 `CONSTRAINT TRIGGER … DEFERRABLE INITIALLY DEFERRED`로, 커밋 시점에 그 분개의 줄 합 = 0, 줄 수 ≥ 2를 검사한다(ADR-001 결정 7)
- **추가 전용 강제**: `journal_entries`·`journal_lines`의 `UPDATE`·`DELETE`를 막는 트리거(예외 발생). ADR-001 결정 2의 "권한 회수" 대신 트리거로 한다
  / 대안: REVOKE / 이유: 테스트·로컬 DB 사용자가 superuser라 REVOKE가 아무것도 막지 않고 검증할 수 없다 / 바꿀 신호: 운영 DB에 전용 역할이 생기면 REVOKE를 더한다(트리거는 유지). ADR-001 결정 2를 이 내용으로 고친다
- **금액 타입**: API는 `Amount(u128)`(0 금지, `i128::MAX` 이하)와 `Side::{Debit, Credit}`. DB 줄 금액은 부호 있는 값(차변 +). Rust ↔ DB는 SQL에서 `::text`/`::numeric` 캐스트로 주고받고 `i128`로 파싱한다
  / 대안: bigdecimal 의존 / 이유: 의존 하나가 늘 뿐이고, rust_decimal은 28자리라 wei를 못 담는다 / 바꿀 신호: 합계가 i128을 넘을 수 있는 자산이 생길 때
- **음수 금지를 모든 계정으로 넓힌다**: 계정마다 정상 잔액 방향(ADR-001 결정 4 표)이 있고, 분개 뒤 그 방향의 잔액이 0 미만이 되면 `InsufficientFunds`로 거부한다. ADR-001 결정 5(사용자 계정만)를 이렇게 고친다
  / 대안: 사용자 계정만 / 이유: 잠정 입금·출금 대기·핫월렛이 음수가 되는 것도 이중 반영이나 역분개 오류의 신호이고, 막지 않으면 03·05의 버그가 원장에 조용히 남는다. 운영 자본·가스비는 정상 방향으로만 쌓이므로 걸리지 않는다 / 바꿀 신호: 정상적인 흐름이 일시적 음수를 필요로 하는 경우가 나오면(예: 대사 전 선지급) 그 계정 종류만 예외로 둔다
- **잠금 순서**: 분개에 나오는 계정 행을 키 순서로 `SELECT … FOR UPDATE`한 뒤 잔액을 계산한다(교착 방지)
- **원장 API**(`vault::ledger`):
  - `open_account(conn, AccountKey) -> Result<()>`: 있으면 그대로(멱등)
  - `post(tx, NewEntry) -> Result<Posted, PostError>`: `Posted::New(EntryId) | Posted::Existing(EntryId)`. `PostError::{Unbalanced, TooFewLines, UnknownAccount(key), InsufficientFunds(key), IdempotencyConflict(key), Db(..)}`
  - `balance(conn, &AccountKey) -> Result<i128>`: 정상 방향 기준 잔액. `trial_balance(conn) -> Result<i128>`: 전체 줄 합(언제나 0이어야 한다)
  - `post`는 받은 트랜잭션 안에 savepoint(sqlx의 중첩 `begin`)를 열고, 거부되면 savepoint를 되돌린 뒤 오류를 돌려준다. 스스로 커밋하지 않는다(ADR-001 결정 8)
  - 검사 순서는 ADR-001 결정 8 그대로: Rust에서 차대 합·줄 수 → savepoint 안에서 분개 INSERT(`ON CONFLICT (idempotency_key) DO NOTHING`) → 기존 키면 body_hash 비교 → 줄 INSERT(계정 FK) → 계정 잠금 후 잔액 검사
- **body_hash**: 분개 kind와 (계정 키, 부호 있는 금액)을 계정 키 순서로 정렬해 정해진 바이트로 인코딩한 sha256. 키에 시각·난수를 넣지 않는다
- **테스트 DB 도우미를 `vault-sim`으로 옮긴다**: `tests/db.rs`의 `TestDb`를 `vault_sim::db::TestDb`(마이그레이션까지 적용한 풀을 돌려줌)로 옮겨 불변조건 테스트와 함께 쓴다
- **불변조건 테스트와 DB**: verify fast는 DB 없이 돌아야 한다. `invariants` 테스트 대상은 `db` 기능이 켜졌을 때만 빌드한다(`[[test]] required-features = ["db"]` 등). check-invariant-tests는 소스를 읽으므로 영향이 없다. 바꾸면 verify의 집계(`ignored`)가 full 기준으로 맞는지 확인한다
- **proptest**: dev-dependency. DB 테스트라 경우 수는 작게(예: 32~64), 실패 시 축소된 예가 출력되게 둔다. 모델은 테스트 안의 메모리 원장(같은 규칙)이다
- **변형 파일**: `mutants/T1.1|T2.1|T2.2|T3.1|T3.2.{patch,test}`. 각 변형은 docs/SYSTEM_INVARIANTS.md 표의 "실패해야 할 변형" 그대로. `.test`는 `--features db --test invariants <필터>` 꼴

## 완료 기준

1. `scripts/harness/verify.sh full` 통과. 불변조건 테스트 T1.1, T2.1, T2.2, T3.1, T3.2가 ignore 없이 통과하고 나머지 19개는 ignored
2. `scripts/harness/mutants.sh T1.1 T2.1 T2.2 T3.1 T3.2`가 다섯 개 모두 "잡힘"(종료 코드 0). 평가가 다시 돌려 확인한다
3. 불변조건 표 밖의 원장 테스트(`tests/ledger.rs`, db 기능): (가) 분개 줄·분개 `UPDATE`·`DELETE`가 거부된다 (나) 두 연결이 같은 멱등 키로 동시에 `post`하면 분개는 1개이고 둘 다 같은 `EntryId`를 받는다 (다) 정상 방향 음수 금지가 `pending_deposit`·`hot_wallet`에도 걸린다
4. ADR-001이 구현과 같다(결정 2의 트리거, 결정 5의 범위, 금액 캐스트). 평가가 대조한다
5. `scripts/check-public.sh` 통과

## 범위 밖

입금·출금 흐름(03, 05), 잔액 스냅숏, 장애 주입 래퍼(04)

## 외부 의존

docker(Postgres 17). 새 crate 의존: proptest(dev), sha2. 그 밖에 더하면 보고한다
