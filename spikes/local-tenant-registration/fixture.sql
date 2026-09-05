CREATE DATABASE reprodb_spike_central
    CHARACTER SET utf8mb4
    COLLATE utf8mb4_unicode_ci;

CREATE DATABASE reprodb_spike_tenant_data
    CHARACTER SET utf8mb4
    COLLATE utf8mb4_unicode_ci;

CREATE TABLE reprodb_spike_central.tenants (
    id VARCHAR(255) NOT NULL PRIMARY KEY,
    created_at TIMESTAMP NULL,
    updated_at TIMESTAMP NULL,
    data JSON NULL
) ENGINE = InnoDB;

CREATE TABLE reprodb_spike_central.domains (
    id INT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
    domain VARCHAR(255) NOT NULL UNIQUE,
    tenant_id VARCHAR(255) NOT NULL,
    created_at TIMESTAMP NULL,
    updated_at TIMESTAMP NULL,
    CONSTRAINT domains_tenant_id_foreign
        FOREIGN KEY (tenant_id)
        REFERENCES reprodb_spike_central.tenants (id)
        ON UPDATE CASCADE
        ON DELETE CASCADE
) ENGINE = InnoDB;

INSERT INTO reprodb_spike_central.tenants (
    id,
    created_at,
    updated_at,
    data
) VALUES (
    'reprodb_spike_tenant',
    CURRENT_TIMESTAMP,
    CURRENT_TIMESTAMP,
    JSON_OBJECT(
        'tenancy_db_name', 'reprodb_spike_tenant_data',
        'tenancy_app_color', '#123456',
        'tenancy_enable_stock_label_control', TRUE
    )
);

INSERT INTO reprodb_spike_central.domains (
    domain,
    tenant_id,
    created_at,
    updated_at
) VALUES (
    'reprodb-spike',
    'reprodb_spike_tenant',
    CURRENT_TIMESTAMP,
    CURRENT_TIMESTAMP
);

CREATE TABLE reprodb_spike_tenant_data.reprodb_marker (
    id BIGINT UNSIGNED NOT NULL PRIMARY KEY,
    value VARCHAR(64) NOT NULL
) ENGINE = InnoDB;

INSERT INTO reprodb_spike_tenant_data.reprodb_marker (id, value)
VALUES (1, 'tenant-ready');
