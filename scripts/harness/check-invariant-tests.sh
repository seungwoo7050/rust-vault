#!/usr/bin/env bash
# docs/SYSTEM_INVARIANTS.md 증명 테스트 표의 (ID, 테스트 이름, 브랜치)와
# crates/vault/tests/invariants/의 (주석 ID, 모듈::함수, ignore 사유)를 대조한다.
# ignore가 없는 테스트는 구현된 것으로 보고 브랜치를 비교하지 않는다.
# 쓰는 법: scripts/harness/check-invariant-tests.sh [docs 파일] [테스트 디렉터리]
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
doc=${1:-docs/SYSTEM_INVARIANTS.md}
dir=${2:-crates/vault/tests/invariants}
entry=$(dirname "$dir")/invariants.rs

# 표: | T1.1 | 시나리오 | `mod::fn` | ... | 브랜치 |
want=$(awk -F' \\| ' '/^\| T[0-9]+\.[0-9]+ \|/ {
  id=$1; sub(/^\| /, "", id); name=$3; gsub(/`/, "", name); br=$NF; sub(/ \|$/, "", br)
  print id "|" name "|" br }' "$doc" | sort)

# 코드: // T1.1 ... / #[test] / #[ignore = "브랜치"] / fn 이름(  (테스트 속성이 없는 보조 함수는 건너뛴다)
have=$(for f in "$dir"/*.rs; do
  mod=$(basename "$f" .rs)
  awk -v mod="$mod" '
    match($0, /^\/\/ T[0-9]+\.[0-9]+ /) { id=substr($0, 4, RLENGTH-4) }
    match($0, /#\[ignore = "[^"]*"\]/) { br=substr($0, RSTART+12, RLENGTH-14) }
    /^[ \t]*#\[([a-z_]+::)?test/ { t=1 }
    t && match($0, /^[ \t]*(async )?fn [a-z0-9_]+\(/) { fn=$0; sub(/^[ \t]*(async )?fn /, "", fn); sub(/\(.*/, "", fn)
      print id "|" mod "::" fn "|" br; id=""; br=""; t=0 }' "$f"
done | sort)

fail=0
if [ -z "$want" ]; then echo "check-invariant-tests: $doc에서 표 행을 못 읽었다"; exit 1; fi
# 이름으로 묶어 비교한다
while IFS='|' read -r id name br; do
  line=$(printf '%s\n' "$have" | awk -F'|' -v n="$name" '$2==n' | head -1)
  if [ -z "$line" ]; then echo "  표에 있고 코드에 없음: $id $name"; fail=1; continue; fi
  cid=$(cut -d'|' -f1 <<<"$line"); cbr=$(cut -d'|' -f3 <<<"$line")
  [ "$cid" = "$id" ] || { echo "  ID 불일치: $name 표 $id, 코드 주석 '${cid:-없음}'"; fail=1; }
  if [ -n "$cbr" ] && [ "$cbr" != "$br" ]; then echo "  브랜치 불일치: $id $name 표 $br, ignore '$cbr'"; fail=1; fi
  mod=${name%%::*}
  grep -q "^mod $mod;" "$entry" || { echo "  $entry에 mod $mod; 없음"; fail=1; }
done <<<"$want"
while IFS='|' read -r id name br; do
  printf '%s\n' "$want" | awk -F'|' -v n="$name" '$2==n {f=1} END {exit !f}' \
    || { echo "  코드에 있고 표에 없음: ${id:-ID없음} $name"; fail=1; }
done <<<"$have"

n=$(printf '%s\n' "$want" | wc -l | tr -d ' ')
if [ "$fail" -eq 0 ]; then echo "check-invariant-tests: 통과 (${n}개)"; else echo "check-invariant-tests: 실패"; fi
exit "$fail"
