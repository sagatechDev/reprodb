SELECT
    COUNT(*),
    SUM(amount),
    BIT_XOR(CRC32(payload)),
    BIT_XOR(CRC32(COALESCE(nullable_text, '<NULL>'))),
    MIN(occurred_at),
    MAX(occurred_at)
FROM events;

SELECT COUNT(*), SUM(event_count)
FROM accounts;

SELECT COUNT(*)
FROM information_schema.REFERENTIAL_CONSTRAINTS
WHERE CONSTRAINT_SCHEMA = DATABASE();

SELECT COUNT(*)
FROM information_schema.TRIGGERS
WHERE TRIGGER_SCHEMA = DATABASE();

SELECT COUNT(*)
FROM information_schema.VIEWS
WHERE TABLE_SCHEMA = DATABASE();

SELECT DEFAULT_COLLATION_NAME
FROM information_schema.SCHEMATA
WHERE SCHEMA_NAME = DATABASE();
