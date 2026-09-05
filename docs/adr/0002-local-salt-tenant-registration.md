# ADR 0002 — Registro local mínimo de tenant do Salt

- Status: aceita
- Data: 5 de setembro de 2026
- Issue: RDB-005

## Contexto

Restaurar somente o database do tenant não basta para abrir o Salt. A aplicação usa `stancl/tenancy`, consulta `salt_central.tenants`, resolve subdomínios por `domains` e inicializa a conexão a partir dos atributos internos do tenant.

O campo JSON `tenants.data` pode conter ao mesmo tempo:

- o override estrutural `tenancy_db_name`;
- feature flags que afetam a reprodução;
- host, usuário e senha de banco;
- credenciais de integrações fiscais;
- outros campos desconhecidos no futuro.

Copiar esse JSON integralmente seria um vazamento desnecessário e poderia fazer o ambiente local chamar serviços externos com credenciais reais.

## Evidência no Salt

Foram examinados:

- as migrations de `tenants`, `domains` e `tenant_links`;
- `ErpSaga\Models\Tenant`;
- `stancl/tenancy` `DatabaseConfig`, `HasInternalKeys` e `HasDataColumn`;
- os middlewares de tenant das rotas web/API;
- `TenantCatalog::ensureBaseTenants()` do bootstrap de testes;
- usos de atributos `tenancy_*` na aplicação.

O bootstrap de testes do próprio Salt prova que um tenant cujo database tem o mesmo nome do ID precisa somente do registro em `tenants` e de um `domain`. O pacote usa `tenancy_db_name`, quando presente, antes da convenção `prefix + tenant ID + suffix`.

No `salt_central` local foram observados somente nomes de chaves, nunca seus valores:

```text
created_at
updated_at
tenancy_annotation_atm
tenancy_api_nf_grupo
tenancy_api_nf_host
tenancy_api_nf_senha
tenancy_api_nf_tp_amb
tenancy_api_nf_usuario
tenancy_app_color
tenancy_db_name
tenancy_enable_stock_label_control
```

Isso confirma que dados de apresentação, comportamento, conexão e secrets convivem na mesma coluna.

## Decisão

O reprodb cria ou atualiza um registro local mínimo depois que o restore do database do tenant termina com sucesso.

O core recebe um valor já sanitizado e incapaz de carregar JSON arbitrário:

```rust
struct LocalTenantRegistration {
    tenant_id: TenantId,
    local_domain: LocalDomain,
    target_database: DatabaseName,
    features: LocalTenantFeatures,
}

struct LocalTenantFeatures {
    app_color: Option<AppColor>,
    annotation_atm: Option<bool>,
    enable_stock_label_control: Option<bool>,
    enable_sped_contrib: Option<bool>,
    enable_beta: Option<bool>,
    has_cyclic_counting: Option<bool>,
    new_production: Option<bool>,
}
```

Não existe um campo `source_json`, `extra` ou `Map<String, Value>` nesse contrato. Adicionar uma nova chave exige mudança de código, teste e revisão de segurança.

### Conteúdo de `tenants`

O reprodb escreve:

- `id`: ID canônico resolvido no source central;
- `created_at`/`updated_at`: timestamps locais;
- `data.tenancy_db_name`: sempre o `DatabaseName` restaurado no target local;
- somente os atributos opcionais representados por `LocalTenantFeatures`.

O valor de `tenancy_db_name` não é copiado cegamente do JSON. O resolver usa o valor do source para localizar o dump; o registro local usa o nome efetivo validado do target.

Os seguintes grupos nunca são copiados:

- `tenancy_db_connection`, `tenancy_db_host`, `tenancy_db_port`, `tenancy_db_username`, `tenancy_db_password` e equivalentes;
- todo prefixo `tenancy_api_*`;
- campos de serviços externos, tokens, chaves, usuários ou senhas;
- `tenancy_salt_historical_table_name` e configuração de database histórico;
- `created_at` e `updated_at` que apareçam dentro do JSON;
- qualquer chave desconhecida.

Overrides de conexão no source são detectados pela RDB-032 e bloqueiam o fluxo até existir um source profile correto. Eles nunca são reproduzidos localmente.

### Domain local

O `domains.domain` local é um subdomínio local, não uma cópia de hostname/FQDN de produção.

Seleção:

1. quando a entrada do usuário resolveu por `domains.domain` e já é um label local válido, reutilizar esse alias;
2. quando a entrada foi um tenant ID, derivar uma sugestão explícita e visível, normalizando `_` para `-`;
3. validar lowercase, tamanho e charset de DNS label;
4. bloquear valores que coincidam com `tenancy.central_domains`;
5. se o domain já pertence a outro tenant, falhar sem reatribuição automática.

O resultado deve ser mostrado como URL local no final do `pull`.

### `tenant_links`

O MVP não cria linhas em `tenant_links`.

Esses vínculos acionam comportamento de replicação entre tenants e pressupõem a existência de outros databases. Copiá-los parcialmente seria semanticamente incorreto; copiar todos ampliaria o escopo e a quantidade de dados. Uma reprodução que dependa de relação pai/filho deve falhar com limitação documentada até existir uma feature própria.

### Atualização de registro preexistente

Tenant e domain são gravados numa transação do `salt_central` local.

- um tenant novo recebe somente o conjunto mínimo;
- num tenant existente, o reprodb atualiza `tenancy_db_name` e as chaves de feature que trouxe, preservando outras configurações exclusivamente locais;
- chaves bloqueadas preexistentes não são usadas pelo reprodb e geram diagnóstico no `doctor` se puderem redirecionar conexão;
- domain em conflito bloqueia toda a transação;
- falha de registro não apaga um database restaurado com sucesso; o retry reutiliza o cache e repete o upsert.

Toda mutação ocorre exclusivamente no `LocalTarget` verificado. O tipo não aceita `SourceProfile`.

### Construção do SQL

O reprodb não interpola input cru em SQL. Identificadores passam pelos value objects e valores são transportados como hex UTF-8 ou por outro mecanismo estruturalmente seguro do adapter. O JSON final é construído pela ferramenta a partir dos campos tipados; ele nunca é repassado do source.

## Fixture automatizada

O spike está em `spikes/local-tenant-registration`:

```bash
./spikes/local-tenant-registration/run.sh
```

Ele cria dois databases reservados, com as tabelas centrais mínimas e um marker no database do tenant. Em seguida, executa o Laravel real do repositório Salt, resolve o model por domain, inicializa `tenancy()` e consulta o marker pela conexão `tenant`.

O JSON da fixture possui exatamente:

```json
{
  "tenancy_db_name": "reprodb_spike_tenant_data",
  "tenancy_app_color": "#123456",
  "tenancy_enable_stock_label_control": true
}
```

Resultado:

```text
salt_tenancy_initialization=ok
domain_resolution=ok
database_override=ok
tenant_database_query=ok
full_source_json_copied=no
```

O harness recusa sobrescrever databases preexistentes e remove os dois databases ao sair.

## Alternativas rejeitadas

### Restaurar `salt_central` inteiro

Rejeitada por volume, acoplamento, risco de secrets e possibilidade de interferir em todos os tenants locais.

### Copiar a linha inteira de `tenants`

Rejeitada porque a coluna `data` mistura opções benignas e credenciais de integração.

### Criar apenas o database

Rejeitada porque middleware, rotas e helpers dependem de tenant/domain central.

### Usar somente pattern sem registro central

Rejeitada para o Salt porque não exercita o mesmo mecanismo de inicialização da aplicação e não resolve aliases reais.

### Copiar `tenant_links`

Rejeitada no MVP porque produziria relações quebradas ou exigiria restauração coordenada de múltiplos tenants.

## Consequências

- feature flags conhecidas podem acompanhar o dump sem trazer secrets;
- features novas ficam desabilitadas até serem classificadas e adicionadas à allowlist;
- reproduções que dependam de integrações externas ou hierarquia entre tenants continuam fora do MVP;
- schema drift de `salt_central` precisa ser detectado pelo `setup`/`doctor` antes do upsert;
- o registro é idempotente, mas conflitos de domain ou target são erros deliberados.
