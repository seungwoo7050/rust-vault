#!/usr/bin/env bash
# Claude Code Stop 훅. verify.sh fast가 실패하면 끝내지 못하게 막고(종료 코드 2) 요약을 돌려준다.
# 이미 이 훅 때문에 이어 가는 중이면(stop_hook_active) 무한 반복을 막으려고 통과시킨다.
set -uo pipefail
input=$(cat)
[ "$(jq -r '.stop_hook_active // false' <<<"$input")" = "true" ] && exit 0
cwd=$(jq -r '.cwd // ""' <<<"$input")
cd "$(git -C "${cwd:-${CLAUDE_PROJECT_DIR:-$PWD}}" rev-parse --show-toplevel)" || exit 0
if ! out=$(scripts/harness/verify.sh fast 2>&1); then
  { echo "verify fast 실패. 고친 뒤 끝낸다:"; grep -E '^(✗|verify:)' <<<"$out"; tail -n 20 <<<"$out"; } >&2
  exit 2
fi
exit 0
