SELECT
    (SELECT COUNT(*) FROM events) = 10000,
    (SELECT COUNT(*) FROM accounts) = 2,
    (SELECT SUM(event_count) FROM accounts) = 10000,
    (
        SELECT COUNT(*)
        FROM information_schema.REFERENTIAL_CONSTRAINTS
        WHERE CONSTRAINT_SCHEMA = DATABASE()
    ) = 1,
    (
        SELECT COUNT(*)
        FROM information_schema.TRIGGERS
        WHERE TRIGGER_SCHEMA = DATABASE()
    ) = 1,
    (
        SELECT COUNT(*)
        FROM information_schema.VIEWS
        WHERE TABLE_SCHEMA = DATABASE()
    ) = 1,
    (
        SELECT DEFAULT_COLLATION_NAME
        FROM information_schema.SCHEMATA
        WHERE SCHEMA_NAME = DATABASE()
    ) = 'utf8mb4_unicode_ci';
