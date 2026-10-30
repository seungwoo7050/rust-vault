#!/usr/bin/env bash
# 손으로 쓴 변형이 증명 테스트를 실제로 깨뜨리는지 본다.
#   <root>/mutants/<ID>.patch   구현에 거는 변형 diff(<root> 기준 경로, git apply 형식)
#   <root>/mutants/<ID>.test    `cargo test` 뒤에 붙일 인자(# 줄은 주석). 예: -p vault --test invariants -- i1_asset_safety::
#   <root>/mutants/<ID>.expect  (선택) 실패 출력에 반드시 들어 있어야 할 문자열(#·빈 줄 제외, 줄마다 grep -Fq).
#                               변형이 의도한 단언이 아니라 엉뚱한 패닉으로 죽으면 "다른 이유로 실패"로 분류한다.
# 작업 트리의 임시 사본에 패치를 걸고 그 테스트를 돌린다. 실패하면 잡힘, 통과하면 가려짐.
# 사본마다 패치 없이 먼저 돌려(기준) 원래 통과하는지, 테스트가 1개 이상 도는지도 확인한다.
# 작업 트리는 바꾸지 않는다. 잡힘이 아닌 결과가 하나라도 있으면 종료 코드 1.
# 쓰는 법: scripts/harness/mutants.sh [--root DIR] [ID...]
set -uo pipefail
top=$(git rev-parse --show-toplevel)
root=$top
if [ "${1:-}" = "--root" ]; then root=$(cd "$2" && pwd -P); shift 2; fi
dir=$root/mutants
export CARGO_TARGET_DIR=${MUTANTS_TARGET_DIR:-$top/target/mutants}

ids=("$@")
if [ "${#ids[@]}" -eq 0 ]; then
  for p in "$dir"/*.patch; do [ -e "$p" ] && ids+=("$(basename "$p" .patch)"); done
fi
if [ "${#ids[@]}" -eq 0 ]; then echo "mutants: 변형 없음 ($dir)"; exit 0; fi

tmp=$(mktemp -d "${TMPDIR:-/tmp}/mutants.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

# 추적 파일과 무시되지 않은 미추적 파일(커밋 전 변경 포함)을 사본으로 옮긴다.
# 사본들이 CARGO_TARGET_DIR를 같이 쓰고 cargo는 mtime으로 신선도를 보므로, 원본 mtime을 살리면
# 앞 사본의 변형 빌드가 다음 사본의 기준 실행에 그대로 쓰인다. 그래서 풀 때 mtime을 지금으로 둔다(-m)
copy_tree() {
  mkdir -p "$1"
  (cd "$root" && git ls-files -co --exclude-standard -z . | xargs -0 tar cf - 2>/dev/null) | (cd "$1" && tar xmf -)
}

# cargo test 출력에서 돈 테스트 수(passed + failed)를 센다
ran() { awk '/^test result:/ { gsub(/;/, ""); n += $4 + $6 } END { print n + 0 }' "$1"; }

# .expect 파일이 있으면 주석·빈 줄을 뺀 모든 줄이 $2 로그에 들어 있는지 확인한다
check_expect() {
  local exp=$1 log=$2 line
  [ ! -f "$exp" ] && return 0
  while IFS= read -r line || [ -n "$line" ]; do
    [[ $line =~ ^[[:space:]]*(#|$) ]] && continue
    grep -Fq -- "$line" "$log" || return 1
  done <"$exp"
}

declare -a rows
caught=0 masked=0 other=0
for id in "${ids[@]}"; do
  patch=$dir/$id.patch test=$dir/$id.test copy=$tmp/$id log=$tmp/$id.log
  if [ ! -f "$patch" ] || [ ! -f "$test" ]; then rows+=("$id | 파일 없음(.patch/.test)"); other=$((other + 1)); continue; fi
  read -ra args <<<"$(grep -vE '^[[:space:]]*(#|$)' "$test" | tr '\n' ' ')"
  copy_tree "$copy"

  if ! (cd "$copy" && cargo test "${args[@]}") >"$log" 2>&1; then
    result="기준 실패(패치 없이도 실패)"
  elif [ "$(ran "$log")" -eq 0 ]; then
    result="테스트 없음(필터에 맞는 테스트가 0개)"
  elif ! (cd "$copy" && git apply "$patch") >>"$log" 2>&1; then
    result="패치 실패"
  elif ! (cd "$copy" && cargo test --no-run "${args[@]}") >"$log" 2>&1; then
    result="빌드 실패"
  elif (cd "$copy" && cargo test "${args[@]}") >"$log" 2>&1; then
    result="가려짐"
  elif ! check_expect "$dir/$id.expect" "$log"; then
    result="다른 이유로 실패($id.expect 불일치)"
  else
    result="잡힘"
  fi
  case $result in 잡힘) caught=$((caught + 1)) ;; 가려짐) masked=$((masked + 1)) ;; *) other=$((other + 1)) ;; esac
  rows+=("$id | $result")
done

echo "ID | 결과"
printf '%s\n' "${rows[@]}"
echo "mutants: 잡힘 $caught 가려짐 $masked 기타 $other"
[ "$masked" -eq 0 ] && [ "$other" -eq 0 ]
