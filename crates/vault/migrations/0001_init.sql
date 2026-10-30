-- 스키마 버전 표. 원장·체인 테이블은 02 이후 새 마이그레이션 파일로 더한다(이 파일은 고치지 않는다).
CREATE TABLE schema_version (
    version    integer     PRIMARY KEY,
    applied_at timestamptz NOT NULL DEFAULT now()
);

INSERT INTO schema_version (version) VALUES (1);
