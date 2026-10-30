//! 불변조건 테스트를 돕는 시뮬레이터. `FakeChain`(분기·reorg·중복·응답 유실), 장애 주입 지점 구현(`FaultPlan`),
//! 테스트 DB. `vault`를 정방향으로 의존하고, 운영 코드에는 링크하지 않는다.

pub mod chain;
pub mod db;
pub mod faults;
