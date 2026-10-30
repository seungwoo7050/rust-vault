#!/usr/bin/env bash
# 훅·CI·사람이 같이 부르는 검증 명령.
#   fast: fmt, clippy, cargo test(DB 없는 테스트), 표·뼈대 일치, 가드 테스트
#   full: fast + compose의 Postgres로 DB 테스트(--features db: db, ledger, invariants) + 공개 경계 검사
# 마지막 줄: verify: fmt ✓ clippy ✓ test a/b db c/d ignored e invariants ✓ guards ✓ public ✓
#   test·db는 통과/실행 수, ignored는 cargo가 센 ignored 수(test와 db의 합)다. 하지 않은 단계는 –.
#   불변조건 증명 테스트는 db 기능이 있어야 빌드되므로 ignored는 full에서만 그 수가 잡힌다.
# 이 스크립트가 띄운 Postgres는 끝나면 내린다(VERIFY_KEEP_DB=1이면 남긴다).
set -uo pipefail
mode=${1:-fast}
case $mode in fast | full) ;; *) echo "쓰는 법: $0 fast|full" >&2; exit 2 ;; esac
cd "$(git rev-parse --show-toplevel)"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/verify.XXXXXX")
started_db=0
cleanup() {
  [ "$started_db" -eq 1 ] && [ "${VERIFY_KEEP_DB:-0}" != 1 ] && docker compose down -v >/dev/null 2>&1
  rm -rf "$tmp"
}
trap cleanup EXIT

failed=0
# step <이름> <명령...>: 결과를 ✓/✗로 찍고, 실패하면 출력 끝부분을 보인다. 출력은 $tmp/<이름>.log
step() {
  local name=$1; shift
  if "$@" >"$tmp/$name.log" 2>&1; then
    echo "✓ $name"; eval "st_$name='✓'"
  else
    echo "✗ $name"; tail -n 30 "$tmp/$name.log" | sed 's/^/    /'; eval "st_$name='✗'"; failed=1
  fi
}
# cargo test 출력의 "test result:" 줄을 더해 "통과/실행 ignored"를 낸다
count() { awk '/^test result:/ { gsub(/;/, ""); p += $4; f += $6; i += $8 } END { print p + 0, p + f, i + 0 }' "$1"; }

st_fmt=– st_clippy=– st_test=– st_db=– st_invariants=– st_guards=– st_public=–
step fmt cargo fmt --all --check
if [ "$mode" = full ]; then
  step clippy cargo clippy --workspace --all-targets --all-features -- -D warnings
else
  step clippy cargo clippy --workspace --all-targets -- -D warnings
fi
step test cargo test --workspace
read -r tp tn ti < <(count "$tmp/test.log")
step invariants scripts/harness/check-invariant-tests.sh
step guards scripts/harness/test-guards.sh

db="db –"
if [ "$mode" = full ]; then
  if [ -z "$(docker compose ps -q --status running postgres 2>/dev/null)" ]; then started_db=1; fi
  step compose docker compose up -d --wait postgres
  # .env.example의 로컬 테스트 값(밖에서 DATABASE_URL을 주면 그것을 쓴다)
  export DATABASE_URL=${DATABASE_URL:-$(sed -n 's/^DATABASE_URL=//p' .env.example)}
  if [ "$st_compose" = "✓" ]; then step db cargo test -p vault --features db --test db --test ledger --test invariants; fi
  touch "$tmp/db.log"; read -r dp dn di < <(count "$tmp/db.log")
  ti=$((ti + di))
  db="db $dp/$dn"; [ "$st_db" = "✓" ] || db="$db ✗"
  step public scripts/check-public.sh
fi

t="test $tp/$tn"; [ "$st_test" = "✓" ] || t="$t ✗"
echo "verify: fmt $st_fmt clippy $st_clippy $t $db ignored $ti invariants $st_invariants guards $st_guards public $st_public"
exit "$failed"
