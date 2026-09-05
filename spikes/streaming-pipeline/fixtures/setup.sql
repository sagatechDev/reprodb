CREATE DATABASE reprodb_spike_source
    CHARACTER SET utf8mb4
    COLLATE utf8mb4_unicode_ci;

CREATE DATABASE reprodb_spike_target
    CHARACTER SET utf8mb4
    COLLATE utf8mb4_unicode_ci;

CREATE TABLE reprodb_spike_source.accounts (
    id BIGINT UNSIGNED PRIMARY KEY,
    name VARCHAR(120) NOT NULL UNIQUE,
    event_count BIGINT UNSIGNED NOT NULL DEFAULT 0
) ENGINE = InnoDB;

CREATE TABLE reprodb_spike_source.events (
    id BIGINT UNSIGNED PRIMARY KEY,
    account_id BIGINT UNSIGNED NOT NULL,
    code VARCHAR(80) NOT NULL UNIQUE,
    nullable_text TEXT NULL,
    amount DECIMAL(18, 4) NOT NULL,
    occurred_at DATETIME(6) NOT NULL,
    payload BLOB NOT NULL,
    CONSTRAINT events_account_fk
        FOREIGN KEY (account_id) REFERENCES accounts (id)
) ENGINE = InnoDB;

CREATE TRIGGER reprodb_spike_source.events_after_insert
AFTER INSERT ON reprodb_spike_source.events
FOR EACH ROW
UPDATE reprodb_spike_source.accounts
SET event_count = event_count + 1
WHERE id = NEW.account_id;

INSERT INTO reprodb_spike_source.accounts (id, name)
VALUES
    (1, 'Conta ação 🙂'),
    (2, 'Conta dois');

CREATE TABLE reprodb_spike_source.digits (
    n INT NOT NULL PRIMARY KEY
) ENGINE = InnoDB;

INSERT INTO reprodb_spike_source.digits (n)
VALUES (0), (1), (2), (3), (4), (5), (6), (7), (8), (9);

INSERT INTO reprodb_spike_source.events (
    id,
    account_id,
    code,
    nullable_text,
    amount,
    occurred_at,
    payload
)
SELECT
    n + 1,
    1 + MOD(n, 2),
    CONCAT('evt-', n),
    IF(MOD(n, 5) = 0, NULL, REPEAT('ação🙂', 10)),
    CAST(n / 100 AS DECIMAL(18, 4)),
    TIMESTAMP('2024-01-01 00:00:00.000000') + INTERVAL n SECOND,
    UNHEX(LPAD(HEX(n), 32, '0'))
FROM (
    SELECT a.n + 10 * b.n + 100 * c.n + 1000 * d.n AS n
    FROM reprodb_spike_source.digits a
    CROSS JOIN reprodb_spike_source.digits b
    CROSS JOIN reprodb_spike_source.digits c
    CROSS JOIN reprodb_spike_source.digits d
) sequence_rows;

DROP TABLE reprodb_spike_source.digits;

CREATE VIEW reprodb_spike_source.event_summary AS
SELECT
    account_id,
    COUNT(*) AS total,
    SUM(amount) AS amount
FROM reprodb_spike_source.events
GROUP BY account_id;
