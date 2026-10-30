# 01-harness

```
상태: 확정(오케스트레이터)
브랜치: 01-harness (앞 브랜치: 00-charter @ 9957b23)
문제: 에이전트가 불변조건을 깨는 변경(기존 마이그레이션 수정, .env 노출, 검증 없이 끝내기, 항상 통과하는 테스트)을 해도 막을 장치가 없다.
      00-charter 평가에서 "변형이 테스트를 깨뜨리는가"를 문서 추론으로만 판정해 네 번 돌았다(interventions 2026-10-30).
지킬 불변조건: 직접 증명하는 것은 없다. I1–I6 증명 테스트 24개의 자리와 그 테스트를 판정할 도구를 만든다
```

## 결정

- **workspace**: 루트 `Cargo.toml`은 가상 workspace, 멤버 `crates/vault`(lib), `crates/vault-sim`(lib). edition 2024. 루트 `rust-toolchain.toml`로 stable 고정
  / 대안: 루트 패키지 + 하위 crate / 이유: 두 crate가 대등하고 루트에 코드가 없다 / 바꿀 신호: 없음
- **불변조건 테스트 위치**: `crates/vault/tests/invariants.rs`가 진입점이고 `#[path]`로 `crates/vault/tests/invariants/iN_*.rs` 6개를 모듈로 묶는다. 테스트 이름은 `iN_*::함수`(docs/SYSTEM_INVARIANTS.md의 표와 같다). `vault`는 `vault-sim`을 dev-dependency로 받는다(`vault-sim` → `vault` 정방향 의존과 합쳐 dev 순환이지만 통합 테스트에서는 같은 `vault` 하나로 링크된다)
  / 대안: 테스트를 `vault-sim/tests`에 / 이유: 읽는 사람은 `vault`의 테스트를 `vault`에서 찾는다 / 바꿀 신호: dev 순환 때문에 타입 불일치 컴파일 오류가 나면 `vault-sim/tests`로 옮기고 docs 표의 경로를 고친다
- **테스트 뼈대**: SYSTEM_INVARIANTS의 T1.1–T6.6 24개를 같은 이름으로 만들고 `#[ignore = "NN-브랜치"]`(표의 브랜치)를 단다. 본문은 `unimplemented!("T1.1: <넣는 입력과 단언 한 줄>")`. 테스트 위 주석에 ID와 "실패해야 할 변형"을 한 줄로 옮긴다
  / 대안: 빈 본문 / 이유: ignore를 실수로 떼면 빈 테스트는 통과한다. unimplemented는 실패한다 / 바꿀 신호: 없음
- **표와 뼈대의 일치 검사**: `scripts/harness/check-invariant-tests.sh`가 docs 표의 (ID, 테스트 이름, 브랜치)와 코드의 테스트 이름·ignore 사유를 대조한다. verify fast에 넣는다
  / 대안: 사람이 맞춤 / 이유: 02 이후 표와 코드가 어긋나는 것을 기계로 잡는다 / 바꿀 신호: 없음
- **verify.sh**: `scripts/harness/verify.sh fast|full`.
  fast = `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`(DB 없는 테스트), check-invariant-tests, 가드 테스트.
  full = fast + docker compose로 Postgres 띄우기(`compose.yaml`, postgres 17, 호스트 포트는 5433처럼 겹치지 않게) + DB 테스트(`--features db` 또는 `DATABASE_URL`이 있을 때만 도는 테스트) + `scripts/check-public.sh`.
  마지막 줄에 `verify: fmt ✓ clippy ✓ test a/b db c/d ignored e` 모양의 요약 한 줄을 찍는다(ORCHESTRATION 3절의 보고 형식). 숫자는 cargo 출력에서 센다
  / 대안: Makefile, cargo xtask / 이유: 훅·CI·사람이 같은 명령 하나를 부른다. xtask는 crate가 하나 늘어난다(crate 2개는 사람 결정) / 바꿀 신호: 스크립트가 200줄을 넘으면 xtask 검토(사람에게 묻는다)
- **DB 스모크 테스트**: `vault`에 `store::connect`(sqlx PgPool)와 빈 마이그레이션 디렉터리(`crates/vault/migrations/`, 이 브랜치에는 `0001_init.sql`로 스키마 버전 표만)를 두고, full에서 연결·마이그레이션 실행을 확인한다. 테스트마다 새 데이터베이스를 만들어 지운다(ARCHITECTURE 결정)
  / 대안: 02에서 처음 / 이유: docker·포트·권한 문제를 하네스 단계에서 드러낸다 / 바꿀 신호: 없음
- **Claude Code 훅**(`.claude/settings.json`, 커밋한다. `settings.local.json`은 그대로 gitignore):
  - PreToolUse 가드 `scripts/harness/guards/pre-tool.sh`: (1) `.env`, `.env.*`(`.env.example` 제외) 읽기·쓰기 차단 (2) Bash의 `git push` 차단 (3) 이미 커밋된 `crates/*/migrations/*.sql` 수정·삭제 차단(새 파일 추가는 허용). 막을 때 이유와 대신 할 일을 한 줄로 돌려준다
  - PostToolUse(Edit·Write가 `.rs`를 썼을 때): `cargo clippy -q --workspace --all-targets -- -D warnings`. 실패하면 출력 앞부분을 돌려준다
  - Stop: `verify.sh fast`. 실패하면 끝내지 못하게 막고 요약을 돌려준다. 무한 반복을 막기 위해 훅 입력의 `stop_hook_active`가 참이면 통과시킨다
  / 대안: git pre-commit 훅만 / 이유: 에이전트는 커밋 전에도 잘못된 일을 한다. 커밋 훅은 CI에서 같은 검사로 대신한다 / 바꿀 신호: 훅이 작업을 자주 잘못 막으면(같은 오탐 2번) interventions에 적고 가드를 고친다
- **가드 테스트**: `scripts/harness/test-guards.sh`가 각 가드에 훅 입력 JSON 표본을 넣어 종료 코드를 확인한다. 가드마다 막아야 할 예와 통과해야 할 예를 최소 2개씩(예: `.env.local` 쓰기 차단 / `.env.example` 쓰기 통과, `git push origin` 차단 / `git status` 통과, 커밋된 마이그레이션 수정 차단 / 새 마이그레이션 추가 통과). verify fast에 넣는다
- **변형 검사 도구** `scripts/harness/mutants.sh`(interventions 2026-10-30의 가드): `mutants/<ID>.patch`(구현에 거는 변형 diff)와 `mutants/<ID>.test`(그 변형에서 실패해야 할 테스트 필터)를 읽어, 임시 worktree에 패치를 적용하고 그 테스트를 돌려 **실패하면 잡힘, 통과하면 가려짐**으로 보고한다. 작업 트리는 바꾸지 않는다. 결과 표: `ID | 잡힘/가려짐/패치 실패`. 가려짐이 하나라도 있으면 종료 코드 1
  - 자체 테스트: `scripts/harness/fixtures/mutants-demo/`(workspace 밖 작은 crate, 루트 Cargo.toml에서 exclude)에 함수 하나와 테스트 둘, 변형 둘을 둔다. 하나는 테스트가 잡고, 하나는 다른 장치에 가려진다. `test-guards.sh`가 mutants.sh를 이 픽스처에 돌려 "잡힘 1, 가려짐 1"을 확인한다
  - 02부터 각 구현 브랜치는 증명 테스트마다 `mutants/` 파일을 더하고, 평가 단계는 문서 추론 대신 `mutants.sh` 결과로 판정한다(ORCHESTRATION 2-1의 3단계)
  / 대안: cargo-mutants / 이유: 무작위 변형이라 "이 불변조건을 깨는 그 변형"을 고를 수 없고 느리다 / 바꿀 신호: 손으로 쓴 변형 밖의 구멍을 찾고 싶을 때 06 이후 보조로 추가
- **CI**: `.github/workflows/verify.yml`이 `scripts/harness/verify.sh full`만 부른다(services로 Postgres 대신 같은 compose를 쓴다). 원격이 없으므로 이 브랜치에서는 파일만 두고, 실제 실행은 `blocked`에 적는다
- **AGENTS.md**: 100줄 이하. 명령(verify fast/full, mutants), 금지(기존 마이그레이션 수정, 잔액 열 추가, 다시 서명하는 경로, `Indeterminate`를 실패로 매핑, `cfg(test)` 분기로 장애 주입), 포인터(docs/SYSTEM_INVARIANTS.md, ADR). 설명 문단은 두지 않는다
- **check-public.sh**: ai-work 브랜치의 같은 스크립트를 코드 브랜치 `scripts/check-public.sh`로 복사한다(푸시 전 검사가 코드 브랜치에서도 돌아야 한다)

## 완료 기준

1. `scripts/harness/verify.sh full`이 통과하고 마지막 줄 요약을 찍는다. 불변조건 테스트 24개가 ignored로 집계된다
2. `scripts/harness/test-guards.sh`: 가드 3종 × (막을 것 ≥ 2, 통과할 것 ≥ 2)와 mutants 픽스처의 "잡힘 1, 가려짐 1"이 모두 기대대로다
3. `cargo test --workspace -- --ignored` 를 돌리면 24개 모두 실패한다(ignore를 떼도 통과하는 빈 테스트가 없다)
4. check-invariant-tests: docs 표 하나의 테스트 이름을 일부러 바꾸면 실패한다(평가가 변형으로 확인, 커밋하지 않음)
5. 가드 변형: pre-tool 가드에서 `git push` 검사를 지우면 test-guards가 실패한다(평가가 확인)
6. AGENTS.md ≤ 100줄, `scripts/check-public.sh` 통과

## 범위 밖

원장·체인 코드(02, 03), 실제 CI 실행(원격 없음), cargo-mutants

## 외부 의존

docker(Postgres 17 이미지). jq(훅 입력 JSON 파싱. 없으면 python3로 대신하고 그 사실을 보고한다)
