SELECT
    (SELECT COUNT(*) FROM reprodb_spike_source.events)
        = (SELECT COUNT(*) FROM reprodb_spike_target.events) AS row_count_match,
    (SELECT SUM(amount) FROM reprodb_spike_source.events)
        = (SELECT SUM(amount) FROM reprodb_spike_target.events) AS decimal_match,
    (SELECT BIT_XOR(CRC32(payload)) FROM reprodb_spike_source.events)
        = (SELECT BIT_XOR(CRC32(payload)) FROM reprodb_spike_target.events) AS blob_match,
    (
        SELECT BIT_XOR(CRC32(COALESCE(nullable_text, '<NULL>')))
        FROM reprodb_spike_source.events
    ) = (
        SELECT BIT_XOR(CRC32(COALESCE(nullable_text, '<NULL>')))
        FROM reprodb_spike_target.events
    ) AS utf8_null_match,
    (SELECT SUM(event_count) FROM reprodb_spike_source.accounts)
        = (SELECT SUM(event_count) FROM reprodb_spike_target.accounts) AS trigger_state_match,
    (SELECT COUNT(*) FROM reprodb_spike_target.event_summary) = 2 AS view_match;

SELECT COUNT(*), SUM(event_count)
FROM reprodb_spike_target.accounts;

SELECT COUNT(*)
FROM information_schema.REFERENTIAL_CONSTRAINTS
WHERE CONSTRAINT_SCHEMA = 'reprodb_spike_target';

SELECT COUNT(*)
FROM information_schema.TRIGGERS
WHERE TRIGGER_SCHEMA = 'reprodb_spike_target';

SELECT DEFAULT_COLLATION_NAME
FROM information_schema.SCHEMATA
WHERE SCHEMA_NAME = 'reprodb_spike_target';
