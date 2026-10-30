# AGENTS.md

## 명령

| 명령 | 언제 |
| --- | --- |
| `scripts/harness/verify.sh fast` | 작업 단위마다. Stop 훅이 끝내기 전에 돌린다 |
| `scripts/harness/verify.sh full` | 커밋 전. compose의 Postgres(호스트 포트 55433)로 DB 테스트, 공개 경계 검사까지. CI도 이것만 부른다 |
| `scripts/harness/mutants.sh [ID...]` | 증명 테스트를 더한 뒤. `mutants/<ID>.patch`를 임시 사본에 걸어 `<ID>.test`가 실패(잡힘)하는지 본다 |
| `scripts/harness/check-invariant-tests.sh` | docs 표와 `crates/vault/tests/invariants/`의 이름·ID·ignore 대조(fast에 포함) |
| `scripts/harness/test-guards.sh` | 가드·mutants 도구를 고친 뒤(fast에 포함) |
| `scripts/check-public.sh` | 공개 금지 문자열 검사(full에 포함) |

- DB 테스트는 `--features db`로만 돈다. `DATABASE_URL`은 `.env.example`의 값이 기본이다.
- 불변조건 테스트(`--test invariants`)도 db 기능이 있어야 빌드된다. 그래서 `mutants.sh`로 원장 변형을 볼 때는 Postgres를 먼저 띄우고(`docker compose up -d --wait postgres`) `DATABASE_URL`을 내보낸다.
- 보고하는 숫자는 `verify:` 요약 줄을 그대로 옮긴다.

## 증명 테스트 규칙

- 테스트는 `crates/vault/tests/invariants/iN_*.rs`, 이름은 docs 표와 같다. 구현하면 `#[ignore]`를 떼고 같은 커밋에서 표를 맞춘다.
- 증명 테스트마다 `mutants/<ID>.patch`(표의 "실패해야 할 변형")와 `mutants/<ID>.test`(`cargo test` 뒤 인자)를 더한다. 가려짐이면 입력·단언을 고친다.
- 불변조건 테스트는 `vault` 공개 API와 `vault-sim`만 쓴다(직접 SQL은 T3.2의 우회 쓰기와 단언용 읽기만).
- 테스트마다 새 데이터베이스를 만들고 지운다.

## 금지

- 이미 커밋된 `crates/*/migrations/*.sql` 수정·삭제. 새 번호 파일로 더한다(가드가 막는다).
- 잔액 열 추가. 잔액은 분개 줄 합으로 계산한다(ADR-001).
- 서명한 출금을 다시 서명하는 경로. 저장한 raw tx만 재전송한다(ADR-003).
- `Indeterminate`(타임아웃·연결 끊김·응답 유실)를 `Failed`로 매핑하기, 그것만으로 홀드 해제·빈 블록 처리(I6).
- 운영 코드의 `cfg(test)` 분기로 장애 주입. 주입은 trait 경계(`ChainClient`, store 래퍼, `Clock`)에서만.
- `.env`, `.env.*` 읽기·쓰기(`.env.example`만 쓴다), `git push`(가드가 막는다).
- 불변조건 표(I1–I6) 수정. 두 불변조건이 충돌하면 멈추고 사람에게 묻는다.

## 포인터

- [docs/SYSTEM_INVARIANTS.md](docs/SYSTEM_INVARIANTS.md): 불변조건, 추적표, 증명 테스트 표
- [docs/FAILURE_SCENARIOS.md](docs/FAILURE_SCENARIOS.md), [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
- [docs/adr/001-ledger-model.md](docs/adr/001-ledger-model.md), [002-reorg-handling.md](docs/adr/002-reorg-handling.md), [003-nonce-and-rpc-uncertainty.md](docs/adr/003-nonce-and-rpc-uncertainty.md)
- 훅: `.claude/settings.json` → `scripts/harness/guards/`
