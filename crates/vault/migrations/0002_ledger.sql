-- 복식부기 원장(ADR-001). 잔액 열은 두지 않는다. 잔액은 journal_lines.amount의 합이다.

-- 계정. 키가 곧 이름이다(`user:42`, `hot_wallet:0xabc`, `network_fee`). 종류는 키의 앞부분과 같다
CREATE TABLE accounts (
    key        text        PRIMARY KEY,
    kind       text        NOT NULL CHECK (kind IN (
                   'user', 'pending_deposit', 'withdrawal_pending',
                   'hot_wallet', 'network_fee', 'operator_capital')),
    created_at timestamptz NOT NULL DEFAULT now()
);

-- 분개. 멱등 키는 외부 사실로만 만들고(ADR-001 결정 6), 같은 키의 재기록은 body_hash로 같은 내용인지 본다
CREATE TABLE journal_entries (
    id              bigserial   PRIMARY KEY,
    idempotency_key text        NOT NULL UNIQUE,
    kind            text        NOT NULL,
    body_hash       bytea       NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now()
);

-- 분개 줄. 차변 +, 대변 −. wei가 들어가도록 78자리
CREATE TABLE journal_lines (
    id          bigserial     PRIMARY KEY,
    entry_id    bigint        NOT NULL REFERENCES journal_entries (id),
    account_key text          NOT NULL REFERENCES accounts (key),
    amount      numeric(78,0) NOT NULL CHECK (amount <> 0)
);

CREATE INDEX journal_lines_entry_id ON journal_lines (entry_id);
CREATE INDEX journal_lines_account_key ON journal_lines (account_key);

-- 차대 검사(ADR-001 결정 7). 커밋 시점에 분개마다 줄 2개 이상, 줄 합 0인지 본다.
-- Rust 검사를 거치지 않은 SQL(마이그레이션, 수동 작업)도 이 검사를 피하지 못한다
CREATE FUNCTION journal_check_balanced(p_entry_id bigint) RETURNS void
LANGUAGE plpgsql AS $$
DECLARE
    n   bigint;
    s   numeric;
BEGIN
    SELECT count(*), coalesce(sum(amount), 0) INTO n, s
      FROM journal_lines WHERE entry_id = p_entry_id;
    IF n < 2 OR s <> 0 THEN
        RAISE EXCEPTION 'journal entry % is unbalanced: % lines, sum %', p_entry_id, n, s
            USING ERRCODE = 'check_violation', CONSTRAINT = 'journal_entry_balanced';
    END IF;
END;
$$;

CREATE FUNCTION journal_lines_balanced() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM journal_check_balanced(NEW.entry_id);
    RETURN NULL;
END;
$$;

CREATE FUNCTION journal_entries_balanced() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM journal_check_balanced(NEW.id);
    RETURN NULL;
END;
$$;

-- 줄을 넣은 분개와, 줄이 하나도 없는 분개 둘 다 커밋 때 걸린다
CREATE CONSTRAINT TRIGGER journal_lines_balanced
    AFTER INSERT ON journal_lines
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION journal_lines_balanced();

CREATE CONSTRAINT TRIGGER journal_entries_balanced
    AFTER INSERT ON journal_entries
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION journal_entries_balanced();

-- 추가 전용(ADR-001 결정 2). 고칠 일은 역분개로만 한다. 테스트·로컬 사용자가 superuser라
-- REVOKE로는 막히지 않으므로 트리거로 막는다
CREATE FUNCTION journal_append_only() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION '% is append-only: % rejected', TG_TABLE_NAME, TG_OP
        USING ERRCODE = 'restrict_violation';
END;
$$;

CREATE TRIGGER journal_entries_append_only
    BEFORE UPDATE OR DELETE ON journal_entries
    FOR EACH ROW EXECUTE FUNCTION journal_append_only();

CREATE TRIGGER journal_lines_append_only
    BEFORE UPDATE OR DELETE ON journal_lines
    FOR EACH ROW EXECUTE FUNCTION journal_append_only();

CREATE TRIGGER journal_entries_no_truncate
    BEFORE TRUNCATE ON journal_entries
    FOR EACH STATEMENT EXECUTE FUNCTION journal_append_only();

CREATE TRIGGER journal_lines_no_truncate
    BEFORE TRUNCATE ON journal_lines
    FOR EACH STATEMENT EXECUTE FUNCTION journal_append_only();

INSERT INTO schema_version (version) VALUES (2);
