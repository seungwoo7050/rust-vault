# 04-crash-recovery

```
상태: 확정(오케스트레이터)
브랜치: 04-crash-recovery (앞 브랜치: 03-deposit-reorg @ dfc5630)
문제: 블록 처리 중 어느 지점에서 프로세스가 죽거나 DB가 잠깐 끊기면, 재시작 뒤 블록을 건너뛰거나 두 번 반영하거나
      부분 상태가 남을 수 있다. 03은 "커밋 결과 모름" 한 지점만 확인했다.
지킬 불변조건: I5, I2 (증명 테스트 T5.1, T5.2. T5.3은 05)
```

설계 원본은 docs/ARCHITECTURE.md의 장애 주입 결정, docs/FAILURE_SCENARIOS.md의 "시나리오를 테스트로 만들 때", ADR-002 결정 3이다.

## 결정

- **`Faults` trait를 넓힌다**(`vault::store`): 지점마다 부르는 메서드 하나 `fn hit(&self, point: FaultPoint) -> Result<(), Injected>`로 바꾸고, `Injected::{Crash, Transient, CommitOutcomeUnknown}`. 03의 `after_commit`은 이것으로 옮긴다(동작 같음). 운영 기본값 `NoFaults`는 항상 `Ok`
  / 대안: 지점마다 메서드 / 이유: 테스트가 "모든 지점 × k번째 통과"를 한 반복문으로 돌 수 있다 / 바꿀 신호: 지점별로 넘길 데이터가 달라질 때
- **지점**(`FaultPoint`, 이름 고정): `BlockFetched`(블록을 받은 직후, 쓰기 전 = 이벤트 수신 직후), `BlockEntriesWritten`(분개는 썼고 커서 전진 전 = 분개 기록 중간), `BeforeCommit(Site)`, `AfterCommit(Site)`. `Site::{Block, Rewind}`. 스캐너의 모든 커밋은 `store`의 커밋 도우미 하나를 거치고, 도우미가 `BeforeCommit`·`AfterCommit`을 부른다(FAILURE_SCENARIOS: 구현이 트랜잭션을 나누면 나뉜 커밋 사이에도 자동으로 걸린다)
- **주입 의미**: `Crash` = 그 자리에서 `ScanError::Crashed`로 빠져나온다(열린 트랜잭션은 버려진다). 테스트는 `Scanner`를 버리고 같은 DB·같은 체인으로 새로 만든다 = 재시작. `Transient` = 일시 DB 오류. 열린 트랜잭션을 버리고 `ScanError::Transient`. `AfterCommit`에서의 `Transient`·`CommitOutcomeUnknown`은 커밋은 된 상태다
- **일시 오류 분류와 재시도**: 실제 sqlx 오류 중 연결 오류, `40001`(직렬화 실패), `40P01`(교착), 풀 타임아웃은 `Transient`. `run_until_idle`은 `Transient`면 같은 step을 다시 한다(최대 횟수는 설정값, 넘으면 오류를 돌려준다). 재시도는 DB에서 커서를 다시 읽고 시작한다. 그 밖의 DB 오류는 재시도하지 않고 돌려준다(블록을 건너뛰지 않는다)
  / 대안: 모든 DB 오류 재시도 / 이유: 제약 위반 같은 영구 오류를 무한 반복한다 / 바꿀 신호: 운영에서 다른 일시 오류 코드가 보이면 목록에 더한다
- **T5.1 실행 방식**: 장애 없는 기준 실행(입금·reorg·재포함·확정이 섞인 고정 시나리오 + proptest로 만든 시나리오 몇 개)을 한 번 돌려 지점별 통과 횟수를 센다. 그다음 (지점, k) 모든 조합마다 새 DB에서 "k번째 통과 때 Crash → 재시작 → idle까지"를 돌려 최종 원장(분개 키·잔액)·커서·`deposits`·`block_headers`가 기준과 같은지 본다. 실행한 조합 수를 테스트 출력에 찍는다(보고 숫자는 이것만 쓴다)
- **T5.2**: 같은 시나리오에서 `BlockEntriesWritten`과 `BeforeCommit(Block)`·`AfterCommit(Block)`에 `Transient`를 넣고(재시작 없이) `run_until_idle`만으로 기준과 같아지는지 본다
- **복구 루틴 문서화**: 재시작 때 메모리에서 이어받는 것이 없다는 것을 `Scanner::new` 문서 주석과 ARCHITECTURE "재시작(04)"에 맞춘다. 출금 쪽은 05
- **변형 파일**: `mutants/T5.1`(표: 커서 전진을 원장 반영보다 먼저 별도 트랜잭션으로 커밋), `mutants/T5.2`(표: DB 오류를 처리할 수 없는 블록으로 분류해 재시도하지 않고 DB 커서를 그 블록으로 직접 전진). 표 밖: `mutants/RETRY-ALL`(모든 DB 오류를 재시도로 분류 → 영구 오류에서 끝나지 않음을 잡는 테스트가 있어야 한다)

## 완료 기준

1. `scripts/harness/verify.sh full` 통과. 증명 테스트 16개 통과(T5.1, T5.2 추가), 8개 ignored
2. `scripts/harness/mutants.sh` 전체 잡힘(03까지 18 + 이 브랜치 3)
3. T5.1이 모든 (지점, k) 조합을 돌고, 그 수가 0이 아니며 기준 실행의 지점별 통과 횟수 합과 같다(테스트 안에서 단언)
4. 표 밖 테스트: (가) 영구 DB 오류(예: 제약 위반을 일으키는 주입)는 재시도 없이 오류로 끝나고 커서가 움직이지 않는다 (나) 재시도 횟수를 넘긴 `Transient`는 오류로 끝나고 상태가 그 블록 앞 그대로다
5. docs(ARCHITECTURE의 재시작, FAILURE_SCENARIOS의 주입 지점 이름, 표의 T5.1·T5.2 문구)가 구현과 같다. `scripts/check-public.sh` 통과

## 범위 밖

출금의 재시작(05의 T5.3), 실제 프로세스 kill 테스트(ARCHITECTURE의 바꿀 신호), Clock 기반 재시도 간격(05에서 Clock이 생기면 옮긴다)

## 외부 의존

docker(Postgres 17)
