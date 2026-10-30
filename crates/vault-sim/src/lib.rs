//! 불변조건 테스트를 돕는 시뮬레이터. `FakeChain`(분기·reorg·중복·응답 유실·멤풀·nonce), 장애 주입 지점 구현(`FaultPlan`),
//! 가짜 시계(`FakeClock`), 테스트 키, 테스트 DB. `vault`를 정방향으로 의존하고, 운영 코드에는 링크하지 않는다.

pub mod chain;
pub mod clock;
pub mod db;
pub mod faults;
pub mod keys;
