# ADR-001 원장 모델

상태: 확정: 오케스트레이터 · 지키는 불변조건: I1, I2, I3, I5

## 맥락

입금, reorg로 인한 되돌림, 출금 홀드, 지급, 가스비가 모두 사용자 자산을 움직인다. 이 이동이 어디서 와서 어디로 갔는지 남지 않으면 I1(승인된 거래 없이 잔액 변경 금지)을 판정할 수 없다. 재시도·중복 이벤트가 일상인 환경이라 같은 사실이 여러 번 기록되려 할 때 한 번만 반영되어야 한다(I2).

## 대안

| 안 | 내용 | 고르지 않은 이유 |
| --- | --- | --- |
| A. 잔액 열 갱신 | `accounts.balance`를 `UPDATE … SET balance = balance + x` | 잔액이 왜 그 값인지 남지 않는다. 갱신 하나가 빠지거나 두 번 되면 찾을 근거가 없다. I1을 테스트로 판정할 수 없다 |
| B. 복식부기 분개, 잔액은 분개 합 | 추가 전용 `journal_entries`·`journal_lines`, 잔액은 `SUM(lines.amount)` | **고름** |
| C. 이벤트 소싱 + 잔액 프로젝션 | 도메인 이벤트를 저장하고 잔액은 별도 프로젝션 | 프로젝션과 이벤트 사이 정합성이라는 문제가 하나 더 생긴다. B의 분개가 이미 이벤트 로그 역할을 한다 |

## 결정

1. **분개와 줄.** 계정(`accounts`)은 텍스트 키(`user:42`처럼 결정 4 표의 꼴)가 기본 키이고 `kind`(6종 CHECK)를 갖는다. 분개(`journal_entries`)는 `id`, `idempotency_key`(유일), `kind`, `created_at`, 내용 해시(`body_hash`)를 갖는다. 줄(`journal_lines`)은 `entry_id`, `account_key`(→ `accounts.key`), `amount`(`NUMERIC(78,0)`, 0 금지, 차변 +, 대변 −)를 갖는다. 줄이 2개 이상이고 합이 0이어야 한다(I3). 마이그레이션은 `0002_ledger.sql`.

   금액은 API에서 `Amount(u128)`(0 금지, `i128::MAX` 이하)와 `Side::{Debit, Credit}`로 받고, DB와는 SQL의 `::text`/`::numeric` 캐스트로 주고받아 Rust에서 `i128`로 파싱한다. 합이 `i128`을 넘으면 디코드 오류다(바꿀 신호 참고). 내용 해시는 분개 `kind`와 (계정 키, 부호 있는 금액)을 정렬해 길이를 붙여 인코딩한 sha256이고, 멱등 키·시각·난수는 넣지 않는다.
2. **추가 전용.** 분개와 줄은 `UPDATE`·`DELETE`하지 않는다. 고칠 일은 역분개(부호를 뒤집은 새 분개)로만 한다. 두 테이블의 `UPDATE`·`DELETE`(행마다)와 `TRUNCATE`를 예외를 내는 트리거로 막는다. 권한 회수(`REVOKE`)는 쓰지 않는다. 테스트·로컬 DB 사용자가 superuser라 `REVOKE`가 아무것도 막지 않고 검증할 수도 없어서다. 운영 DB에 전용 역할이 생기면 `REVOKE`를 더한다(트리거는 유지).
3. **잔액은 분개 합.** 잔액 열을 두지 않는다. `balance(account) = SUM(amount)`이고 계정의 정상 잔액 방향에 따라 부호를 읽는다.
4. **계정 종류 6개.** ROADMAP의 4개에 가스비와 운영 자본을 더했다. 가스비가 갈 곳이 없으면 핫월렛의 원장 잔액과 온체인 잔액이 어긋나 06의 대사가 성립하지 않는다. 출금 주소에 운영자가 넣어 두는 코인도 분개 없이 들어오면 같은 이유로 대사가 깨진다.

   | 종류 | 계정 예 | 성격 | 정상 잔액 |
   | --- | --- | --- | --- |
   | `user` | `user:{user_id}` | 부채(사용자에게 줄 돈, 쓸 수 있음) | 대변 |
   | `pending_deposit` | `pending_deposit:{user_id}` | 부채(아직 쓸 수 없음) | 대변 |
   | `withdrawal_pending` | `withdrawal_pending:{user_id}` | 부채(홀드된 출금) | 대변 |
   | `hot_wallet` | `hot_wallet:{address}` | 자산(우리가 키를 가진 주소 하나의 온체인 코인. 사용자 입금 주소와 출금용 핫월렛 주소 모두) | 차변 |
   | `network_fee` | `network_fee` | 비용(가스비) | 차변 |
   | `operator_capital` | `operator_capital` | 자본(운영자가 출금 주소에 넣은 코인) | 대변 |

5. **모든 계정은 정상 방향으로 음수 금지.** 분개를 기록한 뒤 그 분개에 나오는 계정 중 하나라도 정상 잔액 방향(결정 4 표)의 잔액이 0 미만이 되면 `InsufficientFunds(계정)`로 거부한다. 사용자 계정의 음수 금지(I1)가 여기에 들어 있다. 잠정 입금·출금 대기·핫월렛이 음수가 되는 것도 이중 반영이나 역분개 오류의 신호이고, 막지 않으면 03·05의 버그가 원장에 조용히 남는다. 운영 자본·가스비는 정상 방향으로만 쌓이므로 걸리지 않는다. 정상적인 흐름이 일시적 음수를 필요로 하면(예: 대사 전 선지급) 그 계정 종류만 예외로 둔다.

   검사와 기록은 같은 savepoint 안에서, 분개에 나오는 계정 행을 키 순서로 잠근 뒤 한다(교착 방지). 잠금은 `SELECT … FOR NO KEY UPDATE`다. 잠금끼리는 `FOR UPDATE`처럼 서로 배제하지만, 앞서 줄 `INSERT`의 FK 검사가 잡아 둔 `KEY SHARE` 잠금과는 충돌하지 않는다. `FOR UPDATE`면 같은 계정에 줄을 넣은 두 트랜잭션이 서로의 `KEY SHARE`를 기다려 교착한다.
6. **멱등 키.** 키는 외부 사실로만 만든다(아래 표). 같은 키가 이미 있으면 내용 해시를 비교해, 같으면 기존 분개 ID를 돌려주고 아무것도 쓰지 않으며, 다르면 `IdempotencyConflict` 오류다(I2). 키를 시각·난수·수신 순번으로 만들지 않는다.

   | 분개 | 키 | 줄 |
   | --- | --- | --- |
   | 잠정 입금 | `dep:pending:{chain_id}:{tx_hash}:{block_hash}` | 차 `hot_wallet:{입금 주소}` / 대 `pending_deposit:u` |
   | 잠정 입금 역분개 | `dep:revert:{chain_id}:{tx_hash}:{block_hash}` | 차 `pending_deposit:u` / 대 `hot_wallet:{입금 주소}` |
   | 입금 확정 | `dep:confirm:{chain_id}:{tx_hash}` | 차 `pending_deposit:u` / 대 `user:u` |
   | 출금 홀드 | `wd:hold:{request_id}` | 차 `user:u` / 대 `withdrawal_pending:u` |
   | 출금 지급 | `wd:settle:{request_id}` | 차 `withdrawal_pending:u` 금액, 차 `network_fee` 가스비 / 대 `hot_wallet:{출금 주소}` 금액+가스비 |
   | 출금 취소 | `wd:release:{request_id}` | 차 `withdrawal_pending:u` / 대 `user:u` |
   | 운영 자금 입금 | `cap:fund:{chain_id}:{tx_hash}` | 차 `hot_wallet:{출금 주소}` / 대 `operator_capital` |
   | 실패 tx 가스비 | `wd:fee:{chain_id}:{tx_hash}` | 차 `network_fee` / 대 `hot_wallet:{출금 주소}` |

   입금 주소의 코인을 출금 주소로 모으는 sweep은 범위 밖이다. 출금 주소는 운영 자금 입금으로 채운다(아래 바꿀 신호).

   잠정 입금 키에 블록 해시를 넣는 까닭은 reorg 뒤 같은 tx가 다른 블록에 다시 들어갈 때 새 잠정 분개가 필요해서다. 확정 키에는 넣지 않는다. 확정은 tx 하나에 한 번뿐이어야 한다([ADR-002](002-reorg-handling.md)).
7. **두 겹 검증.** Rust 쪽에서 분개를 만들 때 합 0과 줄 2개 이상을 검사하고, DB에 커밋 시점 제약 트리거(`DEFERRABLE INITIALLY DEFERRED`)를 둬 SQL로 직접 넣어도 불일치 분개가 커밋되지 않게 한다(I3). Rust 검사만 있으면 다른 경로(마이그레이션, 수동 SQL)를 막지 못한다.
8. **한 트랜잭션.** 분개 기록은 호출자가 넘긴 DB 트랜잭션 안에서 한다. 원장 모듈이 스스로 커밋하지 않는다. 그래야 03·04에서 블록 헤더·커서와 같은 트랜잭션에 묶인다(I5). 분개 하나는 그 트랜잭션 안의 savepoint에서 기록한다. 쓰기 전에 Rust에서 거르는 것은 차대 합과 줄 수뿐이고, 계정 존재는 FK로, 멱등 키는 유일 제약으로, 잔액 부족은 계정 행을 잠근 뒤 같은 savepoint 안에서 판정한다. 그래서 DB 오류가 트랜잭션 전체를 중단시키지 않도록 savepoint가 필요하다(`post(tx, NewEntry)`는 받은 트랜잭션에 sqlx의 중첩 `begin`으로 savepoint를 연다). 같은 키를 두 트랜잭션이 동시에 기록하면 뒤의 `INSERT … ON CONFLICT DO NOTHING`이 앞의 커밋을 기다렸다가 기존 분개를 찾으므로 분개는 하나이고 둘 다 같은 ID를 받는다. 거부(차대 불일치, 없는 계정, 잔액 부족, 멱등 키 충돌)되면 savepoint로 되돌린 뒤 오류를 돌려준다. 호출자는 그 트랜잭션을 계속 쓰고 커밋할 수 있고, 거부된 분개의 흔적은 남지 않는다(I1).

## 검증

[SYSTEM_INVARIANTS](../SYSTEM_INVARIANTS.md#증명-테스트)의 T1.1, T2.1, T2.2, T3.1, T3.2(02-ledger), T5.1, T5.2(04-crash-recovery, 결정 8의 한 트랜잭션), T1.3, T2.4(05-withdrawal).

## 바꿀 신호

| 신호 | 바꿀 것 |
| --- | --- |
| 계정당 줄이 많아져 잔액 조회가 블록 처리 시간의 의미 있는 몫이 될 때 | 분개와 같은 트랜잭션에서 갱신하는 잔액 스냅숏 테이블. 스냅숏 = 분개 합을 검사하는 테스트를 함께 둔다 |
| 입금 주소에 쌓인 코인을 출금에 써야 할 때 | sweep 분개(`hot_wallet:{입금 주소}` → `hot_wallet:{출금 주소}`, 가스비는 `network_fee`)와 sweep tx를 출금과 같은 상태 기계로 처리 |
| 토큰이나 두 번째 체인이 생길 때 | 계정에 자산 ID 열을 더하고, 차대 합 0을 자산별로 검사한다 |
| 사용자 출금 수수료가 생길 때 | 수익 계정 종류를 더한다 |
| 합계가 `i128`을 넘을 수 있는 자산이 생길 때 | 금액을 임의 정밀도 정수 타입(bigdecimal 등)으로 주고받는다 |
| 운영 DB에 원장 전용 역할이 생길 때 | 결정 2의 트리거에 더해 두 테이블의 `UPDATE`·`DELETE` 권한을 회수한다 |
| 동시에 여러 writer가 원장을 쓰게 될 때 | 사용자 계정 행 잠금만으로 부족한지 다시 본다(직렬화 격리 수준 검토) |
