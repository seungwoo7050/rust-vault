# RustVault

블록체인의 불확실성(중복 이벤트, reorg, RPC 응답 유실, 프로세스 종료) 속에서도 자산의 이동과 기록이 항상 올바르게 처리되는 입출금 시스템을 설계하고 검증한다.

진행 중이다. 설계 문서와 검증 하네스(`scripts/harness/verify.sh fast|full`, 불변조건 테스트 24개의 뼈대)까지 있다. 작업 규칙은 [AGENTS.md](AGENTS.md).

| 문서 | 내용 |
| --- | --- |
| [docs/SYSTEM_INVARIANTS.md](docs/SYSTEM_INVARIANTS.md) | 불변조건 I1–I6, 불변조건 → 장애 시나리오 → 증명 테스트 추적표 |
| [docs/FAILURE_SCENARIOS.md](docs/FAILURE_SCENARIOS.md) | 장애 시나리오 F1–F8과 기대 동작 |
| [docs/REQUIREMENTS.md](docs/REQUIREMENTS.md) | 도메인 용어, 요구사항, 범위 밖 |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | crate·모듈, 저장소 초안, 결정 |
| [docs/adr/](docs/adr/) | 001 원장 모델, 002 reorg와 입금 확정, 003 출금의 nonce와 RPC 불확실성 |

불변조건·장애 시나리오·ADR-002의 선택·범위는 사람이 설계했고, 그 밖의 결정은 오케스트레이터(에이전트)가 확정했다. 각 문서에 표기한다.
