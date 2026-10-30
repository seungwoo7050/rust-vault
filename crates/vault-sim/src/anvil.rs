//! 로컬 `anvil` 프로세스를 띄워 JSON-RPC로 통신하는 `ChainClient` 구현(`--features anvil`).
//!
//! `FakeChain`에서 증명한 스캐너(reorg 감지·되감기·확정), 출금 처리기(EIP-1559 서명·전송·영수증 확정),
//! 대사(`recon::reconcile`)가 실제 EVM 노드의 블록·영수증·잔액·`evm_snapshot`/`evm_revert` reorg 위에서도
//! 그대로 성립하는지 검증한다.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use vault::chain::{
    Address, Block, BlockHeader, BlockNumber, ChainClient, ChainTx, Hash32, Receipt, RpcError,
    RpcOutcome,
};

struct AnvilInner {
    port: u16,
    child: Mutex<Child>,
}

impl Drop for AnvilInner {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// 로컬 `anvil` 노드를 감싸는 `ChainClient`.
#[derive(Clone)]
pub struct AnvilClient(Arc<AnvilInner>);

impl AnvilClient {
    /// 빈 로컬 포트에 `anvil --no-mining` 프로세스를 띄우고 JSON-RPC 응답이 올 때까지 기다린다.
    pub async fn spawn(chain_id: u64) -> Result<Self, String> {
        let bin = find_anvil_bin();
        let listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| format!("bind port: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("local_addr: {e}"))?
            .port();
        drop(listener);

        let child = Command::new(&bin)
            .args([
                "--port",
                &port.to_string(),
                "--chain-id",
                &chain_id.to_string(),
                "--no-mining",
                "--silent",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn {bin}: {e}"))?;

        let client = Self(Arc::new(AnvilInner {
            port,
            child: Mutex::new(child),
        }));

        for _ in 0..50 {
            if matches!(client.head().await, RpcOutcome::Ok(_)) {
                return Ok(client);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err("anvil did not become ready in time".into())
    }

    /// genesis(0번) 블록 헤더.
    pub async fn genesis(&self) -> BlockHeader {
        match self.block_by_number(BlockNumber(0)).await {
            RpcOutcome::Ok(Some(b)) => b.header,
            other => panic!("genesis block: {other:?}"),
        }
    }

    /// 지금 canonical tip 헤더.
    pub async fn tip(&self) -> BlockHeader {
        let RpcOutcome::Ok(n) = self.head().await else {
            panic!("head failed");
        };
        match self.block_by_number(n).await {
            RpcOutcome::Ok(Some(b)) => b.header,
            other => panic!("tip block {n}: {other:?}"),
        }
    }

    /// 현재 체인 상태의 스냅숏 ID를 만든다(`evm_snapshot`).
    pub async fn snapshot(&self) -> String {
        match self.rpc("evm_snapshot", json!([])).await {
            RpcOutcome::Ok(v) => v.as_str().expect("snapshot id string").to_owned(),
            other => panic!("evm_snapshot: {other:?}"),
        }
    }

    /// 스냅숏 ID로 체인을 되돌린다(`evm_revert`).
    pub async fn revert(&self, id: &str) {
        match self.rpc("evm_revert", json!([id])).await {
            RpcOutcome::Ok(v) if v.as_bool() == Some(true) => {}
            other => panic!("evm_revert({id}): {other:?}"),
        }
    }

    /// 다음 블록 타임스탬프를 늘린다(`evm_increaseTime`). 분기마다 다른 블록 해시를 만들 때 쓴다.
    pub async fn increase_time(&self, seconds: u64) {
        match self.rpc("evm_increaseTime", json!([seconds])).await {
            RpcOutcome::Ok(_) => {}
            other => panic!("evm_increaseTime: {other:?}"),
        }
    }

    /// 블록을 `blocks`개 채굴한다(`evm_mine`).
    pub async fn mine(&self, blocks: usize) {
        for _ in 0..blocks {
            match self.rpc("evm_mine", json!([])).await {
                RpcOutcome::Ok(_) => {}
                other => panic!("evm_mine: {other:?}"),
            }
        }
    }

    /// 주소의 잔액을 설정한다(`anvil_setBalance`).
    pub async fn set_balance(&self, address: Address, wei: u128) {
        let hex_wei = format!("0x{wei:x}");
        match self
            .rpc("anvil_setBalance", json!([address.to_string(), hex_wei]))
            .await
        {
            RpcOutcome::Ok(_) => {}
            other => panic!("anvil_setBalance: {other:?}"),
        }
    }

    /// `from` 주소를 가장(`anvil_impersonateAccount`)해 `to`로 `value` wei 전송을 멤풀에 넣는다.
    pub async fn send_eth(&self, from: Address, to: Address, value: u128) -> Hash32 {
        match self
            .rpc("anvil_impersonateAccount", json!([from.to_string()]))
            .await
        {
            RpcOutcome::Ok(_) => {}
            other => panic!("anvil_impersonateAccount: {other:?}"),
        }
        let tx = json!({
            "from": from.to_string(),
            "to": to.to_string(),
            "value": format!("0x{value:x}"),
        });
        match self.rpc("eth_sendTransaction", json!([tx])).await {
            RpcOutcome::Ok(v) => {
                let s = v.as_str().expect("tx hash string").to_ascii_lowercase();
                Hash32::parse(&s).expect("valid tx hash")
            }
            other => panic!("eth_sendTransaction: {other:?}"),
        }
    }

    async fn rpc(&self, method: &str, params: Value) -> RpcOutcome<Value> {
        let port = self.0.port;
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        })
        .to_string();

        let resp_bytes = match send_http_post(port, &body) {
            Ok(b) => b,
            Err(e) => return RpcOutcome::Indeterminate(e),
        };
        let parsed: Value = match serde_json::from_slice(&resp_bytes) {
            Ok(v) => v,
            Err(e) => return RpcOutcome::Indeterminate(format!("invalid json: {e}")),
        };
        if let Some(err) = parsed.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("rpc error")
                .to_owned();
            return RpcOutcome::Definite(RpcError(msg));
        }
        match parsed.get("result") {
            Some(r) => RpcOutcome::Ok(r.clone()),
            None => RpcOutcome::Indeterminate("missing result field".into()),
        }
    }
}

impl ChainClient for AnvilClient {
    async fn head(&self) -> RpcOutcome<BlockNumber> {
        let val = match self.rpc("eth_blockNumber", json!([])).await {
            RpcOutcome::Ok(v) => v,
            RpcOutcome::Definite(e) => return RpcOutcome::Definite(e),
            RpcOutcome::Indeterminate(m) => return RpcOutcome::Indeterminate(m),
        };
        let Some(n) = val.as_str().and_then(parse_hex_u64) else {
            return RpcOutcome::Indeterminate(format!("bad blockNumber: {val}"));
        };
        RpcOutcome::Ok(BlockNumber(n))
    }

    async fn block_by_number(&self, number: BlockNumber) -> RpcOutcome<Option<Block>> {
        let hex_num = format!("0x{:x}", number.0);
        let val = match self
            .rpc("eth_getBlockByNumber", json!([hex_num, true]))
            .await
        {
            RpcOutcome::Ok(v) => v,
            RpcOutcome::Definite(e) => return RpcOutcome::Definite(e),
            RpcOutcome::Indeterminate(m) => return RpcOutcome::Indeterminate(m),
        };
        if val.is_null() {
            return RpcOutcome::Ok(None);
        }
        let Some(num) = val
            .get("number")
            .and_then(Value::as_str)
            .and_then(parse_hex_u64)
        else {
            return RpcOutcome::Indeterminate("missing block.number".into());
        };
        let Some(hash) = val
            .get("hash")
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase)
            .and_then(|s| Hash32::parse(&s))
        else {
            return RpcOutcome::Indeterminate("missing block.hash".into());
        };
        let Some(parent_hash) = val
            .get("parentHash")
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase)
            .and_then(|s| Hash32::parse(&s))
        else {
            return RpcOutcome::Indeterminate("missing block.parentHash".into());
        };
        let Some(raw_txs) = val.get("transactions").and_then(Value::as_array) else {
            return RpcOutcome::Indeterminate("missing block.transactions".into());
        };

        let mut txs = Vec::with_capacity(raw_txs.len());
        for t in raw_txs {
            let Some(tx_hash) = t
                .get("hash")
                .and_then(Value::as_str)
                .map(str::to_ascii_lowercase)
                .and_then(|s| Hash32::parse(&s))
            else {
                return RpcOutcome::Indeterminate("missing tx.hash".into());
            };
            let to = match t.get("to").and_then(Value::as_str) {
                Some(s) => match Address::parse(&s.to_ascii_lowercase()) {
                    Some(a) => a,
                    None => return RpcOutcome::Indeterminate("bad tx.to".into()),
                },
                None => Address([0; 20]),
            };
            let Some(value) = t
                .get("value")
                .and_then(Value::as_str)
                .and_then(parse_hex_u128)
            else {
                return RpcOutcome::Indeterminate("missing tx.value".into());
            };
            let success = match self.receipt(tx_hash).await {
                RpcOutcome::Ok(Some(r)) => r.success,
                RpcOutcome::Ok(None) => false,
                RpcOutcome::Definite(e) => return RpcOutcome::Definite(e),
                RpcOutcome::Indeterminate(m) => return RpcOutcome::Indeterminate(m),
            };
            txs.push(ChainTx {
                hash: tx_hash,
                to,
                value,
                success,
            });
        }

        RpcOutcome::Ok(Some(Block {
            header: BlockHeader {
                number: BlockNumber(num),
                hash,
                parent_hash,
            },
            txs,
        }))
    }

    async fn send_raw(&self, raw: &[u8]) -> RpcOutcome<Hash32> {
        let mut hex_raw = String::with_capacity(2 + raw.len() * 2);
        hex_raw.push_str("0x");
        for b in raw {
            use std::fmt::Write;
            let _ = write!(&mut hex_raw, "{b:02x}");
        }
        let val = match self.rpc("eth_sendRawTransaction", json!([hex_raw])).await {
            RpcOutcome::Ok(v) => v,
            RpcOutcome::Definite(e) => return RpcOutcome::Definite(e),
            RpcOutcome::Indeterminate(m) => return RpcOutcome::Indeterminate(m),
        };
        let Some(hash) = val
            .as_str()
            .map(str::to_ascii_lowercase)
            .and_then(|s| Hash32::parse(&s))
        else {
            return RpcOutcome::Indeterminate(format!("bad sendRawTransaction result: {val}"));
        };
        RpcOutcome::Ok(hash)
    }

    async fn receipt(&self, tx: Hash32) -> RpcOutcome<Option<Receipt>> {
        let val = match self
            .rpc("eth_getTransactionReceipt", json!([tx.to_string()]))
            .await
        {
            RpcOutcome::Ok(v) => v,
            RpcOutcome::Definite(e) => return RpcOutcome::Definite(e),
            RpcOutcome::Indeterminate(m) => return RpcOutcome::Indeterminate(m),
        };
        if val.is_null() {
            return RpcOutcome::Ok(None);
        }
        let Some(block_hash) = val
            .get("blockHash")
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase)
            .and_then(|s| Hash32::parse(&s))
        else {
            return RpcOutcome::Indeterminate("missing receipt.blockHash".into());
        };
        let Some(block_number) = val
            .get("blockNumber")
            .and_then(Value::as_str)
            .and_then(parse_hex_u64)
        else {
            return RpcOutcome::Indeterminate("missing receipt.blockNumber".into());
        };
        let Some(status) = val
            .get("status")
            .and_then(Value::as_str)
            .and_then(parse_hex_u64)
        else {
            return RpcOutcome::Indeterminate("missing receipt.status".into());
        };
        let Some(gas_used) = val
            .get("gasUsed")
            .and_then(Value::as_str)
            .and_then(parse_hex_u64)
        else {
            return RpcOutcome::Indeterminate("missing receipt.gasUsed".into());
        };
        let Some(effective_gas_price) = val
            .get("effectiveGasPrice")
            .and_then(Value::as_str)
            .and_then(parse_hex_u128)
        else {
            return RpcOutcome::Indeterminate("missing receipt.effectiveGasPrice".into());
        };

        RpcOutcome::Ok(Some(Receipt {
            block_hash,
            block_number: BlockNumber(block_number),
            success: status == 1,
            gas_used,
            effective_gas_price,
        }))
    }

    async fn nonce_at(&self, address: Address, block: BlockNumber) -> RpcOutcome<u64> {
        let hex_block = format!("0x{:x}", block.0);
        let val = match self
            .rpc(
                "eth_getTransactionCount",
                json!([address.to_string(), hex_block]),
            )
            .await
        {
            RpcOutcome::Ok(v) => v,
            RpcOutcome::Definite(e) => return RpcOutcome::Definite(e),
            RpcOutcome::Indeterminate(m) => return RpcOutcome::Indeterminate(m),
        };
        let Some(n) = val.as_str().and_then(parse_hex_u64) else {
            return RpcOutcome::Indeterminate(format!("bad transactionCount: {val}"));
        };
        RpcOutcome::Ok(n)
    }

    async fn balance_at(&self, address: Address, block: BlockNumber) -> RpcOutcome<u128> {
        let hex_block = format!("0x{:x}", block.0);
        let val = match self
            .rpc("eth_getBalance", json!([address.to_string(), hex_block]))
            .await
        {
            RpcOutcome::Ok(v) => v,
            RpcOutcome::Definite(e) => return RpcOutcome::Definite(e),
            RpcOutcome::Indeterminate(m) => return RpcOutcome::Indeterminate(m),
        };
        let Some(b) = val.as_str().and_then(parse_hex_u128) else {
            return RpcOutcome::Indeterminate(format!("bad balance: {val}"));
        };
        RpcOutcome::Ok(b)
    }
}

fn find_anvil_bin() -> String {
    if let Ok(bin) = std::env::var("ANVIL_BIN")
        && !bin.is_empty()
    {
        return bin;
    }
    if let Ok(home) = std::env::var("HOME") {
        let candidate = format!("{home}/.foundry/bin/anvil");
        if std::path::Path::new(&candidate).exists() {
            return candidate;
        }
    }
    "anvil".to_owned()
}

fn send_http_post(port: u16, body: &str) -> Result<Vec<u8>, String> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream =
        TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map_err(|e| e.to_string())?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let req = format!(
        "POST / HTTP/1.1\r\nHost: localhost:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| e.to_string())?;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut header_end = None;
    let mut content_len = None;

    loop {
        let n = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if header_end.is_none()
            && let Some(pos) = find_header_end(&buf)
        {
            header_end = Some(pos);
            content_len = parse_content_length(&buf[..pos]);
        }
        if let (Some(h_end), Some(c_len)) = (header_end, content_len)
            && buf.len() >= h_end + c_len
        {
            return Ok(buf[h_end..h_end + c_len].to_vec());
        }
    }

    let Some(h_end) = header_end else {
        return Err("incomplete http response headers".into());
    };
    Ok(buf[h_end..].to_vec())
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

fn parse_content_length(headers: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(headers).ok()?;
    for line in text.lines() {
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            return v.trim().parse::<usize>().ok();
        }
    }
    None
}

fn parse_hex_u64(s: &str) -> Option<u64> {
    let hex = s.strip_prefix("0x")?;
    u64::from_str_radix(hex, 16).ok()
}

fn parse_hex_u128(s: &str) -> Option<u128> {
    let hex = s.strip_prefix("0x")?;
    u128::from_str_radix(hex, 16).ok()
}
