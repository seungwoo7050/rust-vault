#!/usr/bin/env bash
# Claude Code PostToolUse 훅(Edit·Write·MultiEdit). .rs 파일을 썼으면 clippy를 돌리고,
# 실패하면 종료 코드 2와 출력 앞부분을 stderr로 돌려준다.
set -uo pipefail
command -v jq >/dev/null || { echo "post-tool 훅: jq가 없다. jq를 설치한다(검사 없이 통과시키지 않는다)" >&2; exit 2; }
input=$(cat)
path=$(jq -r '.tool_input.file_path // ""' <<<"$input")
case $path in *.rs) ;; *) exit 0 ;; esac
cwd=$(jq -r '.cwd // ""' <<<"$input")
cd "$(git -C "${cwd:-${CLAUDE_PROJECT_DIR:-$PWD}}" rev-parse --show-toplevel)" || exit 0
if ! out=$(cargo clippy -q --workspace --all-targets -- -D warnings 2>&1); then
  { echo "clippy 실패($path 저장 뒤). 고친 뒤 다음 작업으로 간다:"; head -n 40 <<<"$out"; } >&2
  exit 2
fi
exit 0
