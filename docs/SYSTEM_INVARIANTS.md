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
| I6 | 결과를 모르는 RPC 오류(타임아웃, 연결 끊김, 응답 유실)는 `Indeterminate`로 분류되고, 이것만으로는 출금이 `Failed`가 되거나 홀드가 풀리거나 블록이 빈 것으로 처리되지 않는다 | RPC 결과 타입을 `Ok`, `Definite(오류)`, `Indeterminate` 셋으로 나눈다. `Unknown` 상태는 영수증·계정 nonce 조회로만 벗어난다([ADR-003](adr/003-nonce-and-rpc-uncertainty.md)) |

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

"실패해야 할 변형"은 평가 단계에서 구현을 일부러 그렇게 바꿔(커밋하지 않는다) 테스트가 빨간색이 되는지 보는 기준이다. 그 변형에서도 통과하는 테스트는 불변조건을 증명하지 못한 것으로 판정한다.

| ID | 시나리오 | 테스트 | 넣는 입력 | 실패해야 할 변형 | 브랜치 |
| --- | --- | --- | --- | --- | --- |
| T1.1 | — | `i1_asset_safety::rejected_entry_changes_no_balance` | 차대 불일치, 없는 계정, 사용자 잔액 부족 분개를 정상 분개 사이에 섞은 임의 열(proptest) | 검증 전에 줄을 먼저 쓰고, 거부할 때 트랜잭션을 롤백하지 않음 | 02-ledger |
| T1.2 | F2 | `i1_asset_safety::pending_deposit_not_spendable` | 확정 수 N−1인 입금 직후 같은 금액의 출금 홀드 요청 | 잠정 입금을 사용자 계정에 바로 대변 기록 | 03-deposit-reorg |
| T1.3 | — | `i1_asset_safety::hold_rejects_overspend` | 잔액보다 큰 출금, 잔액을 나눠 쓰는 동시 출금 두 건 | 홀드 분개의 사용자 잔액 음수 검사 제거 | 05-withdrawal |
| T2.1 | F1, F7 | `i2_idempotency::same_key_reposted_changes_nothing` | 임의 분개 열에 같은 키 재기록을 무작위 위치로 끼움(proptest) | 멱등 키 유일 제약과 기존 키 조회 제거 | 02-ledger |
| T2.2 | — | `i2_idempotency::same_key_different_body_rejected` | 같은 키, 다른 금액·계정의 분개 | 기존 키가 있으면 내용 비교 없이 성공 반환 | 02-ledger |
| T2.3 | F1, F3 | `i2_idempotency::duplicate_block_events_credit_once` | 같은 블록을 2~3번 전달, 재시작 후 커서 직전 블록 재전달 | 입금 키를 tx 해시 대신 수신 순번으로 만듦 | 03-deposit-reorg |
| T2.4 | F6 | `i2_idempotency::retried_withdrawal_request_holds_once` | 같은 출금 요청 ID를 여러 번 제출(재시작 사이에도). 단언: 출금 행 1개, 홀드 분개 1개, 체인 지급 1회 | `withdrawals.request_id` 유일 제약과 기존 요청 조회를 제거(홀드 키는 그대로 두어도 두 번째 출금 행이 따로 서명·지급된다) | 05-withdrawal |
| T3.1 | — | `i3_ledger_balance::every_posted_entry_sums_to_zero` | 정상 분개만으로 된 임의 열(proptest). 단언: 모든 기록이 성공하고, 분개별 줄 합 0, 시산 0, 계정 잔액이 모델 계산과 같다 | 분개 줄 기록 루프가 마지막 줄을 빠뜨림(커밋 트리거가 거부해 "모두 성공" 단언이 깨진다) | 02-ledger |
| T3.2 | F7 | `i3_ledger_balance::db_rejects_unbalanced_entry` | Rust 검증을 거치지 않고 SQL로 차대 불일치 분개 삽입 | 커밋 시점 제약 트리거 제거 | 02-ledger |
| T3.3 | F2 | `i3_ledger_balance::trial_balance_zero_after_reorgs` | 입금·reorg·재포함·확정이 섞인 임의 체인 이력. 단언: 스캔이 오류 없이 head까지 가고, 시산 0, 고아 블록의 잠정 입금 잔액 0, `hot_wallet:{입금 주소}` 잔액 = 가짜 체인 canonical 입금 합 | 역분개가 잠정 입금 계정 줄만 쓰고 핫월렛 줄을 빠뜨림(거부되어 스캔이 멈춘다) | 03-deposit-reorg |
| T4.1 | F2 | `i4_chain_consistency::reorg_before_confirmation_reverses_pending` | 입금 블록을 포함한 깊이 1~N−1의 분기 교체 | reorg 감지에서 부모 해시 비교를 빼고 블록 번호만 봄 | 03-deposit-reorg |
| T4.2 | F2 | `i4_chain_consistency::reorg_beyond_confirmation_keeps_ledger_and_halts` | 확정된 입금 블록을 빼는 깊이 N 이상의 reorg | 확정된 입금도 역분개 | 03-deposit-reorg |
| T4.3 | F1, F2 | `i4_chain_consistency::reincluded_tx_confirms_once` | reorg로 빠진 tx가 새 분기의 다른 블록에 다시 들어감 | 잠정 입금 키에서 블록 해시를 뺌 | 03-deposit-reorg |
| T4.4 | F2 | `i4_chain_consistency::confirmation_counts_canonical_depth` | 고아 블록이 섞인 상태에서 확정 수 N−1, N 경계 | 확정 수를 받은 블록 개수(고아 포함)로 셈 | 03-deposit-reorg |
| T4.5 | F8 | `i4_chain_consistency::event_order_does_not_change_final_ledger` | 같은 블록 집합을 임의 순서로 전달(proptest) | 커서 순서 대신 도착 순서로 처리 | 03-deposit-reorg |
| T5.1 | F3 | `i5_recovery::crash_at_each_injection_point_matches_clean_run` | 주입 지점(이벤트 수신 직후, 각 DB 커밋 직전, 각 DB 커밋 직후)마다 종료 후 재시작. 단언: 최종 원장·커서·`deposits`가 장애 없는 실행과 같다 | 커서 전진을 원장 반영보다 먼저 별도 트랜잭션으로 커밋("커서 커밋 직후" 지점에서 죽으면 그 블록의 입금이 빠진다) | 04-crash-recovery |
| T5.2 | F7 | `i5_recovery::transient_db_error_leaves_no_partial_state` | 블록 처리의 분개 기록 중간과 커밋에서 일시 DB 오류 주입, 재시작 없이 같은 프로세스가 재시도. 단언: 그 블록의 입금이 정확히 한 번 반영되고 최종 상태가 장애 없는 실행과 같다 | 오류가 나도 메모리 커서를 먼저 올려 재시도가 다음 블록부터 시작됨 | 04-crash-recovery |
| T5.3 | F3, F6 | `i5_recovery::restart_after_sign_resends_same_tx` | 서명 tx 커밋 직후, 전송 직후 종료 후 재시작. 단언: 출금마다 체인 지급 1회, 보낸 raw tx가 저장본과 같다 | 재시작 때 `Signed` 출금에 nonce를 새로 할당해 다시 서명(같은 nonce로 다시 서명하면 결정적 서명이라 같은 바이트가 나와 변형이 안 된다) | 05-withdrawal |
| T6.1 | F4 | `i6_external_uncertainty::send_timeout_moves_to_unknown` | 전송 요청은 반영하고 응답만 버림 | 타임아웃을 `Failed`로 매핑하고 홀드 해제 | 05-withdrawal |
| T6.2 | F4 | `i6_external_uncertainty::lost_response_resolved_by_receipt` | 응답 유실 뒤 tx가 채굴됨, 조회도 몇 번 타임아웃 | `Unknown`이 정해진 시간 뒤 `Failed`로 바뀜 | 05-withdrawal |
| T6.3 | F5 | `i6_external_uncertainty::nonce_conflict_never_double_pays` | (가) 내 tx가 이미 채굴된 뒤 재전송이 "nonce too low"를 받음 (나) 외부 tx가 같은 nonce를 먼저 씀. 단언: (가) 지급 1회, `Confirmed` (나) `NeedsReview`, 홀드 유지, 우리 지급 0회 | "nonce too low"에서 새 nonce로 다시 서명((가)에서 이중 지급, (나)에서 `NeedsReview`가 아님) | 05-withdrawal |
| T6.4 | F6 | `i6_external_uncertainty::resend_uses_identical_raw_tx` | 첫 전송은 반영하고 응답만 버린 뒤 `Unknown`에서 여러 번 재전송. 단언: 노드가 받은 raw tx가 모두 저장본과 같고 지급 1회 | 재전송 때마다 nonce를 새로 할당해 다시 서명 | 05-withdrawal |
| T6.5 | F4 | `i6_external_uncertainty::read_timeout_never_skips_block` | 블록 조회 타임아웃·빈 응답을 입금 블록에 주입 | 조회 실패 블록을 빈 블록으로 보고 커서 전진 | 03-deposit-reorg |
| T6.6 | F3, F4, F5, F6 | `i6_external_uncertainty::random_send_faults_pay_at_most_once` | 응답 유실·타임아웃·nonce 충돌·재시작을 무작위로 섞은 출금 열(proptest). 출금 요청마다 체인 지급 수 ≤ 1, 원장 출금액 = 체인 지급액 | T6.1, T6.3, T6.4의 변형 중 어느 하나 | 05-withdrawal |

## 바꾸는 법

- 불변조건 표(I1–I6)는 사람만 고친다. 구현 중 두 불변조건을 동시에 지킬 수 없다는 발견이 나오면 멈추고 사람에게 묻는다.
- 해석, 추적표, 테스트 표는 구현 브랜치에서 고칠 수 있다. 테스트를 나누거나 합치면 같은 커밋에서 이 표를 고친다.
