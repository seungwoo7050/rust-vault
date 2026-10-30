#!/usr/bin/env bash
# 가드에 훅 입력 JSON 표본을 넣어 종료 코드를 확인하고(막음 2, 통과 0),
# mutants.sh를 픽스처에 돌려 "잡힘 1, 가려짐 1"인지 확인한다.
# 마이그레이션 가드는 임시 git 저장소(0001_init.sql 커밋됨)에서 확인한다.
set -uo pipefail
top=$(git rev-parse --show-toplevel)
guard=$top/scripts/harness/guards/pre-tool.sh

repo=$(mktemp -d "${TMPDIR:-/tmp}/guards.XXXXXX")
trap 'rm -rf "$repo"' EXIT
repo=$(cd "$repo" && pwd -P)
mkdir -p "$repo/crates/vault/migrations" "$repo/crates/vault/src"
echo "CREATE TABLE schema_version (version integer);" >"$repo/crates/vault/migrations/0001_init.sql"
git -C "$repo" init -q && git -C "$repo" add -A &&
  git -C "$repo" -c user.name=guard -c user.email=guard@example.invalid commit -qm init

pass=0 fail=0 blocks=0 passes=0
# expect <2|0> <설명> <JSON>
expect() {
  local want=$1 desc=$2 json=$3 err rc
  err=$(printf '%s' "$json" | env -u CLAUDE_PROJECT_DIR "$guard" 2>&1 >/dev/null); rc=$?
  if [ "$rc" -eq "$want" ] && { [ "$want" -eq 0 ] || [ -n "$err" ]; }; then
    pass=$((pass + 1)); [ "$want" -eq 2 ] && blocks=$((blocks + 1)) || passes=$((passes + 1))
  else
    fail=$((fail + 1)); echo "  ✗ $desc: 종료 코드 $rc(기대 $want) ${err}"
  fi
}
file() { jq -nc --arg t "$1" --arg p "$2" --arg c "$repo" '{tool_name:$t, cwd:$c, tool_input:{file_path:$p}}'; }
bash_() { jq -nc --arg c "$repo" --arg cmd "$1" '{tool_name:"Bash", cwd:$c, tool_input:{command:$cmd}}'; }

# (1) .env
expect 2 ".env.local 쓰기"            "$(file Write "$repo/.env.local")"
expect 2 ".env 읽기"                  "$(file Read "$repo/.env")"
expect 2 "Bash로 .env 읽기"           "$(bash_ 'cat .env')"
expect 0 ".env.example 쓰기"          "$(file Write "$repo/.env.example")"
expect 0 "일반 파일 읽기"             "$(file Read "$repo/crates/vault/src/lib.rs")"
expect 0 "Bash로 .env.example 읽기"   "$(bash_ 'cat .env.example')"
# (2) git push
expect 2 "git push origin"            "$(bash_ 'git push origin 01-harness')"
expect 2 "체인 속 git -C . push"      "$(bash_ 'cargo fmt && git -C . push --force')"
expect 0 "git status"                 "$(bash_ 'git status')"
expect 0 "메시지에 push가 든 커밋"    "$(bash_ 'git commit -m "push 하지 않음"')"
# (3) 커밋된 마이그레이션
expect 2 "커밋된 마이그레이션 Edit"   "$(file Edit "$repo/crates/vault/migrations/0001_init.sql")"
expect 2 "커밋된 마이그레이션 rm"     "$(bash_ 'rm crates/vault/migrations/0001_init.sql')"
expect 2 "커밋된 마이그레이션 덮어쓰기" "$(bash_ 'echo x > crates/vault/migrations/0001_init.sql')"
expect 0 "새 마이그레이션 Write"      "$(file Write "$repo/crates/vault/migrations/0002_ledger.sql")"
expect 0 "커밋된 마이그레이션 Read"   "$(file Read "$repo/crates/vault/migrations/0001_init.sql")"
expect 0 "Bash로 마이그레이션 읽기"   "$(bash_ 'cat crates/vault/migrations/0001_init.sql')"

# Stop 훅: stop_hook_active면 verify를 돌리지 않고 통과
if printf '{"stop_hook_active":true,"cwd":"%s"}' "$repo" | "$top/scripts/harness/guards/stop.sh"; then
  pass=$((pass + 1)); passes=$((passes + 1))
else fail=$((fail + 1)); echo "  ✗ stop_hook_active 통과"; fi
# Stop 훅: stop_hook_active가 거짓이면 검증 명령(STOP_VERIFY_CMD 스텁)의 결과를 따른다
printf '#!/usr/bin/env bash\necho "verify: fmt ✗"; exit 1\n' >"$repo/verify-fail.sh"
printf '#!/usr/bin/env bash\necho "verify: fmt ✓"; exit 0\n' >"$repo/verify-ok.sh"
chmod +x "$repo/verify-fail.sh" "$repo/verify-ok.sh"
# expect_cmd <2|0> <설명> <명령...>: stdin은 비활성 Stop 입력
expect_cmd() {
  local want=$1 desc=$2 err rc; shift 2
  err=$(printf '{"stop_hook_active":false,"cwd":"%s"}' "$repo" | "$@" 2>&1 >/dev/null); rc=$?
  if [ "$rc" -eq "$want" ] && { [ "$want" -eq 0 ] || [ -n "$err" ]; }; then
    pass=$((pass + 1)); [ "$want" -eq 2 ] && blocks=$((blocks + 1)) || passes=$((passes + 1))
  else fail=$((fail + 1)); echo "  ✗ $desc: 종료 코드 $rc(기대 $want) ${err}"; fi
}
expect_cmd 2 "verify 실패 시 Stop 막음" env STOP_VERIFY_CMD="$repo/verify-fail.sh" "$top/scripts/harness/guards/stop.sh"
expect_cmd 0 "verify 통과 시 Stop 통과" env STOP_VERIFY_CMD="$repo/verify-ok.sh" "$top/scripts/harness/guards/stop.sh"

# jq가 없는 PATH(bash만 둔다)에서는 세 훅 모두 막는다. verify는 통과 스텁이라 jq 검사가 없으면 0이 나온다
mkdir -p "$repo/nojq" && ln -s "$(command -v bash)" "$repo/nojq/bash"
for g in pre-tool post-tool stop; do
  expect_cmd 2 "jq 없는 PATH에서 $g 막음" env PATH="$repo/nojq" STOP_VERIFY_CMD="$repo/verify-ok.sh" "$top/scripts/harness/guards/$g.sh"
done

# mutants 픽스처: D1 잡힘, D2 가려짐 → 요약이 "잡힘 1 가려짐 1 기타 0"이고 종료 코드 1
out=$("$top/scripts/harness/mutants.sh" --root "$top/scripts/harness/fixtures/mutants-demo" 2>&1); rc=$?
mut="잡힘/가려짐 기대와 다름"
if [ "$rc" -eq 1 ] && grep -q '^D1 | 잡힘$' <<<"$out" && grep -q '^D2 | 가려짐$' <<<"$out" &&
   grep -q '^mutants: 잡힘 1 가려짐 1 기타 0$' <<<"$out"; then
  pass=$((pass + 1)); mut="잡힘 1 가려짐 1"
else fail=$((fail + 1)); echo "  ✗ mutants 픽스처(종료 코드 $rc):"; sed 's/^/    /' <<<"$out"; fi

echo "test-guards: 통과 $pass 실패 $fail (막음 $blocks, 통과 $passes, mutants 픽스처 $mut)"
[ "$fail" -eq 0 ]
