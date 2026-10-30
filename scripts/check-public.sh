#!/usr/bin/env bash
# 공개 브랜치에 넣으면 안 되는 문자열을 찾는다. 하나라도 나오면 실패한다.
# 쓰는 법: scripts/check-public.sh [검사할 경로...]  (기본: 이 브랜치의 추적 파일 전체)
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
# macOS 기본 bash 3.2에는 mapfile이 없어서 while read로 받는다
files=()
if [ "$#" -gt 0 ]; then files=("$@"); else while IFS= read -r f; do files+=("$f"); done < <(git ls-files); fi

patterns=(
  'DocsArchive'                     # 비공개 학습 저장소 이름
  'JobsArchive'                     # 비공개 지원 저장소 이름
  '빗썸|[Bb]ithumb'                  # 지원 회사 이름
  '면접|이력서|자기소개서'             # 구직 맥락
  '[A-Za-z0-9._%+-]+@(gmail|naver|daum|kakao)\.'   # 개인 이메일
  'sslip\.io|[0-9]{1,3}(\.[0-9]{1,3}){3}'         # 운영 주소·IP
  'AKIA[0-9A-Z]{16}'                # AWS 액세스 키
  'sk-[A-Za-z0-9_-]{20,}'           # LLM API 키 모양
  'sk_(live|test)_[A-Za-z0-9]{10,}' # Stripe 키
  'gh[pousr]_[A-Za-z0-9]{20,}'      # GitHub 토큰
  '-----BEGIN [A-Z ]*PRIVATE KEY'   # 개인 키
)

fail=0
for p in "${patterns[@]}"; do
  hits=$(grep -nE -- "$p" "${files[@]}" 2>/dev/null | grep -v '^scripts/check-public.sh:' || true)
  if [ -n "$hits" ]; then
    echo "공개 금지 패턴: $p"
    echo "$hits" | sed 's/^/  /'
    fail=1
  fi
done

[ "$fail" -eq 0 ] && echo "check-public: 통과 (${#files[@]}개 파일)"
exit "$fail"
