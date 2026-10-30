# 시스템 불변조건

이 문서가 불변조건과 추적표의 원본이다. ROADMAP·README·장애 시나리오 문서는 여기를 링크한다.

## 불변조건

출처: 사람 설계.

| ID | 이름 | 규칙 |
| --- | --- | --- |
| I1 | 자산 안전성 | 사용자 잔액은 승인된 원장 거래 없이 변경될 수 없다 |
| I2 | 멱등성 | 같은 입금 이벤트나 출금 요청을 여러 번 처리해도 자산이 중복 반영되지 않는다 |
| I3 | 원장 정합성 | 모든 회계 거래는 차변과 대변이 일치한다 |
| I4 | 블록체인 정합성 | 입금 확정 여부는 현재 canonical chain과 확정 정책에 따라 결정한다 |
| I5 | 복구 가능성 | 어떤 처리 단계에서 프로세스가 종료되어도 재시작 후 일관된 상태로 복구한다 |
| I6 | 외부 불확실성 | RPC 타임아웃이나 응답 유실을 곧바로 트랜잭션 실패로 판단하지 않는다 |

## 해석과 지키는 장치

확정: 오케스트레이터. 규칙을 코드에서 판정할 수 있는 문장으로 바꾼 것이다.

| ID | 판정할 수 있는 문장 | 지키는 장치 |
| --- | --- | --- |
| I1 | 사용자 계정의 잔액은 언제나 그 계정에 기록된 분개 줄 금액의 합과 같다. 분개 기록이 거부되면 어떤 잔액도 바뀌지 않는다. 사용자 계정은 음수가 되지 않는다. 잠정 입금은 사용자 계정이 아닌 잠정 입금 계정에 있다 | 잔액 열이 없다(분개 합으로 계산). 원장 API에 잔액을 직접 바꾸는 함수가 없다. 분개는 한 DB 트랜잭션에서 검증 후 기록한다([ADR-001](adr/001-ledger-model.md)) |
| I2 | 같은 멱등 키로 분개를 여러 번 기록하면 첫 번째만 반영되고 나머지는 아무것도 바꾸지 않는다. 같은 키에 다른 내용이 오면 오류다. 출금 요청 하나는 체인에 최대 한 번 지급된다 | 분개 멱등 키 유일 제약. 키는 외부 사실(tx 해시, 블록 해시, 출금 요청 ID)에서만 만든다. 서명 tx 저장 후 같은 바이트만 재전송([ADR-003](adr/003-nonce-and-rpc-uncertainty.md)) |
| I3 | 모든 분개는 줄 금액(차변 +, 대변 −)의 합이 0이고 줄이 2개 이상이다. 따라서 전체 계정 잔액 합(시산)도 언제나 0이다 | Rust 쪽 검증과 DB의 커밋 시점 제약 트리거, 두 겹([ADR-001](adr/001-ledger-model.md)) |
| I4 | 입금은 그 tx를 담은 블록이 지금 canonical chain에 있고 확정 수가 N 이상일 때만 확정된다. canonical에서 빠진 블록의 잠정 입금은 역분개된다. 확정된 입금은 원장에서 되돌리지 않는다(정책 가정이 깨진 것으로 보고 사고 기록·처리 중지) | 블록 헤더(번호·해시·부모 해시) 저장과 부모 해시 비교로 reorg 감지. 확정은 원장이 아니라 canonical 헤더로 계산([ADR-002](adr/002-reorg-handling.md)) |
| I5 | 장애 주입 지점 어디에서 종료하고 재시작해도, 최종 원장·커서·출금 상태가 장애 없는 실행과 같다 | 블록 하나(또는 출금 상태 전이 하나)의 모든 쓰기를 한 DB 트랜잭션으로 묶는다. 외부 부작용(tx 전송)은 그 앞 상태가 커밋된 뒤에만 한다 |
| I6 | 결과를 모르는 RPC 오류(타임아웃, 연결 끊김, 응답 유실)는 `Indeterminate`로 분류되고, 이것만으로는 출금이 `Failed`가 되거나 홀드가 풀리거나 블록이 빈 것으로 처리되지 않는다 | RPC 결과 타입을 `Ok`, `Definite(오류)`, `Indeterminate` 셋으로 나눈다. `Unknown` 상태는 영수증·계정 nonce 조회로만 낸다. `Unknown`에서 `Sent`·`NeedsReview`로 가는 전이는 결론이 아니다([ADR-003](adr/003-nonce-and-rpc-uncertainty.md)) |

## 추적표

불변조건 → 장애 시나리오([FAILURE_SCENARIOS](FAILURE_SCENARIOS.md)) → 증명 테스트 → ADR. 테스트는 아직 없고 예정 브랜치에서 생긴다. 01-harness는 아래 이름으로 빈 테스트를 만들고 `#[ignore = "NN-브랜치"]`를 단다.

| 불변조건 | 위협하는 시나리오 | 이 불변조건 이름으로 둔 테스트 | ADR |
| --- | --- | --- | --- |
| I1 자산 안전성 | F1, F2, F4, F5 | T1.1, T1.2, T1.3 | 001, 002, 003 |
| I2 멱등성 | F1, F3, F4, F5, F6, F7, F8 | T2.1, T2.2, T2.3, T2.4 | 001, 002, 003 |
| I3 원장 정합성 | F2, F7 | T3.1, T3.2, T3.3 | 001, 002 |
| I4 블록체인 정합성 | F2, F8 | T4.1, T4.2, T4.3, T4.4, T4.5 | 002 |
| I5 복구 가능성 | F3, F7 | T5.1, T5.2, T5.3 | 001, 003 |
| I6 외부 불확실성 | F4, F5, F6 | T6.1, T6.2, T6.3, T6.4, T6.5, T6.6 | 002, 003 |

시나리오에서 거꾸로 본 것(어느 시나리오든 테스트 1개 이상이 직접 주입한다. 불변조건 행의 시나리오 중 그 불변조건 이름의 테스트가 없는 것은 이 표의 테스트가 함께 판정한다. 예: F4가 I1을 위협하는 경로는 T6.1이 "홀드 해제 안 됨"으로 판정):

| 시나리오 | 증명 테스트 |
| --- | --- |
| F1 블록 이벤트 중복 | T2.1, T2.3, T4.3 |
| F2 reorg | T1.2, T3.3, T4.1, T4.2, T4.3, T4.4 |
| F3 DB 커밋 직후 종료 | T2.3, T5.1, T5.3, T6.6 |
| F4 RPC 응답 유실 | T6.1, T6.2, T6.5, T6.6 |
| F5 nonce 충돌 | T6.3, T6.6 |
| F6 출금 재전송 | T2.4, T5.3, T6.4, T6.6 |
| F7 DB 일시 장애 | T2.1, T3.2, T5.2 |
| F8 이벤트 순서 변경 | T4.5 |

## 증명 테스트

확정: 오케스트레이터. 파일은 `tests/invariants/iN_*.rs`, 테스트 이름은 `파일::함수`다.

"실패해야 할 변형"은 평가 단계에서 구현을 일부러 그렇게 바꿔(커밋하지 않는다) 테스트가 빨간색이 되는지 보는 기준이다. 그 변형에서도 통과하는 테스트는 불변조건을 증명하지 못한 것으로 판정한다. "변형을 가릴 수 있는 장치" 열은 다른 방어선이 그 변형을 대신 막아 테스트가 빨개지지 않을 가능성과, 입력·변형을 어떻게 골라 피했는지를 적는다. 새 테스트를 더할 때 이 열을 비우지 않는다.

| ID | 시나리오 | 테스트 | 넣는 입력과 단언 | 실패해야 할 변형 | 변형을 가릴 수 있는 장치 | 브랜치 |
| --- | --- | --- | --- | --- | --- | --- |
| T1.1 | — | `i1_asset_safety::rejected_entry_changes_no_balance` | 한 DB 트랜잭션 안에서 정상 분개와 거부될 분개(없는 계정, 차대 불일치, 사용자 잔액 부족)를 섞은 임의 열을 기록한 뒤 그 트랜잭션을 커밋(proptest). 단언: 커밋이 성공하고, 계정 잔액이 정상 분개만으로 계산한 모델과 같다 | 분개 기록에서 savepoint를 없앰(없는 계정의 FK 위반이 트랜잭션 전체를 중단시키고, 그 트랜잭션의 커밋은 롤백으로 끝나 정상 분개까지 사라진다) | 쓰기 전 Rust 검사(차대·줄 수 거부는 쓰기 전에 걸려 savepoint와 무관하다. 그래서 FK로만 걸리는 없는 계정 입력에 기댄다, ADR-001 결정 8) | 02-ledger |
| T1.2 | F2 | `i1_asset_safety::pending_deposit_not_spendable` | 확정 수 N−1인 입금 직후 같은 금액의 출금 요청(출금 API `request`). 단언: 요청 `Rejected`, 홀드 분개 0개, `user:u` = 0, 확정 뒤 같은 금액 요청은 `Held` | 잠정 입금을 사용자 계정에 바로 대변 기록 | 없음 | 03-deposit-reorg |
| T1.3 | — | `i1_asset_safety::hold_rejects_overspend` | 잔액보다 큰 출금, 잔액을 나눠 쓰는 동시 출금 두 건. 단언: 큰 출금 `Rejected`, 동시 두 건 중 잔액 안의 것만 `Held`, `user:u` ≥ 0 | 홀드 분개의 사용자 잔액 음수 검사 제거 | 없음 | 05-withdrawal |
| T2.1 | F1, F7 | `i2_idempotency::same_key_reposted_changes_nothing` | 임의 분개 열에 같은 키 재기록을 무작위 위치로 끼움(proptest). 단언: 잔액·분개 수가 중복 없는 열과 같고, 재기록은 기존 분개 ID를 돌려준다 | 멱등 키 유일 제약과 기존 키 조회 제거 | 없음 | 02-ledger |
| T2.2 | — | `i2_idempotency::same_key_different_body_rejected` | 같은 키, 다른 금액·계정의 분개. 단언: `IdempotencyConflict` 오류, 기존 분개와 잔액 불변 | 기존 키가 있으면 내용 비교 없이 성공 반환 | 없음 | 02-ledger |
| T2.3 | F1, F3 | `i2_idempotency::duplicate_block_events_credit_once` | (가) 같은 블록을 2~3번 전달 (나) 블록 커밋 직후 "결과 모름" 오류를 주입해 같은 프로세스가 그 블록을 다시 처리 (다) 재시작 후 커서 직전 블록 재전달. 단언: 입금마다 `pending_deposit:u`·`user:u`에 한 번씩만 반영 | 입금 키를 tx 해시 대신 수신 순번으로 만듦((나)의 재처리가 원장까지 닿아 두 번 기록된다) | 끌어오기 커서(재전달 블록은 원장까지 안 온다. 그래서 (나)를 넣는다), `deposits` 행의 기본 키(행이 이미 있어도 잠정 분개는 다시 보낸다. 그래서 (나)의 재처리가 원장까지 닿는다) | 03-deposit-reorg |
| T2.4 | F6 | `i2_idempotency::retried_withdrawal_request_holds_once` | 같은 출금 요청 ID를 여러 번 제출(재시작 사이에도). 단언: 출금 행 1개, 홀드 분개 1개, 체인 지급 1회 | `withdrawals.request_id` 유일 제약과 기존 요청 조회를 제거(홀드 키는 그대로라 홀드 분개는 하나여도 출금 행이 둘이 된다) | 홀드 멱등 키(그래서 변형은 요청 ID 중복 확인을 없애고, 다시 제출한 직후 출금 행 수를 단언한다) | 05-withdrawal |
| T3.1 | — | `i3_ledger_balance::every_posted_entry_sums_to_zero` | 정상 분개만으로 된 임의 열(proptest). 단언: 모든 기록이 성공하고, 분개별 줄 합 0, 시산 0, 계정 잔액이 모델 계산과 같다 | 분개 줄 기록 루프가 마지막 줄을 빠뜨림(커밋 트리거가 거부해 "모두 성공" 단언이 깨진다) | Rust 쪽 검증(그래서 변형은 검증 뒤 쓰기 루프에 둔다) | 02-ledger |
| T3.2 | F7 | `i3_ledger_balance::db_rejects_unbalanced_entry` | Rust 검증을 거치지 않고 SQL로 차대 불일치 분개 삽입 후 커밋. 단언: 커밋이 제약 위반으로 거부되고, 그 분개와 줄이 남지 않으며 시산 0 | 커밋 시점 제약 트리거 제거 | Rust 쪽 검증(그래서 SQL로 직접 넣는다) | 02-ledger |
| T3.3 | F2 | `i3_ledger_balance::trial_balance_zero_after_reorgs` | 입금·reorg·재포함·확정이 섞인 임의 체인 이력(proptest. reorg 깊이는 N 미만이고 새 분기가 예전보다 짧지 않아 확정된 입금은 빠지지 않는다). 단언: 스캔이 오류 없이 head까지 가고, 시산 0, 고아 블록의 잠정 입금 잔액 0, `hot_wallet:{입금 주소}` 잔액 = 가짜 체인 canonical 입금 합 | 역분개가 잠정 입금 계정 줄만 쓰고 핫월렛 줄을 빠뜨림(거부되어 스캔이 멈춘다) | Rust 쪽 검증(줄 1개는 `TooFewLines`)과 커밋 트리거. 둘 다 시산을 지키는 대신 기록을 거부하므로 시산 0만으로는 못 잡는다(그래서 스캔 무오류를 단언한다) | 03-deposit-reorg |
| T4.1 | F2 | `i4_chain_consistency::reorg_before_confirmation_reverses_pending` | 입금 블록을 포함한 깊이 1~N−1의 분기 교체. 단언: 스캔이 오류 없이 새 분기 head까지 가고, 그 입금의 역분개 1개, `pending_deposit:u` = 0(새 분기에 그 tx가 없을 때), `user:u` 불변, 커서 = 새 분기 head | reorg 감지에서 부모 해시 비교를 빼고 블록 번호만 봄 | 없음(조건부 커서 전진이 다른 분기의 블록을 거부해 변형은 스캔 오류로 드러난다. 그래서 스캔 무오류를 단언한다) | 03-deposit-reorg |
| T4.2 | F2 | `i4_chain_consistency::reorg_beyond_confirmation_keeps_ledger_and_halts` | 확정된 입금 블록을 빼는 깊이 N 이상의 reorg. 단언: 분개 수·모든 잔액 불변, `finality_violations` 1행, 스캐너 정지 | 확정된 입금도 역분개 | 없음 | 03-deposit-reorg |
| T4.3 | F1, F2 | `i4_chain_consistency::reincluded_tx_confirms_once` | reorg로 빠진 tx가 새 분기의 다른 블록에 다시 들어가고 확정 수 N에 이름. 단언: 스캔이 오류 없이 head까지 가고, `user:u` += 금액 한 번, `pending_deposit:u` = 0, `hot_wallet:{입금 주소}` = canonical 입금 합 | 잠정 입금 키에서 블록 해시를 뺌 | 블록 해시 없는 확정 키(사용자 잔액은 한 번만 는다. 그래서 잠정 입금 잔액을 단언한다) | 03-deposit-reorg |
| T4.4 | F2 | `i4_chain_consistency::confirmation_counts_canonical_depth` | 고아 블록이 섞인 상태(입금 블록과 같은 번호의 고아 헤더)에서 확정 수 N−1, N 경계. 단언: N−1에서 확정 분개 0개, N에서 1개 | 확정 수를 받은 블록 개수(고아 포함)로 셈 | 없음 | 03-deposit-reorg |
| T4.5 | F8 | `i4_chain_consistency::event_order_does_not_change_final_ledger` | 같은 블록 집합의 새 head 알림을 임의 순서·중복으로 전달(proptest), reorg 없음. 단언: 전달마다 스캔이 오류 없이 보이는 head까지 가고, 저장 헤더가 부모 해시로 이어진 한 줄이고 커서는 1씩만 전진, 역분개 0개, 확정 분개는 그 블록 위로 N−1개가 처리된 뒤에만 생김, 최종 원장이 순서대로 전달한 실행과 같다 | 알림으로 받은 블록을 "부모 해시 = 커서 해시" 검사 없이 바로 처리(끌어오기 대신 밀어넣기) | 멱등 키(최종 원장은 순서와 무관해진다. 그래서 중간 상태를 단언한다), 부모 해시 검사(가짜 reorg로 되감아 고친다. 그래서 변형이 그 검사를 없애고, 역분개 0개를 단언한다), 조건부 커서 전진(가리지 않는다: 순서가 바뀐 블록을 거부해 변형이 스캔 오류로 드러난다. 그래서 스캔 무오류를 단언한다) | 03-deposit-reorg |
| T5.1 | F3 | `i5_recovery::crash_at_each_injection_point_matches_clean_run` | 입금·reorg·재포함·확정이 섞인 고정 시나리오 하나와 proptest(결정적 RNG)로 만든 시나리오 3개. 장애 없이 한 번 돌려(기준) 주입 지점(`BlockFetched` 이벤트 수신 직후, `BlockEntriesWritten` 분개 기록 중간, `BeforeCommit(Block·Rewind)` 각 DB 커밋 직전, `AfterCommit(Block·Rewind)` 각 DB 커밋 직후)별 통과 횟수를 센 뒤, (지점, k) 모든 조합마다 새 DB에서 k번째 통과 때 종료(`Crash`) → 재시작(`Scanner`를 새로 만든다) → 그 스캔부터 이어 감. 단언: 조합마다 종료·재시작이 한 번 나고, 스캔마다(마지막이 최종) 원장(분개 키·잔액)·커서·`deposits`·`block_headers`·정지 여부가 장애 없는 실행과 같다. 돈 조합 수가 0이 아니고 기준 실행의 지점별 통과 횟수 합과 같다(출력에 조합 수를 찍는다) | 커서 전진을 원장 반영보다 먼저 별도 트랜잭션으로 커밋("커서 커밋 직후" 지점에서 죽으면 그 블록의 입금이 빠진다) | 멱등 키(커서를 원장 뒤에 나누면 재처리가 흡수된다. 그래서 커서를 먼저 커밋하는 변형). 나뉜 커밋도 `store::commit`을 거치므로 변형의 커서 커밋 직후에도 주입 지점이 자동으로 생긴다 | 04-crash-recovery |
| T5.2 | F7 | `i5_recovery::transient_db_error_leaves_no_partial_state` | T5.1과 같은 시나리오에서 블록 처리의 분개 기록 중간(`BlockEntriesWritten`), 커밋 직전(`BeforeCommit(Block)`)·직후(`AfterCommit(Block)`)의 k번째 통과마다(기준 실행의 통과 횟수만큼) 일시 DB 오류(`Transient`)를 한 번 주입, 재시작 없이 같은 프로세스의 `run_until_idle`이 재시도. 단언: 재시작 없이 스캔이 끝나고, 그 지점 통과가 기준보다 한 번 많고(그 블록을 다시 처리했다), 스캔마다 상태(T5.1과 같은 항목)가 장애 없는 실행과 같다(그 블록의 입금이 정확히 한 번 반영된다) | DB 오류를 "처리할 수 없는 블록"으로 분류해 재시도하지 않고, DB 커서를 그 블록으로 직접 전진 | DB 트랜잭션(부분 상태는 DB가 막는다), 조건부 커서 전진(메모리 커서만 앞서는 변형은 다음 커밋이 거부되고, 오류 뒤 DB에서 커서를 다시 읽으면 고쳐진다. 그래서 변형은 DB 커서를 직접 옮긴다), 커밋 뒤 주입(`AfterCommit`은 이미 커밋돼 건너뛰어도 잃는 것이 없다. 그래서 커밋 전 두 지점을 함께 넣는다) | 04-crash-recovery |
| T5.3 | F3, F6 | `i5_recovery::restart_after_sign_resends_same_tx` | 서명 tx 커밋 직후, 전송 직후 종료 후 재시작. 단언: 출금마다 체인 지급 1회, 보낸 raw tx가 저장본과 같다 | 재시작 때 `Signed` 출금에 nonce를 새로 할당해 다시 서명(같은 nonce로 다시 서명하면 결정적 서명이라 같은 바이트가 나와 변형이 안 된다) | 결정적 서명(같은 nonce면 같은 바이트. 그래서 새 nonce로 다시 서명) | 05-withdrawal |
| T6.1 | F4 | `i6_external_uncertainty::send_timeout_moves_to_unknown` | 전송 요청은 반영하고 응답만 버림. 단언: 상태 `Unknown`, 홀드 유지, 새 서명 tx 0개 | 타임아웃을 `Failed`로 매핑하고 홀드 해제 | 없음 | 05-withdrawal |
| T6.2 | F4 | `i6_external_uncertainty::lost_response_resolved_by_receipt` | 응답 유실 뒤 조회가 몇 번 타임아웃되고 tx가 채굴됨. (가) 기한 전에 채굴 (나) 가짜 시계를 기한 너머로 돌려 `NeedsReview`가 된 뒤 채굴. 단언: 둘 다 `Confirmed`, 지급 분개 1개, 홀드 해제 분개 0개 | 기한이 지나면 `Unknown`·`NeedsReview`를 `Failed`로 바꾸고 홀드 해제((나)에서 깨진다) | 기한(그래서 가짜 시계로 기한 뒤를 만든다) | 05-withdrawal |
| T6.3 | F5 | `i6_external_uncertainty::nonce_conflict_never_double_pays` | (가) 전송 직후 종료해 `Signed`로 남은 사이 내 tx가 채굴되고, 재시작 뒤 재전송이 "nonce too low"를 받음 (나) 외부 tx가 다음 nonce를 먼저 써서 채굴되고, 그 뒤 우리 출금이 같은 nonce로 서명됨. 단언: (가) 지급 1회, `Confirmed` (나) `NeedsReview`, 홀드 유지, 우리 지급 0회 | "nonce too low"에서 새 nonce로 다시 서명((가)에서 이중 지급, (나)에서 `NeedsReview`가 아님) | 외부 선점만 넣으면 우리 지급이 1회로 남는다(그래서 (가)를 넣는다). 외부 tx가 멤풀에만 있으면 거절이 "replacement underpriced"라 변형이 안 걸린다(그래서 (나)는 외부 tx를 채굴한 뒤 서명하게 한다) | 05-withdrawal |
| T6.4 | F6 | `i6_external_uncertainty::resend_uses_identical_raw_tx` | 첫 전송은 반영하고 응답만 버린 뒤 `Unknown`에서 여러 번 재전송. 단언: 노드가 받은 raw tx가 모두 저장본과 같고 지급 1회 | 재전송 때마다 nonce를 새로 할당해 다시 서명 | 결정적 서명(T5.3과 같다) | 05-withdrawal |
| T6.5 | F4 | `i6_external_uncertainty::read_timeout_never_skips_block` | 블록 조회 타임아웃·빈 응답을 입금 블록에 주입. 단언: 커서가 그 블록 앞에 머물고, 조회가 회복되면 입금이 한 번 반영되고, 역분개 0개 | 조회 실패 블록을 빈 블록으로 보고 커서 전진 | 없음 | 03-deposit-reorg |
| T6.6 | F3, F4, F5, F6 | `i6_external_uncertainty::random_send_faults_pay_at_most_once` | 응답 유실·타임아웃·nonce 충돌·재시작을 무작위로 섞은 출금 열(proptest). 출금 요청마다 체인 지급 수 ≤ 1, 원장 출금액 = 체인 지급액 | T6.1, T6.3, T6.4의 변형 중 어느 하나(`mutants/T6.6`은 T6.4의 변형) | T6.1·T6.3·T6.4 행 참조. 끝의 마무리 처리가 채굴 전에 조회 루틴을 돌려 재전송이 언제나 일어난다 | 05-withdrawal |

## 바꾸는 법

- 불변조건 표(I1–I6)는 사람만 고친다. 구현 중 두 불변조건을 동시에 지킬 수 없다는 발견이 나오면 멈추고 사람에게 묻는다.
- 해석, 추적표, 테스트 표는 구현 브랜치에서 고칠 수 있다. 테스트를 나누거나 합치면 같은 커밋에서 이 표를 고친다.
