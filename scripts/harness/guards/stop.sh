#!/usr/bin/env bash
# Claude Code Stop 훅. verify.sh fast가 실패하면 끝내지 못하게 막고(종료 코드 2) 요약을 돌려준다.
# 검증 명령은 STOP_VERIFY_CMD로 바꿀 수 있다(기본 scripts/harness/verify.sh fast. test-guards.sh가 스텁을 넣는다).
# 이미 이 훅 때문에 이어 가는 중이면(stop_hook_active) 무한 반복을 막으려고 통과시킨다.
set -uo pipefail
command -v jq >/dev/null || { echo "stop 훅: jq가 없다. jq를 설치한다(검사 없이 통과시키지 않는다)" >&2; exit 2; }
input=$(cat)
[ "$(jq -r '.stop_hook_active // false' <<<"$input")" = "true" ] && exit 0
cwd=$(jq -r '.cwd // ""' <<<"$input")
cd "$(git -C "${cwd:-${CLAUDE_PROJECT_DIR:-$PWD}}" rev-parse --show-toplevel)" || exit 0
if ! out=$(${STOP_VERIFY_CMD:-scripts/harness/verify.sh fast} 2>&1); then
  { echo "verify fast 실패. 고친 뒤 끝낸다:"; grep -E '^(✗|verify:)' <<<"$out"; tail -n 20 <<<"$out"; } >&2
  exit 2
fi
exit 0
