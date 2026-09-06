CREATE TABLE tenants (
    id VARCHAR(255) NOT NULL PRIMARY KEY,
    created_at TIMESTAMP NULL,
    updated_at TIMESTAMP NULL,
    data JSON NULL
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_unicode_ci;

CREATE TABLE domains (
    id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
    domain VARCHAR(255) NOT NULL UNIQUE,
    tenant_id VARCHAR(255) NOT NULL,
    created_at TIMESTAMP NULL,
    updated_at TIMESTAMP NULL,
    CONSTRAINT domains_tenant_fk FOREIGN KEY (tenant_id) REFERENCES tenants (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_unicode_ci;

CREATE TABLE tenant_links (
    id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
    parent_tenant_id VARCHAR(255) NOT NULL,
    child_tenant_id VARCHAR(255) NOT NULL
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_unicode_ci;

INSERT INTO tenants (id, data) VALUES
    ('salt_by_id', NULL),
    ('salt_sagatec', JSON_OBJECT()),
    ('salt_polymer', JSON_OBJECT('tenancy_db_name', 'salt_polymer_data')),
    ('salt_sensitive', JSON_OBJECT(
        'tenancy_db_host', 'production.fixture.invalid',
        'tenancy_db_port', 4406,
        'tenancy_db_username', 'fixture_prod_user',
        'tenancy_db_password', 'fixture-only-password-do-not-use',
        'tenancy_api_token', 'fixture-only-token-do-not-use'
    )),
    ('salt_admin_override', JSON_OBJECT('tenancy_db_name', 'mysql')),
    ('collision', JSON_OBJECT()),
    ('salt_collision_domain', JSON_OBJECT());

INSERT INTO domains (domain, tenant_id) VALUES
    ('sagatec', 'salt_sagatec'),
    ('polymer', 'salt_polymer'),
    ('sensitive', 'salt_sensitive'),
    ('admin-override', 'salt_admin_override'),
    ('collision', 'salt_collision_domain');

INSERT INTO tenant_links (parent_tenant_id, child_tenant_id)
VALUES ('salt_sagatec', 'salt_polymer');
