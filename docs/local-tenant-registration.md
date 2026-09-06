# Registro mínimo do tenant no `salt_central` local

> RDB-052 implementa o núcleo pós-restore. A ligação ao comando público `reprodb restore` será feita na RDB-053.

Somente restaurar `salt_sagatec` ou `salt_polymer` não é suficiente para o Salt inicializar a tenancy. O middleware real resolve um registro em `salt_central.tenants`, encontra seu `domain` e então escolhe o database pelo `tenancy_db_name`.

A implementação foi confrontada com as migrations, `ErpSaga\Models\Tenant`, `TenantCatalog::ensureBaseTenants()` e o schema existente no container `mysql-8`:

- `tenants.id`: `varchar(191)` e primary key;
- `tenants.data`: JSON nullable;
- `domains.domain`: unique;
- `domains.tenant_id`: FK para `tenants.id`, com cascade;
- os dois registros possuem timestamps nullable.

## Capability pós-restore

O writer não pode ser chamado pelo fluxo normal apenas com strings. O serviço exige:

```text
AuthorizedLocalTarget
    + RestoreCompleted(tenant ID + database + dump ID)
    + LocalTenantRegistration(tenant ID + domain + database + features tipadas)
```

O database precisa coincidir nos três valores, e o tenant ID precisa coincidir entre o artefato restaurado e o registro. `RestoreCompleted` só é produzido depois que o `RestoreEngine` conclui o import e marca o target como ready. Isso impede cadastrar `salt_polymer` usando um restore concluído de `salt_sagatec`.

`LocalTenantRegistration::from_artifact()` reconstrói o registro usando somente a metadata validada do dump. Um restore por dump ID não precisa consultar o source novamente.

## Snapshot seguro de features

O resolver do `salt_central` source extrai somente esta allowlist:

- `tenancy_app_color`;
- `tenancy_annotation_atm`;
- `tenancy_enable_stock_label_control`;
- `tenancy_enable_sped_contrib`;
- `tenancy_enable_beta`;
- `tenancy_has_cyclic_counting`;
- `tenancy_new_production`.

As seis flags são aceitas apenas como boolean JSON. A cor aceita somente os temas observados no Salt (`blue`, `blueLight`, `gray`, `green`, `orange`, `violet`) ou o formato histórico `#RRGGBB`. Tipo ou valor divergente invalida a resolução em vez de transportar conteúdo arbitrário.

Essas features tipadas ficam em `metadata.json`, dentro do artefato gerenciado em `~/.reprodb/cache`. Metadata antiga sem esse campo continua válida e equivale a uma lista vazia.

Não existe campo para JSON genérico. Nunca atravessam a fronteira:

- `tenancy_api_*`, tokens, senhas ou configuração fiscal;
- host, porta, username, password ou connection de database;
- configuração de database histórico;
- `tenant_links`;
- qualquer chave desconhecida.

## Upsert seguro e idempotente

Antes da transação, o writer valida em `information_schema`:

- tabelas e colunas necessárias;
- limite real do tenant ID;
- tipo JSON;
- índice unique do domain;
- FK `domains.tenant_id -> tenants.id`.

Também bloqueia antes da mutação:

- domain pertencente a outro tenant;
- tenant local preexistente com override de conexão ou database histórico.

O mesmo bloqueio é repetido dentro do `ON DUPLICATE KEY UPDATE`, protegendo a transação contra uma alteração concorrente entre preflight e escrita.

Tenant e domain são gravados numa única transação. Um registro novo recebe somente `tenancy_db_name` e as features presentes no snapshot. Num registro existente, os demais dados que já eram locais são mantidos, mas overrides de conexão impedem a operação. Em particular:

- `tenancy_db_name` e as features trazidas são atualizados;
- `created_at` é preservado e `updated_at` é atualizado;
- chaves exclusivamente locais e não bloqueadas são preservadas;
- o mesmo domain para o mesmo tenant é idempotente;
- conflito nunca reatribui automaticamente um domain.

Todos os valores variáveis entram no SQL como hexadecimal UTF-8 convertido pelo MySQL. Não há interpolação de input cru, shell ou senha no argv. O client MySQL aprovado entra pelo namespace do container exato e usa o option file efêmero `0600`.

## Domain local

Um lookup por domain válido, como `sagatec`, preserva esse label. Um lookup por tenant ID normaliza `_` para `-`, por exemplo `salt_sagatec -> salt-sagatec`. O value object aceita apenas DNS label lowercase de até 63 caracteres; `localhost` é reservado como domínio central local padrão.

A URL local e uma eventual escolha/alteração interativa do alias pertencem à UX da RDB-053/RDB-054.

## Evidências

O teste integrado existente restaura um database `salt_reprodb_restore_<uuid>` no `mysql-8`, registra tenant e domain únicos no `salt_central`, executa o upsert duas vezes, preserva uma chave local, verifica o JSON permitido e remove tenant/domain/database ao final:

```bash
cargo test --test restore_integration -- --ignored --nocapture
```

O resolver com a allowlist de features também foi revalidado contra `sagatec` e `polymer` reais do ambiente local:

```bash
cargo test --test salt_central_resolver_integration -- --ignored --nocapture
```

O spike da RDB-005 continua sendo a evidência de compatibilidade com o Laravel real: ele inicializa `stancl/tenancy` por subdomain e consulta um marker no database selecionado.
