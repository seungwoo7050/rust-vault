#!/usr/bin/env bash
# Claude Code PreToolUse 가드. 훅 입력 JSON을 stdin으로 받는다.
# 막으면 종료 코드 2와 함께 "이유. 대신 할 일" 한 줄을 stderr로 돌려준다(Claude가 읽는다). 통과는 0.
#   (1) .env, .env.*(.env.example 제외) 읽기·쓰기
#   (2) Bash의 git push
#   (3) 이미 커밋된 crates/*/migrations/*.sql 수정·삭제(새 파일 추가는 허용)
set -uo pipefail
command -v jq >/dev/null || { echo "pre-tool 가드: jq가 없다. jq를 설치한다" >&2; exit 2; }

input=$(cat)
tool=$(jq -r '.tool_name // ""' <<<"$input")
cwd=$(jq -r '.cwd // ""' <<<"$input")
[ -n "$cwd" ] || cwd=${CLAUDE_PROJECT_DIR:-$PWD}
root=$(git -C "$cwd" rev-parse --show-toplevel 2>/dev/null || echo "$cwd")
root=$(cd "$root" 2>/dev/null && pwd -P || echo "$root")

block() { echo "막음: $1" >&2; exit 2; }

# 경로를 저장소 루트 기준 상대 경로로 바꾼다(루트 밖이면 절대 경로 그대로)
relpath() {
  local p=$1 d
  case $p in /*) ;; *) p="$cwd/$p" ;; esac
  d=$(cd "$(dirname "$p")" 2>/dev/null && pwd -P) && p="$d/$(basename "$p")"
  case $p in "$root"/*) echo "${p#"$root"/}" ;; *) echo "$p" ;; esac
}

is_env_name() { # .env, .env.local 등. .env.example은 아니다
  local b; b=$(basename "$1")
  [[ $b =~ ^\.env(\..+)?$ ]] && [ "$b" != ".env.example" ]
}

is_committed_migration() {
  [[ $1 =~ ^crates/[^/]+/migrations/[^/]+\.sql$ ]] && git -C "$root" cat-file -e "HEAD:$1" 2>/dev/null
}

check_path() {
  local path=$1 rel
  [ -n "$path" ] || return 0
  is_env_name "$path" && block "$path 는 비밀 값 파일이라 읽고 쓰지 않는다. 필요한 키 이름은 .env.example에 적는다"
  rel=$(relpath "$path")
  if [ "$tool" != "Read" ] && [ "$tool" != "Grep" ] && is_committed_migration "$rel"; then
    block "$rel 는 이미 커밋된 마이그레이션이라 고치지 않는다. 바꿀 내용은 다음 번호의 새 마이그레이션 파일로 더한다"
  fi
  return 0
}

check_bash() {
  local cmd=$1 seg tok rel
  # (1) 명령 속 .env 경로 토큰
  while IFS= read -r tok; do
    tok=$(sed -E 's/^[^.]//; s/[^A-Za-z0-9_.-]$//' <<<"$tok")
    [ -n "$tok" ] && [ "$tok" != ".env.example" ] &&
      block "명령이 $tok 를 건드린다. 비밀 값 파일은 읽고 쓰지 않는다. 필요한 키 이름은 .env.example에 적는다"
  done < <(grep -oE '(^|[^A-Za-z0-9_.-])\.env(\.[A-Za-z0-9_-]+)*([^A-Za-z0-9_.-]|$)' <<<"$cmd")

  # 명령을 조각으로 나눈다(; && || | ( ) ` 따옴표)
  local segs; segs=$(perl -pe 's/(&&|\|\||;|\||\(|\)|`|"|\x27)/\n/g' <<<"$cmd")
  while IFS= read -r seg; do
    # (2) git push (git -C dir push, VAR=x git push, sudo git push 포함)
    if grep -qE '^[[:space:]]*((sudo|command|exec|env|nohup|time)[[:space:]]+)*([A-Za-z_][A-Za-z0-9_]*=[^[:space:]]*[[:space:]]+)*git([[:space:]]+(-C|-c|--git-dir|--work-tree)[[:space:]]+[^[:space:]]+|[[:space:]]+-[^[:space:]]+)*[[:space:]]+push([[:space:]]|$)' <<<"$seg"; then
      block "git push는 하지 않는다(원격 반영은 사람이 한다). 커밋까지만 하고 보고에 브랜치와 커밋을 적는다"
    fi
    # (3) 커밋된 마이그레이션을 지우거나 바꾸는 명령
    while IFS= read -r tok; do
      rel=$(relpath "$tok")
      is_committed_migration "$rel" || continue
      if grep -qE '(^|[[:space:]])(rm|mv|truncate|tee|git[[:space:]]+(rm|mv|checkout|restore)|sed[[:space:]]+(-[a-zA-Z]*[[:space:]]+)*-i|perl[[:space:]]+-[a-zA-Z]*i)([[:space:]]|$)' <<<"$seg" ||
         grep -qE ">[[:space:]]*$(sed 's/[.[\*^$/]/\\&/g' <<<"$tok")" <<<"$seg" ||
         grep -qE "(^|[[:space:]])cp[[:space:]].*[[:space:]]$(sed 's/[.[\*^$/]/\\&/g' <<<"$tok")[[:space:]]*$" <<<"$seg"; then
        block "$rel 는 이미 커밋된 마이그레이션이라 지우거나 고치지 않는다. 바꿀 내용은 다음 번호의 새 마이그레이션 파일로 더한다"
      fi
    done < <(grep -oE '[^[:space:]<>]*migrations/[^[:space:]<>]+\.sql' <<<"$seg")
  done <<<"$segs"
  return 0
}

case $tool in
  Bash) check_bash "$(jq -r '.tool_input.command // ""' <<<"$input")" ;;
  *) check_path "$(jq -r '.tool_input.file_path // .tool_input.notebook_path // .tool_input.path // ""' <<<"$input")" ;;
esac
exit 0
