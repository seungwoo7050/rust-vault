-- 입금 스캐너(ADR-002). 해시·주소는 `0x` + 소문자 16진수 텍스트로 둔다(원장 계정 키·멱등 키와 같은 꼴).

-- 입금 주소 → 사용자. 설정이라 스캐너는 읽기만 한다
CREATE TABLE deposit_addresses (
    address text   PRIMARY KEY CHECK (address ~ '^0x[0-9a-f]{40}$'),
    user_id bigint NOT NULL
);

-- 처리한 블록 헤더. reorg로 canonical에서 빠지면 지우지 않고 고아로 표시한다
CREATE TABLE block_headers (
    hash        text    PRIMARY KEY CHECK (hash ~ '^0x[0-9a-f]{64}$'),
    number      bigint  NOT NULL CHECK (number >= 0),
    parent_hash text    NOT NULL CHECK (parent_hash ~ '^0x[0-9a-f]{64}$'),
    orphaned    boolean NOT NULL DEFAULT false
);

-- 번호 하나에 고아가 아닌 헤더는 하나뿐이다(저장된 canonical chain은 한 줄)
CREATE UNIQUE INDEX block_headers_canonical_number ON block_headers (number) WHERE NOT orphaned;

-- 마지막으로 처리한 canonical 블록. 한 행뿐이다
CREATE TABLE scan_cursor (
    id     boolean PRIMARY KEY DEFAULT true CHECK (id),
    number bigint  NOT NULL,
    hash   text    NOT NULL REFERENCES block_headers (hash)
);

-- 블록에서 본 입금. 같은 tx가 reorg 뒤 다른 블록에 다시 들어가면 행이 하나 더 생긴다
CREATE TABLE deposits (
    tx_hash    text          NOT NULL CHECK (tx_hash ~ '^0x[0-9a-f]{64}$'),
    block_hash text          NOT NULL REFERENCES block_headers (hash),
    position   integer       NOT NULL CHECK (position >= 0),
    address    text          NOT NULL REFERENCES deposit_addresses (address),
    user_id    bigint        NOT NULL,
    amount     numeric(78,0) NOT NULL CHECK (amount > 0),
    status     text          NOT NULL CHECK (status IN ('pending', 'confirmed', 'reverted')),
    PRIMARY KEY (tx_hash, block_hash)
);

CREATE INDEX deposits_block_hash ON deposits (block_hash);

-- 확정 정책 가정(N보다 깊은 reorg는 없다)이 깨진 기록. 원장은 바꾸지 않고 사람이 본다(ADR-002 결정 7)
CREATE TABLE finality_violations (
    id          bigserial     PRIMARY KEY,
    kind        text          NOT NULL CHECK (kind IN ('deposit', 'withdrawal')),
    block_hash  text          NOT NULL,
    tx_hash     text          NOT NULL,
    amount      numeric(78,0) NOT NULL,
    recorded_at timestamptz   NOT NULL DEFAULT now()
);

-- 스캐너 정지 여부. 한 행뿐이고, 정지는 사람이 풀 때까지 재시작해도 유지된다
CREATE TABLE scanner_state (
    id     boolean PRIMARY KEY DEFAULT true CHECK (id),
    halted boolean NOT NULL DEFAULT false,
    reason text
);

INSERT INTO scanner_state (halted) VALUES (false);

INSERT INTO schema_version (version) VALUES (3);
