# RDB-001 — Baseline MySQL local e de CI do Salt

> **Histórico.** Pesquisa datada, anterior à remoção do conceito de tenant. Mantida como registro.

> Levantamento realizado em 5 de setembro de 2026 sem conexão com produção e sem leitura de dados de negócio.

## Objetivo

Registrar versões, arquitetura, características dos schemas e convenções de tenancy que afetam o desenvolvimento local do reprodb.

Este relatório não define a matriz de produção. A versão, o TLS e os objetos do source de produção continuam pendentes para a milestone de hardening.

## Ambiente examinado

| Componente | Valor observado |
|---|---|
| Host | macOS 26.5.1, ARM64 |
| Docker | Docker Desktop 27.5.1, `aarch64` |
| Docker context | `desktop-linux`, socket Unix local |
| Container MySQL | `mysql-8` |
| Referência configurada | `mysql:8` |
| Imagem local | `mysql@sha256:1d967fb75a64dc3c2894c69285becfc2304ae0c3c4f4c715c297f3c12d60b01c` |
| Arquitetura da imagem | Linux ARM64 |
| MySQL server | MySQL Community Server 8.4.4 |
| `mysql` no host | MySQL client 8.0.45 para macOS ARM64 |
| `mysqldump` no host | MySQL client 8.0.45 para macOS ARM64 |
| `mysql` no container | MySQL client 8.4.4 para Linux ARM64 |
| `mysqldump` no container | MySQL client 8.4.4 para Linux ARM64 |
| Salt analisado | commit `1e355013d5` |
| Tenancy for Laravel | `stancl/tenancy` v3.9.1 |

## Divergência de versão encontrada

A tag configurada como `mysql:8` resolve localmente para MySQL 8.4.4, enquanto:

- o README do Salt descreve MySQL 8.0 / MySQL >= 8.0;
- o workflow do Salt usa a mesma tag mutável `mysql:8`;
- os clients instalados no host são 8.0.45.

Consequências:

1. `mysql:8` não pode ser tratado como sinônimo de MySQL 8.0;
2. o reprodb precisa consultar `VERSION()` e `@@version_comment`;
3. imagens de client devem ser fixadas por tag exata e digest;
4. compatibilidade deve ser uma matriz explícita de source, client e target;
5. os testes do Salt também podem mudar de série sem alteração no repositório enquanto usarem `mysql:8`.

## Configuração do servidor local

| Propriedade | Valor observado |
|---|---|
| Charset padrão | `utf8mb4` |
| Collation padrão | `utf8mb4_0900_ai_ci` |
| `lower_case_table_names` | `0` |
| GTID | `OFF` |
| `require_secure_transport` | `0` |
| TLS da sessão examinada | não utilizado |
| SQL mode | `ONLY_FULL_GROUP_BY`, `STRICT_TRANS_TABLES`, `NO_ZERO_IN_DATE`, `NO_ZERO_DATE`, `ERROR_FOR_DIVISION_BY_ZERO`, `NO_ENGINE_SUBSTITUTION` |
| Porta publicada | host `0.0.0.0:3306` → container `3306/tcp` |
| Healthcheck | não configurado |

O bind em `0.0.0.0` deve ser mostrado como alerta pelo futuro `reprodb setup`. A ferramenta não deve alterar automaticamente o container existente.

## Schemas Salt observados

Foram encontrados `salt_central` e onze outros schemas `salt_*`.

| Schema | Tabelas InnoDB | Tamanho aproximado |
|---|---:|---:|
| `salt_central` | 424 | 19 MiB |
| `salt_brunotech` | 500 | 29 MiB |
| `salt_ct` | 505 | 2,22 GiB |
| `salt_empresa_1` | 505 | 24 MiB |
| `salt_empresa_2` | 505 | 23 MiB |
| `salt_fk_probe` | 453 | 20 MiB |
| `globex_production` | 504 | 183 MiB |
| `globex_production1` | 456 | 177 MiB |
| `acme_production` | 519 | 2,38 GiB |
| `salt_sigga` | 486 | 858 MiB |
| `salt_test` | 9 | 240 KiB |
| `salt_watt_construtora` | 484 | 48 MiB |

Resumo dos objetos:

| Tipo | Quantidade observada |
|---|---:|
| Base tables | 5.350 |
| Engines diferentes de InnoDB | 0 |
| Views | 0 |
| Triggers | 0 |
| Routines | 0 |
| Events | 0 |
| Foreign keys entre schemas | 0 |

Esses números justificam `--single-transaction` no ambiente local, mas não substituem o preflight do source real. Produção pode conter objetos ou engines diferentes.

## Charset e collation por database

Os schemas não compartilham uma única collation:

- vários usam `utf8mb4_0900_ai_ci`;
- `salt_ct`, `salt_fk_probe`, `acme_production` e `salt_test` usam `utf8mb4_unicode_ci` no ambiente examinado.

O restore deve criar o database target com charset e collation registrados no metadata do dump. Usar apenas o default do target pode mudar comparação e ordenação de strings.

## Regra real de resolução de tenant

O Salt utiliza `stancl/tenancy`. A configuração atual possui prefix e suffix vazios.

O nome do database é resolvido por:

```text
tenant.data.tenancy_db_name, quando definido
ou
tenant.id, quando não definido
```

O database `salt_central` contém:

- `tenants`, cuja chave é o tenant ID;
- `domains`, que relaciona aliases/domains a tenants;
- `tenant_links`, que relaciona tenants pais e filhos.

Exemplos observados sem leitura de dados de negócio:

```text
acme -> acme_production -> acme_production
sigga   -> salt_sigga   -> salt_sigga
watt    -> salt_watt_construtora -> salt_watt_construtora
```

Portanto, o MVP precisa de um `SaltCentralTenantResolver`. Um pattern isolado não cobre domains nem override por `tenancy_db_name`.

## Limite de dados centrais

O JSON `tenants.data` pode conter:

- configuração visual e feature flags;
- override do nome/conexão do database;
- credenciais de integrações do tenant.

O reprodb não deve copiar o JSON completo para o target local. O contrato mínimo a validar na RDB-005 é:

```text
tenant id
tenancy_db_name local
domain/alias local
timestamps exigidos pelo schema
```

Fields relacionados a host, username, password ou APIs não devem ser impressos ou copiados por padrão.

## Snapshot já existente no Salt

O `TenantSnapshotManager` do Salt já demonstra que o schema local pode ser exportado e restaurado com:

```text
--single-transaction
--skip-lock-tables
--no-tablespaces
--set-gtid-purged=OFF
```

Entretanto, essa implementação:

- depende de `mysql` e `mysqldump` no host;
- cria SQL cru no disco;
- passa password por argumento;
- é voltada ao bootstrap de testes, não a dumps de produção.

Ela serve como precedente funcional, não como implementação a reutilizar diretamente.

## Baseline adotado para os próximos spikes

Para RDB-002 e RDB-003:

1. usar o MySQL local 8.4.4 como source/target de desenvolvimento inicial;
2. usar client container 8.4.4 fixado pela imagem/digest já inspecionados;
3. manter `--set-gtid-purged=OFF` mesmo com GTID local desligado;
4. preservar charset/collation no metadata;
5. testar conexão ao host por `host.docker.internal` no macOS;
6. testar `host-gateway` separadamente no Linux;
7. não acessar produção durante os spikes.

Suporte a MySQL 8.0 será incluído somente após validar uma imagem de client e um target nessa série. Não se presume compatibilidade apenas por ambos começarem com “8”.

## Fontes locais

- `Salt/README.md`;
- `Salt/.github/workflows/cicd.yml`;
- `Salt/config/database.php`;
- `Salt/config/tenancy.php`;
- `Salt/app/Models/Tenant.php`;
- `Salt/vendor/stancl/tenancy/src/DatabaseConfig.php`;
- `Salt/app/Support/Testing/TenantBootstrap/TenantSnapshotManager.php`;
- metadata obtida por `docker inspect` sem leitura de environment values;
- `information_schema` e variáveis técnicas do MySQL local.

## Resultado

RDB-001 está concluída para o baseline local/CI. Permanecem deliberadamente desconhecidos até a Milestone 7:

- versão exata de produção;
- vendor e patch de produção;
- TLS de produção;
- GTID de produção;
- objetos e engines de produção;
- grants do usuário de produção.
