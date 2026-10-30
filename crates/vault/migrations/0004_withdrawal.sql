-- 출금(ADR-003). 상태는 모두 여기 있고, 처리기를 새로 만들면 이어 간다(결정 7).
-- 시각은 처리기의 Clock이 쓴다(DB의 now()를 쓰지 않는다. 테스트가 가짜 시계로 기한을 만든다).

-- 출금 요청. 같은 request_id는 한 행뿐이다(I2)
CREATE TABLE withdrawals (
    request_id        text          PRIMARY KEY CHECK (request_id <> ''),
    user_id           bigint        NOT NULL,
    to_address        text          NOT NULL CHECK (to_address ~ '^0x[0-9a-f]{40}$'),
    amount            numeric(78,0) NOT NULL CHECK (amount > 0),
    status            text          NOT NULL CHECK (status IN (
                          'held', 'rejected', 'signed', 'sent', 'unknown',
                          'confirmed', 'failed', 'needs_review')),
    created_at        timestamptz   NOT NULL,
    status_changed_at timestamptz   NOT NULL,
    -- 결론(confirmed·failed)을 낸 영수증의 블록. 확정 뒤 깊이의 reorg 감지가 쓴다(결정 6)
    block_hash        text          CHECK (block_hash ~ '^0x[0-9a-f]{64}$')
);

CREATE INDEX withdrawals_open ON withdrawals (created_at, request_id)
    WHERE status IN ('held', 'signed', 'sent', 'unknown', 'needs_review');

-- 서명한 tx. 출금 하나에 하나뿐이고, 재전송은 이 raw 바이트만 보낸다(결정 4)
CREATE TABLE signed_txs (
    request_id   text        NOT NULL UNIQUE REFERENCES withdrawals (request_id),
    from_address text        NOT NULL CHECK (from_address ~ '^0x[0-9a-f]{40}$'),
    nonce        bigint      NOT NULL CHECK (nonce >= 0),
    raw          bytea       NOT NULL,
    tx_hash      text        NOT NULL UNIQUE CHECK (tx_hash ~ '^0x[0-9a-f]{64}$'),
    -- NeedsReview 기한의 시작(결정 3)
    signed_at    timestamptz NOT NULL,
    UNIQUE (from_address, nonce)
);

-- 출금 주소별 다음 nonce. 처음 한 번만 체인의 확정 nonce로 채우고, 그 뒤로는 이 행이 원본이다(결정 5)
CREATE TABLE wallet_nonces (
    address    text   PRIMARY KEY CHECK (address ~ '^0x[0-9a-f]{40}$'),
    next_nonce bigint NOT NULL CHECK (next_nonce >= 0)
);

INSERT INTO schema_version (version) VALUES (4);
