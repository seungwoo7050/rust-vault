//! 로컬 `anvil` 노드를 띄워 실제 EVM 체인 위에서 입금·reorg(`evm_snapshot`/`evm_revert`)·
//! EIP-1559 서명 출금·사후 대사(`recon::reconcile`) 전 과정을 검증하는 PoC 테스트(`--features "db,anvil"`).

use std::time::Duration;

use vault::chain::{BlockNumber, ChainClient, RpcOutcome};
use vault::deposit::{self, ScanConfig, Scanner, Step as ScanStep};
use vault::ledger::{self, AccountKey, Amount};
use vault::recon::{self, ReconConfig};
use vault::signer::{LocalSigner, Signer};
use vault::store::NoFaults;
use vault::withdrawal::{self, Processor, Status, WithdrawalConfig, WithdrawalRequest};
use vault_sim::anvil::AnvilClient;
use vault_sim::chain as sim;
use vault_sim::clock::FakeClock;
use vault_sim::db::TestDb;

const CHAIN_ID: u64 = 31_337;
const N: u64 = 3;

#[tokio::test]
async fn anvil_reorg_and_withdrawal_reconciles_cleanly() {
    let db = TestDb::create().await;
    let anvil = AnvilClient::spawn(CHAIN_ID).await.expect("anvil 시작");

    // anvil 기본 10개 계정은 시작 잔액이 10,000 ETH이므로, 잔액이 0에서 시작하는 별도 키를 핫월렛으로 쓴다
    let signer = LocalSigner::from_bytes([0x42; 32]).expect("핫월렛 서명기");
    let hot = signer.address();
    let funder = sim::address(500);
    let user_addr = sim::address(1);
    let recipient = sim::address(777);

    anvil.set_balance(funder, 10_000_000_000_000_000_000).await;

    let mut conn = db.pool.acquire().await.unwrap();
    deposit::register_address(&mut conn, user_addr, 1)
        .await
        .unwrap();
    drop(conn);

    let scan_cfg = ScanConfig {
        chain_id: CHAIN_ID,
        confirmations: N,
        start: anvil.genesis().await,
        transient_retries: 3,
        hot_wallet: Some(hot),
    };
    let mut scanner = Scanner::new(db.pool.clone(), anvil.clone(), scan_cfg)
        .await
        .unwrap();

    // 1. 운영 자금 입금(1 ETH) → 블록 채굴 및 확정 수 N 도달 → cap:fund 기록 확인
    let capital: u128 = 1_000_000_000_000_000_000;
    let fund_hash = anvil.send_eth(funder, hot, capital).await;
    anvil.mine(N as usize).await;
    scanner.run_until_idle().await.unwrap();

    let mut conn = db.pool.acquire().await.unwrap();
    assert_eq!(
        ledger::balance(&mut conn, &AccountKey::hot_wallet(hot))
            .await
            .unwrap(),
        capital as i128,
        "운영 자금 cap:fund 반영({fund_hash})"
    );
    drop(conn);

    // 2. 스냅숏 저장 후 분기 A에서 사용자 입금(0.5 ETH) → 잠정 입금(dep:pending) 기록
    let snap = anvil.snapshot().await;
    let branch_a_dep: u128 = 500_000_000_000_000_000;
    anvil.send_eth(funder, user_addr, branch_a_dep).await;
    anvil.mine(1).await;
    scanner.run_until_idle().await.unwrap();

    let mut conn = db.pool.acquire().await.unwrap();
    assert_eq!(
        ledger::balance(&mut conn, &AccountKey::pending_deposit(1))
            .await
            .unwrap(),
        branch_a_dep as i128
    );
    assert_eq!(
        ledger::balance(&mut conn, &AccountKey::user(1))
            .await
            .unwrap(),
        0
    );
    drop(conn);

    // 3. 확정 전 evm_revert + 타임스탬프 변경 후 더 긴 분기 B 채굴 → 스캐너가 부모 해시 불일치로 되감기(dep:revert)
    anvil.revert(&snap).await;
    anvil.increase_time(15).await;
    anvil.mine(2).await;

    let step = scanner.step().await.unwrap();
    assert!(
        matches!(step, ScanStep::Rewound { .. }),
        "부모 해시 불일치로 되감아야 한다: {step:?}"
    );
    scanner.run_until_idle().await.unwrap();

    let mut conn = db.pool.acquire().await.unwrap();
    assert_eq!(
        ledger::balance(&mut conn, &AccountKey::pending_deposit(1))
            .await
            .unwrap(),
        0,
        "되감기 뒤 잠정 입금이 역분개되어야 한다"
    );
    drop(conn);

    // 4. 새 분기 B에서 사용자 입금(0.6 ETH) 및 확정 수 N 도달 → dep:confirm 기록
    let branch_b_dep: u128 = 600_000_000_000_000_000;
    anvil.send_eth(funder, user_addr, branch_b_dep).await;
    anvil.mine(N as usize).await;
    scanner.run_until_idle().await.unwrap();

    let mut conn = db.pool.acquire().await.unwrap();
    assert_eq!(
        ledger::balance(&mut conn, &AccountKey::user(1))
            .await
            .unwrap(),
        branch_b_dep as i128
    );
    assert_eq!(
        ledger::balance(&mut conn, &AccountKey::pending_deposit(1))
            .await
            .unwrap(),
        0
    );
    drop(conn);

    // 5. 출금 요청(0.2 ETH) → alloy EIP-1559 서명 → 실제 anvil 전송 → 채굴·확정(wd:settle)
    let wd_amount: u128 = 200_000_000_000_000_000;
    let w = withdrawal::request(
        &db.pool,
        WithdrawalRequest {
            request_id: "anvil-wd-1".into(),
            user: 1,
            to: recipient,
            amount: Amount::new(wd_amount).unwrap(),
        },
    )
    .await
    .unwrap();
    assert_eq!(w.status, Status::Held);

    let wd_cfg = WithdrawalConfig {
        chain_id: CHAIN_ID,
        confirmations: N,
        max_fee_per_gas: 2_000_000_000,
        max_priority_fee_per_gas: 100_000_000,
        review_after: Duration::from_secs(600),
        transient_retries: 3,
    };
    let mut proc = Processor::new(
        db.pool.clone(),
        anvil.clone(),
        signer,
        FakeClock::new(),
        NoFaults,
        wd_cfg,
    )
    .await
    .unwrap();

    proc.run_until_idle().await.unwrap();
    assert_eq!(
        withdrawal::get(&db.pool, "anvil-wd-1")
            .await
            .unwrap()
            .unwrap()
            .status,
        Status::Sent
    );

    anvil.mine(N as usize).await;
    scanner.run_until_idle().await.unwrap();
    proc.run_until_idle().await.unwrap();

    assert_eq!(
        withdrawal::get(&db.pool, "anvil-wd-1")
            .await
            .unwrap()
            .unwrap()
            .status,
        Status::Confirmed
    );

    let tip = anvil.tip().await.number;
    assert_eq!(
        anvil.balance_at(recipient, tip).await,
        RpcOutcome::Ok(wd_amount),
        "수취인 온체인 잔액"
    );
    assert_eq!(
        anvil.nonce_at(hot, BlockNumber(tip.0)).await,
        RpcOutcome::Ok(1),
        "출금 핫월렛 온체인 nonce"
    );

    // 6. 대사 실행: 불일치 0건
    let report = recon::reconcile(
        &db.pool,
        &anvil,
        &ReconConfig {
            chain_id: CHAIN_ID,
            confirmations: N,
            hot_wallet: Some(hot),
        },
    )
    .await
    .unwrap();
    assert!(
        report.is_clean(),
        "실제 anvil 노드 대사 결과 불일치가 없어야 한다: {:?}",
        report.discrepancies
    );

    db.drop_db().await;
}
