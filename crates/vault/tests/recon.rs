//! 대사(`vault::recon`)와 운영 자금 입금 스캔(`cap:fund`) 테스트(`--features db`).

#[path = "support/model.rs"]
mod model;
#[path = "support/scan.rs"]
mod scan;
#[path = "support/withdraw.rs"]
mod withdraw;

use vault::chain::{BlockNumber, Hash32};
use vault::deposit::{ScanConfig, ScanError, Scanner};
use vault::ledger::{self, AccountKey, Line, NewEntry};
use vault::recon::{self, Discrepancy, ReconConfig};
use vault::store::NoFaults;
use vault::withdrawal::Status;
use vault_sim::chain::transfer;

use scan::Env;
use withdraw::WEnv;

fn recon_config() -> ReconConfig {
    ReconConfig {
        chain_id: scan::CHAIN_ID,
        confirmations: scan::N,
        hot_wallet: Some(withdraw::hot()),
    }
}

fn scan_config_with_hot(env: &Env) -> ScanConfig {
    ScanConfig {
        hot_wallet: Some(withdraw::hot()),
        ..env.config()
    }
}

/// (1) 정상 흐름: 스캐너의 `cap:fund` 운영 자금 확정 → 입금·reorg·재포함·확정 + 미확정 잠정 입금 →
/// 성공 출금(`Confirmed`)과 실패 출금(`Failed`, 가스비 차감) 뒤 `reconcile` 결과가 불일치 0건이고,
/// 연속 두 번 돌려도 리포트와 DB 상태가 완전히 같다(읽기 전용·멱등).
#[tokio::test]
async fn clean_lifecycle_reconciles_with_zero_discrepancies_and_is_idempotent() {
    let env = Env::new().await;
    let chain = &env.chain;
    let hot = withdraw::hot();

    // 1. 출금 핫월렛으로 운영 자금 전송 → 확정 수 N 전에는 미반영, N 도달 시 cap:fund 자동 기록
    let fund_tx = transfer(1, hot, withdraw::CAPITAL);
    let b1 = chain.mine_canonical(chain.genesis().hash, vec![fund_tx]);
    let mut scanner = Scanner::with_faults(
        env.db.pool.clone(),
        chain.clone(),
        scan_config_with_hot(&env),
        NoFaults,
    )
    .await
    .unwrap();
    scanner.run_until_idle().await.unwrap();
    assert_eq!(
        env.entries("cap:fund:").await,
        0,
        "확정 수 N 전에는 cap:fund를 기록하지 않는다"
    );
    scan::extend(chain, b1, (scan::N - 1) as usize);
    scanner.run_until_idle().await.unwrap();
    assert_eq!(
        env.entries("cap:fund:").await,
        1,
        "확정 수 N 도달 시 cap:fund 1건"
    );
    assert_eq!(
        env.balance(&AccountKey::hot_wallet(hot)).await,
        withdraw::CAPITAL as i128
    );
    assert_eq!(
        env.balance(&AccountKey::operator_capital()).await,
        withdraw::CAPITAL as i128
    );

    // 2. 사용자 1 입금 → 확정 전 reorg → 새 분기에서 재포함 및 확정
    let fork_base = chain.tip().hash;
    let dep1 = transfer(2, scan::addr(1), 1_000);
    chain.mine_canonical(fork_base, vec![dep1]);
    scanner.run_until_idle().await.unwrap();
    assert_eq!(env.balance(&AccountKey::pending_deposit(1)).await, 1_000);

    let b_reorg = chain.mine_canonical(fork_base, vec![]);
    let b_reinc = chain.mine_canonical(b_reorg, vec![dep1]);
    scan::extend(chain, b_reinc, (scan::N - 1) as usize);
    scanner.run_until_idle().await.unwrap();
    assert_eq!(env.balance(&AccountKey::user(1)).await, 1_000);
    assert_eq!(env.balance(&AccountKey::pending_deposit(1)).await, 0);

    // 3. 성공 출금 1건(Confirmed) + 실행 실패 출금 1건(Failed, status 0)
    let w = WEnv::from_env(env);
    let ok_to = withdraw::recipient(1);
    let bad_to = withdraw::recipient(2);
    w.chain().revert_to(bad_to);
    w.request("wd-ok", 1, ok_to, 300).await;
    w.request("wd-fail", 1, bad_to, 200).await;

    let mut proc = w.processor().await;
    w.settle(&mut proc, scan::N as usize + 1).await;
    assert_eq!(w.status("wd-ok").await, Status::Confirmed);
    assert_eq!(w.status("wd-fail").await, Status::Failed);

    // 4. 커서와 confirmed_block 사이에 아직 확정되지 않은 사용자 2의 잠정 입금 추가
    let pending_tx = transfer(3, scan::addr(2), 450);
    w.chain()
        .mine_canonical(w.chain().tip().hash, vec![pending_tx]);
    w.scan().await;
    assert_eq!(w.balance(&AccountKey::pending_deposit(2)).await, 450);

    // 5. 대사 2회 연속 실행: 불일치 0건 + 멱등 + DB 불변
    let keys_before = w.env.keys().await;
    let balances_before = w.env.balances().await;
    let r1 = recon::reconcile(w.pool(), w.chain(), &recon_config())
        .await
        .unwrap();
    let r2 = recon::reconcile(w.pool(), w.chain(), &recon_config())
        .await
        .unwrap();
    assert!(
        r1.is_clean(),
        "정상 흐름 뒤 대사 불일치가 없어야 한다: {:?}",
        r1.discrepancies
    );
    assert_eq!(r1, r2, "대사는 몇 번을 돌려도 같은 리포트를 돌려준다");
    assert_eq!(w.env.keys().await, keys_before, "대사가 분개를 바꿨다");
    assert_eq!(
        w.env.balances().await,
        balances_before,
        "대사가 잔액을 바꿨다"
    );

    w.env.db.drop_db().await;
}

/// (2) 온체인 잔액 변조(`HotWalletBalance`) 감지: 입금 주소와 출금 핫월렛 잔액이 어긋나면 모두 잡아낸다.
/// 변형 `RECON-BALANCE`가 이 테스트를 깨뜨린다.
#[tokio::test]
async fn detects_hot_wallet_balance_discrepancy() {
    let w = WEnv::new().await;
    w.deposit(1, 1_000).await;
    let to = withdraw::recipient(1);
    w.request("wd-1", 1, to, 250).await;
    let mut p = w.processor().await;
    w.settle(&mut p, scan::N as usize + 1).await;

    let clean = recon::reconcile(w.pool(), w.chain(), &recon_config())
        .await
        .unwrap();
    assert!(clean.is_clean(), "{:?}", clean.discrepancies);

    // 입금 주소에서 50 wei가 외부로 빠져나가고, 출금 핫월렛에 미기록 77 wei가 더 들어온 상황 주입
    w.chain().adjust_balance(scan::addr(1), -50);
    w.chain().adjust_balance(withdraw::hot(), 77);

    let report = recon::reconcile(w.pool(), w.chain(), &recon_config())
        .await
        .unwrap();
    let hw_diffs: Vec<_> = report
        .discrepancies
        .iter()
        .filter(|d| matches!(d, Discrepancy::HotWalletBalance { .. }))
        .collect();
    assert_eq!(
        hw_diffs.len(),
        2,
        "HotWalletBalance 불일치를 찾아야 한다: {:?}",
        report.discrepancies
    );
    assert!(
        report.discrepancies.iter().any(|d| matches!(
            d,
            Discrepancy::HotWalletBalance {
                address,
                on_chain: 950,
                ledger: 1_000,
                ..
            } if *address == scan::addr(1)
        )),
        "입금 주소 잔액 불일치 누락: {:?}",
        report.discrepancies
    );
    assert!(
        report.discrepancies.iter().any(|d| matches!(
            d,
            Discrepancy::HotWalletBalance { address, .. } if *address == withdraw::hot()
        )),
        "출금 핫월렛 잔액 불일치 누락: {:?}",
        report.discrepancies
    );

    w.env.db.drop_db().await;
}

/// (3) 입금·출금 상태와 분개·영수증 불일치(`DepositMismatch`, `WithdrawalMismatch`) 감지.
/// 변형 `RECON-WITHDRAWAL`과 `RECON-DEPOSIT`이 이 테스트를 깨뜨린다.
#[tokio::test]
async fn detects_deposit_and_withdrawal_mismatches() {
    let w = WEnv::new().await;
    w.deposit(1, 1_000).await;
    // 두 번째 실제 온체인 입금(나중에 deposits 행을 지워 canonical 블록 역방향 대조를 검증)
    let deleted_dep_tx = transfer(501, scan::addr(1), 300);
    let dep_b = w
        .chain()
        .mine_canonical(w.chain().tip().hash, vec![deleted_dep_tx]);
    scan::extend(w.chain(), dep_b, (scan::N - 1) as usize);
    w.scan().await;

    let to1 = withdraw::recipient(1);
    let to2 = withdraw::recipient(2);
    let to_fail = withdraw::recipient(3);
    w.chain().revert_to(to_fail);
    w.request("wd-settled", 1, to1, 200).await;
    w.request("wd-bad-settle", 1, to2, 150).await;
    w.request("wd-failed-nofee", 1, to_fail, 120).await;
    let mut p = w.processor().await;
    w.settle(&mut p, scan::N as usize + 1).await;
    assert_eq!(w.status("wd-settled").await, Status::Confirmed);
    assert_eq!(w.status("wd-bad-settle").await, Status::Confirmed);
    assert_eq!(w.status("wd-failed-nofee").await, Status::Failed);

    // (a) 서명 tx 없이 Held에서 강제로 'confirmed'로 변조 (missing signed_txs row)
    w.request("wd-forged", 1, withdraw::recipient(4), 100).await;
    sqlx::query("UPDATE withdrawals SET status = 'confirmed' WHERE request_id = 'wd-forged'")
        .execute(w.pool())
        .await
        .unwrap();

    // (b) 실제 서명(`Signed`, 유효한 signed_txs 행 보유)만 하고 브로드캐스트·채굴 없이 'confirmed'로 변조
    //     → recon.rs:809-816 (confirmed withdrawal has no canonical on-chain receipt) 검증
    w.request("wd-no-receipt", 1, withdraw::recipient(5), 80)
        .await;
    let mut p2 = w.processor().await;
    p2.step().await.unwrap();
    assert_eq!(w.status("wd-no-receipt").await, Status::Signed);
    sqlx::query("UPDATE withdrawals SET status = 'confirmed' WHERE request_id = 'wd-no-receipt'")
        .execute(w.pool())
        .await
        .unwrap();

    // (c)–(e) journal_entries는 append-only 트리거가 있으므로 replica 역할로 우회해 손상 주입
    let mut conn = w.env.conn().await;
    sqlx::query("SET session_replication_role = replica")
        .execute(&mut *conn)
        .await
        .unwrap();

    // (c) 실제 온체인 영수증(status 1)이 있는 Confirmed 출금(`wd-bad-settle`)의 wd:settle 분개 변조
    //     → recon.rs:851-860 (confirmed withdrawal missing or mismatched wd:settle entry) 검증
    sqlx::query(
        "UPDATE journal_entries SET kind = 'corrupt' WHERE idempotency_key = 'wd:settle:wd-bad-settle'",
    )
    .execute(&mut *conn)
    .await
    .unwrap();

    // (d) 성공한 출금(`wd-settled`, 영수증 status 1)에서 wd:settle을 지우고 유효한 wd:release를 넣은 뒤 'failed'로 변조
    //     → recon.rs:895-911 (failed withdrawal has successful on-chain receipt (status 1)) 검증
    sqlx::query(
        "DELETE FROM journal_lines WHERE entry_id IN (
             SELECT id FROM journal_entries WHERE idempotency_key = 'wd:settle:wd-settled'
         )",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query("DELETE FROM journal_entries WHERE idempotency_key = 'wd:settle:wd-settled'")
        .execute(&mut *conn)
        .await
        .unwrap();

    // (e) 실제 온체인 실행 실패 출금(`wd-failed-nofee`, 영수증 status 0 + wd:release 보유)의 wd:fee 분개 변조
    //     → recon.rs:912-931 (failed withdrawal missing or mismatched wd:fee entry) 검증
    sqlx::query(
        "UPDATE journal_entries SET kind = 'corrupt' WHERE idempotency_key LIKE 'wd:fee:%'",
    )
    .execute(&mut *conn)
    .await
    .unwrap();

    sqlx::query("SET session_replication_role = origin")
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);

    let mut tx = w.pool().begin().await.unwrap();
    ledger::post(
        &mut tx,
        NewEntry {
            idempotency_key: "wd:release:wd-settled".into(),
            kind: "wd:release".into(),
            lines: vec![
                Line::debit(AccountKey::withdrawal_pending(1), model::amt(200)),
                Line::credit(AccountKey::user(1), model::amt(200)),
            ],
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    sqlx::query("UPDATE withdrawals SET status = 'failed' WHERE request_id = 'wd-settled'")
        .execute(w.pool())
        .await
        .unwrap();

    // (f) 온체인에 없던 가짜 confirmed 입금 행을 deposits에 삽입(dep:pending·dep:confirm 분개 없음)
    let cursor_hash = w.env.cursor().await.hash;
    let fake_tx = Hash32([0xdd; 32]);
    sqlx::query(
        "INSERT INTO deposits (tx_hash, block_hash, position, address, user_id, amount, status)
         VALUES ($1, $2, 99, $3, 1, 500, 'confirmed')",
    )
    .bind(fake_tx.to_string())
    .bind(cursor_hash.to_string())
    .bind(scan::addr(1).to_string())
    .execute(w.pool())
    .await
    .unwrap();

    // (g) 실제 온체인 canonical 블록에 들어 있는 입금(`deleted_dep_tx`)의 deposits 행 삭제
    //     → recon.rs:582-626 (on-chain deposit missing from deposits table) 검증
    sqlx::query("DELETE FROM deposits WHERE tx_hash = $1")
        .bind(deleted_dep_tx.hash.to_string())
        .execute(w.pool())
        .await
        .unwrap();

    let report = recon::reconcile(w.pool(), w.chain(), &recon_config())
        .await
        .unwrap();

    assert!(
        report.discrepancies.iter().any(|d| matches!(
            d,
            Discrepancy::DepositMismatch { tx_hash, .. } if *tx_hash == fake_tx
        )),
        "DepositMismatch 불일치를 찾아야 한다(fake_tx): {:?}",
        report.discrepancies
    );
    assert!(
        report.discrepancies.iter().any(|d| matches!(
            d,
            Discrepancy::DepositMismatch { tx_hash, reason, .. }
                if *tx_hash == deleted_dep_tx.hash
                    && reason.contains("on-chain deposit missing from deposits table")
        )),
        "온체인 입금의 deposits 행 누락 불일치를 찾아야 한다: {:?}",
        report.discrepancies
    );
    assert!(
        report.discrepancies.iter().any(|d| matches!(
            d,
            Discrepancy::WithdrawalMismatch { request_id, .. } if request_id == "wd-forged"
        )),
        "WithdrawalMismatch 불일치를 찾아야 한다(wd-forged): {:?}",
        report.discrepancies
    );
    assert!(
        report.discrepancies.iter().any(|d| matches!(
            d,
            Discrepancy::WithdrawalMismatch { request_id, reason, .. }
                if request_id == "wd-no-receipt"
                    && reason.contains("no canonical on-chain receipt")
        )),
        "WithdrawalMismatch 불일치를 찾아야 한다(영수증·wd:settle 검증: wd-no-receipt): {:?}",
        report.discrepancies
    );
    assert!(
        report.discrepancies.iter().any(|d| matches!(
            d,
            Discrepancy::WithdrawalMismatch { request_id, reason, .. }
                if request_id == "wd-bad-settle"
                    && reason.contains("missing or mismatched wd:settle entry")
        )),
        "WithdrawalMismatch 불일치를 찾아야 한다(영수증·wd:settle 검증: wd-bad-settle): {:?}",
        report.discrepancies
    );
    assert!(
        report.discrepancies.iter().any(|d| matches!(
            d,
            Discrepancy::WithdrawalMismatch { request_id, reason, .. }
                if request_id == "wd-settled"
                    && reason.contains("successful on-chain receipt (status 1)")
        )),
        "WithdrawalMismatch 불일치를 찾아야 한다(wd-settled status 1): {:?}",
        report.discrepancies
    );
    assert!(
        report.discrepancies.iter().any(|d| matches!(
            d,
            Discrepancy::WithdrawalMismatch { request_id, reason, .. }
                if request_id == "wd-failed-nofee"
                    && reason.contains("missing or mismatched wd:fee entry")
        )),
        "WithdrawalMismatch 불일치를 찾아야 한다(wd-failed-nofee): {:?}",
        report.discrepancies
    );

    w.env.db.drop_db().await;
}

/// (4) 원장 등식 위반(`LedgerEquation`) 및 음수 잔액(`NegativeAccount`), 그리고
/// 확정된 운영 자금 입금(`cap:fund`)의 깊은 reorg 시 정지·사고 기록 검증.
#[tokio::test]
async fn detects_ledger_equation_violation_and_halts_on_deep_reorg_of_operator_funding() {
    let env = Env::new().await;
    let chain = &env.chain;
    let hot = withdraw::hot();
    let g = chain.genesis().hash;

    // 1. 운영 자금 입금 후 확정 수 N 도달 → cap:fund 기록
    let fund_tx = transfer(77, hot, 5_000);
    let b1 = chain.mine_canonical(g, vec![fund_tx]);
    scan::extend(chain, b1, (scan::N - 1) as usize);
    let mut scanner = Scanner::with_faults(
        env.db.pool.clone(),
        chain.clone(),
        scan_config_with_hot(&env),
        NoFaults,
    )
    .await
    .unwrap();
    scanner.run_until_idle().await.unwrap();
    assert_eq!(env.entries("cap:fund:").await, 1);

    // 2. 확정된 운영 자금 블록(b1)을 빼는 깊은 reorg → finality_violations 기록 및 스캐너 정지
    scan::extend(chain, g, (scan::N + 2) as usize);
    let err = scanner.run_until_idle().await;
    assert!(
        matches!(err, Err(ScanError::Halted(_))),
        "확정된 cap:fund가 깊은 reorg로 빠지면 정지해야 한다: {err:?}"
    );
    let violations = env.violations().await;
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].2, fund_tx.hash.to_string());

    // 3. 트리거를 우회해(session_replication_role = replica) 음수 잔액·차대 불일치 분개 줄 주입 → 대사가 검출
    let mut conn = env.conn().await;
    sqlx::query("SET session_replication_role = replica")
        .execute(&mut *conn)
        .await
        .unwrap();
    let entry_id: i64 = sqlx::query_scalar(
        "INSERT INTO journal_entries (idempotency_key, kind, body_hash)
         VALUES ('corrupt:1', 'corrupt', $1) RETURNING id",
    )
    .bind(vec![0u8; 32])
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO journal_lines (entry_id, account_key, amount)
         VALUES ($1, $2, 999)",
    )
    .bind(entry_id)
    .bind(AccountKey::user(1).as_str())
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query("SET session_replication_role = origin")
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);

    let report = recon::reconcile(&env.db.pool, chain, &recon_config())
        .await
        .unwrap();
    assert!(
        report.discrepancies.iter().any(|d| matches!(
            d,
            Discrepancy::NegativeAccount { account, balance: -999 }
                if *account == AccountKey::user(1)
        )),
        "NegativeAccount 불일치 누락: {:?}",
        report.discrepancies
    );
    assert!(
        report
            .discrepancies
            .iter()
            .any(|d| matches!(d, Discrepancy::LedgerEquation { .. })),
        "LedgerEquation 불일치 누락: {:?}",
        report.discrepancies
    );
    assert_eq!(report.cursor.map(|c| c.number), Some(BlockNumber(scan::N)));

    env.db.drop_db().await;
}
