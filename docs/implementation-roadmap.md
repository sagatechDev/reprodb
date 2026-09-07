# reprodb — Roadmap de implementação

> Documento de referência para transformar o plano arquitetural do reprodb em milestones e issues executáveis.
>
> Estado da análise: 5 de setembro de 2026.

## 1. Objetivo

Construir uma CLI local em Rust que permita a um desenvolvedor reproduzir um tenant do Salt com um fluxo semelhante a:

```bash
reprodb setup
reprodb profile add salt-local
reprodb doctor
reprodb pull sagatec
```

O fluxo completo deve:

1. resolver o tenant a partir do cadastro do `salt_central`;
2. conectar ao source MySQL por host/porta, como o DBeaver faz;
3. executar uma versão conhecida do `mysqldump`;
4. comprimir o stdout em Zstandard sem criar SQL cru em disco;
5. publicar um artefato atômico no cache local;
6. recriar o database no MySQL Docker escolhido durante o setup;
7. restaurar o dump;
8. garantir no `salt_central` local o registro mínimo necessário para inicializar o tenant;
9. funcionar em macOS e Linux.

Produção continua sendo apenas um source profile. Ela não terá outro dump engine; apenas políticas adicionais e uma matriz de compatibilidade validada.

## 2. Evidências do ambiente real

Esta seção registra o que foi observado no ambiente local usado para desenhar o backlog. Esses valores são evidência para o desenvolvimento, não defaults eternos do produto.

| Item | Observação em 2026-09-05 | Consequência para o reprodb |
|---|---|---|
| Docker context | `desktop-linux`, endpoint Unix local | `setup` deve aceitar sockets Unix do Docker Desktop e do Linux, mas rejeitar contexts SSH/TCP remotos no MVP |
| Container local | `mysql-8`, imagem configurada como `mysql:8` | Não confiar em tag major mutável para determinar compatibilidade |
| Versão real do container | MySQL Community Server `8.4.4`, Linux `arm64` | A tag `mysql:8` já não significa necessariamente MySQL 8.0 |
| Porta do target local | `3306` publicada em `0.0.0.0` | `setup` deve mostrar o bind e alertar quando MySQL estiver exposto além de loopback |
| Clients no host macOS | `mysql` e `mysqldump` `8.0.45`, Homebrew, `arm64` | O host pode ter um client diferente do target; a CLI precisa controlar o client que usa |
| Salt README | Declara MySQL 8.0 / MySQL >= 8.0 | A série exata de produção ainda precisa ser medida, não presumida |
| Salt CI | Usa `mysql:8` | O CI também está sujeito à mudança silenciosa da tag major |
| Databases locais | `salt_central` e vários `salt_*` | O resolver do Salt deve consultar o database central, não apenas aplicar um pattern cego |
| `salt_central` local | 424 tabelas e aproximadamente 19 MiB | Não restaurar o database central inteiro só para registrar um tenant |
| Tabelas observadas | Todas as tabelas `salt_*` observadas são InnoDB | `--single-transaction` é adequado para o estado local, mas produção ainda precisa de preflight |
| FKs entre schemas | Nenhuma encontrada no ambiente examinado | Tenant e central podem ser restaurados separadamente, mas a aplicação ainda depende do registro de tenancy |
| Objetos observados | Nenhuma view, trigger, routine ou event nos schemas locais examinados | O dump policy deve detectar esses objetos em produção e tornar sua inclusão explícita |
| Collations | Mistura de `utf8mb4_0900_ai_ci` e `utf8mb4_unicode_ci` | Metadata precisa preservar charset e collation do database original |
| Tamanho local | Tenants de aproximadamente 24 MiB até 2,56 GiB | Testes precisam cobrir streaming e um cenário de tamanho significativo |
| GTID local | `OFF` | Ainda usar `--set-gtid-purged=OFF` para impedir comportamento diferente em outro source |
| Tenant registry | `salt_central.tenants` usa `id` + JSON `data`; domains ficam em `domains` | O alias digitado pode ser resolvido por tenant ID ou domain |
| Nome do database | `tenancy_db_name` quando presente; caso contrário, tenant ID | Essa regra deve ser reproduzida pelo `SaltCentralTenantResolver` |
| Prefix/suffix do Salt | Ambos vazios em `config/tenancy.php` | Hoje o database normalmente tem o mesmo nome do tenant ID |
| Snapshot existente no Salt | Já usa `mysqldump`, `mysql`, `--single-transaction`, `--no-tablespaces` e `--set-gtid-purged=OFF` | Há precedente funcional, mas o reprodb não deve repetir senha em argumento nem SQL cru persistente |

Exemplos reais de resolução encontrados:

```text
domain sagatec -> tenant id salt_sagatec -> database salt_sagatec
domain sigga   -> tenant id salt_sigga   -> database salt_sigga
domain watt    -> tenant id salt_watt_construtora -> database salt_watt_construtora
```

O registro central pode conter campos sensíveis dentro do JSON `data`. O reprodb não deve imprimir, logar ou copiar esse JSON inteiro por padrão.

## 3. Decisões arquiteturais adotadas

### 3.1 Dois fluxos de configuração

`reprodb setup` configura o ambiente local e o target Docker.

`reprodb profile add NAME` configura um source MySQL.

As duas conexões possuem credenciais diferentes e chaves diferentes no credential store.

### 3.2 Autenticação interativa

`profile add` pergunta, no mínimo:

```text
profile name
host
port
username
password
TLS mode
```

A série MySQL e o vendor são detectados por uma conexão real; o usuário não precisa conhecê-los. O resolver inicial é o `SaltCentralTenantResolver`, fixado pela convenção observada no Salt, e poderá virar uma escolha quando houver um segundo caso real.

`setup` pergunta, no mínimo:

```text
Docker target
target username
target password
central database local
database pattern/policy local
```

Durante a senha, a CLI mostra um `*` por caractere digitado ou colado. O conteúdo nunca é exibido e é persistido somente no keyring do sistema operacional. O TOML guarda apenas `credential_key`.

### 3.3 Client MySQL controlado por Docker

O runtime inicial dos clients será um container efêmero baseado em imagem oficial MySQL fixada por tag exata e digest.

```text
reprodb
  -> docker run --rm <mysql-client-image> mysqldump
  -> stdout
  -> zstd
  -> cache
```

A CLI não fará busca aberta na internet nem instalará pacotes no sistema. Ela consultará um catálogo de versões testadas presente no código. Quando a imagem estiver ausente, usará `docker pull` para obter exatamente a referência aprovada.

O client do host poderá existir como adapter futuro ou fallback explícito, mas não será escolhido silenciosamente pelo MVP.

### 3.4 Conexão igual à do DBeaver

O source profile continua sendo descrito por host, porta, usuário e TLS. A diferença é que o processo MySQL roda em um container efêmero.

O spike deve validar:

- host externo acessível pela rede/VPN;
- source publicado em `127.0.0.1` no macOS;
- source publicado em `127.0.0.1` no Linux;
- `host.docker.internal` no Docker Desktop;
- `host-gateway` no Linux;
- IPv4, DNS e erro de TLS.

Se a VPN corporativa impedir tráfego originado pelo Docker, isso bloqueia o adapter Docker para produção e exige uma ADR antes de implementar download de binários no host.

### 3.5 Configuração temporária do client

A senha não será passada como `--password=...`.

Cada execução cria um option file temporário com permissões restritas:

```ini
[client]
host=...
port=3306
user=...
password="..."
protocol=TCP
```

O arquivo é montado read-only no client container, referenciado por `--defaults-file`, mantido até o processo terminar e removido por um guard mesmo em erro/cancelamento.

### 3.6 Target Docker selecionado no setup

`setup` obtém os IDs completos com `docker container ls -a --no-trunc --quiet`, consulta metadata estruturada com `docker container inspect` e confirma o escolhido com uma conexão MySQL real.

Não basta procurar `mysql` no nome da imagem. A descoberta considera:

- imagem/configuração;
- porta 3306 exposta ou publicada;
- estado e healthcheck;
- versão retornada pelo servidor;
- vendor retornado por `@@version_comment`;
- redes Docker;
- container ID exato.

O setup também oferece criar um container dedicado ao reprodb. Essa é a opção recomendada. Um container criado pela ferramenta recebe a label:

```text
com.sagatech.reprodb.target=true
```

Ao selecionar um container existente, o reprodb armazena nome e ID. Se o nome apontar futuramente para outro ID, operações destrutivas são bloqueadas até um novo `setup`.

### 3.7 Resolver real do Salt

O MVP terá um `SaltCentralTenantResolver` além do resolver por pattern.

Resolução conceitual:

```text
entrada do usuário
  -> procurar por tenants.id exato
  -> ou procurar por domains.domain exato
  -> obter tenant id
  -> usar data.tenancy_db_name quando presente
  -> senão usar tenant id
  -> validar DatabaseName novamente
```

O resolver não deve seguir automaticamente `tenancy_db_host`, `tenancy_db_username` ou `tenancy_db_password` encontrados no JSON central. O source profile é a autoridade da conexão. Uma divergência deve produzir erro explícito e pedir outro profile.

### 3.8 Contexto central local mínimo

Restaurar apenas `salt_<tenant>` pode não ser suficiente para o Salt inicializar a tenancy. O fluxo de restore deve garantir um registro mínimo no `salt_central` local:

- tenant ID;
- `tenancy_db_name` apontando para o database restaurado;
- domain selecionado ou alias local;
- timestamps necessários pelo schema.

O JSON `data` completo não será copiado no MVP, pois pode conter credenciais de integrações. A issue específica deverá validar qual conjunto mínimo permite inicializar o Salt sem transportar secrets centrais.

### 3.9 Pipeline assíncrono

O pipeline utilizará Tokio e uma implementação Zstd assíncrona:

```text
mysqldump ChildStdout
  -> contador/hash/progresso
  -> ZstdEncoder
  -> dump.sql.zst.part
```

Não haverá buffer proporcional ao dump. O crate síncrono `zstd` não será conectado diretamente a um `AsyncRead`; usar `async-compression` ou uma thread dedicada será decidido e provado no spike.

### 3.10 Artefato atômico de cache

A unidade de publicação será um diretório:

```text
cache/profiles/<profile>/<tenant-id>/<dump-id>.part/
  dump.sql.zst
  metadata.json
```

Após sucesso completo:

```text
rename <dump-id>.part -> <dump-id>
```

Fingerprint e database resolvido ficam na metadata e são revalidados em todo hit. Mantê-los fora do path permite que uma configuração alterada ainda encontre e limpe artefatos antigos com segurança. `current.json` não será fonte de verdade. O cache selecionará o artefato completo mais recente e poderá reconstruir índices derivados.

### 3.11 Restore somente de artefato gerenciado

O MVP não aceitará um SQL/Zstd arbitrário por path como fluxo normal.

```bash
reprodb restore sagatec --dump-id <id>
```

Importação de arquivo externo ficará fora do MVP ou exigirá um comando separado, aviso explícito e validações adicionais.

## 4. Modelo de configuração proposto

Exemplo sem secrets:

```toml
schema_version = 1
active_profile = "salt-local"

[client_runtime]
type = "docker"
docker_context = "desktop-linux"

[local_target]
docker_context = "desktop-linux"
container_name = "mysql-8"
container_id = "62a9bfe47f1c..."
username = "root"
credential_key = "target:<uuid>"
central_database = "salt_central"

[profiles.salt-local]
host = "127.0.0.1"
port = 3306
username = "root"
credential_key = "source:<uuid>"
mysql_family = "mysql"
mysql_series = "8.4"
production = false
tls_mode = "required"

[profiles.salt-local.client]
image = "mysql:<tested-tag>@sha256:<tested-digest>"

[profiles.salt-local.tenant_resolver]
type = "salt-central"
central_database = "salt_central"
allow_domain_lookup = true
```

O host salvo deve representar o endpoint informado pelo usuário. Traduções necessárias para o namespace de rede Docker pertencem ao `MysqlClientRuntime`, não ao domínio do profile.

## 5. Comandos do MVP

```bash
reprodb setup

reprodb profile add NAME
reprodb profile list
reprodb profile use NAME
reprodb profile remove NAME

reprodb doctor

reprodb dump TENANT
reprodb restore TENANT --dump-id ID
reprodb pull TENANT
reprodb pull TENANT --fresh

reprodb cache list
reprodb cache clean
reprodb cache purge TENANT
```

## 6. Labels sugeridas

Ao criar as issues no tracker, usar combinações destas labels:

```text
priority:p0
priority:p1
priority:p2

type:spike
type:feature
type:test
type:hardening
type:docs

area:cli
area:config
area:credentials
area:mysql
area:docker
area:tenant
area:cache
area:restore
area:security
area:cross-platform
```

Os IDs `RDB-NNN` abaixo são estáveis dentro deste documento e podem ser usados no título da issue.

## 7. Roadmap por milestones e issues

| Milestone | Issues | Resultado esperado |
|---|---|---|
| 0 — Riscos técnicos | RDB-001 a RDB-005 | Pipeline e decisões críticas provados sem produção |
| 1 — Bootstrap | RDB-010 a RDB-013 | Projeto Rust, CLI base, erros e infraestrutura de testes |
| 2 — Configuração/setup | RDB-020 a RDB-026 | Source profiles e target Docker configuráveis com credenciais seguras |
| 3 — Tenant Salt | RDB-030 a RDB-033 | Alias/ID resolvido pelo `salt_central` |
| 4 — Dump/cache | RDB-041 a RDB-046 | Artefato Zstd atômico, íntegro e reutilizável |
| 5 — Restore/pull | RDB-050 a RDB-057 | Tenant restaurado e inicializável pelo Salt com target explícito, cache e ETA |
| 6 — Resiliência | RDB-060 a RDB-064 | Falhas, cancelamento, performance e macOS/Linux cobertos |
| 7 — Produção | RDB-070 a RDB-074 | TLS, compatibilidade e piloto aprovados |

### Milestone 0 — Eliminar riscos técnicos

Critério da milestone: provar conectividade, autenticação, dump, compressão e restore em macOS e Linux antes de construir a aplicação completa.

#### RDB-001 — Inventariar o baseline MySQL local e de CI do Salt

**Labels:** `priority:p0`, `type:spike`, `area:mysql`, `area:security`

**Status:** concluída — [relatório do baseline MySQL](research/mysql-baseline-2026-09-05.md).

**Objetivo:** substituir a suposição genérica “Salt usa MySQL 8” por uma matriz concreta para desenvolvimento e testes, sem acessar produção.

**Escopo:**

- registrar `VERSION()` e `@@version_comment` dos ambientes locais/testáveis;
- registrar série, patch e vendor;
- registrar GTID, charset/collation e TLS;
- contar engines, views, triggers, routines e events;
- documentar o que README, CI e containers realmente usam;
- não consultar nem registrar dados de negócio.

**Aceite:** relatório técnico versionado, sem host, usuário ou segredo sensível, que permita escolher client e target locais. A inspeção do source de produção continua reservada à Milestone 7.

#### RDB-002 — Spike do client MySQL em Docker no macOS e Linux

**Labels:** `priority:p0`, `type:spike`, `area:mysql`, `area:docker`, `area:cross-platform`

**Status:** em andamento — [resultado do spike](research/docker-client-spike.md); caminho macOS local validado, Linux/VPN pendentes.

**Objetivo:** provar que um client container alcança os mesmos hosts acessíveis pelo DBeaver.

**Escopo:**

- imagem oficial fixada por digest;
- conexão a host externo/VPN;
- conexão a uma porta publicada no host;
- suporte a Docker Desktop e Docker Engine Linux;
- option file read-only sem senha em argv;
- stdout e stderr separados;
- cancelamento e propagação de exit code.

**Aceite:** `SELECT VERSION()` e um dump pequeno funcionam nos dois sistemas. Se VPN falhar, abrir ADR de runtime host antes de continuar.

**Depende de:** RDB-001 apenas para escolher a imagem final; pode começar com o MySQL local.

#### RDB-003 — Spike end-to-end dump → Zstd → restore

**Labels:** `priority:p0`, `type:spike`, `area:mysql`, `area:docker`, `area:restore`

**Status:** concluída no macOS — [resultado do spike](research/streaming-pipeline-spike.md); a repetição em Linux continua nas issues cross-platform RDB-002 e RDB-061.

**Objetivo:** provar o núcleo mais arriscado do produto.

**Escopo:**

- crate Rust descartável, independente da estrutura final do produto;
- source MySQL efêmero;
- fixture com UTF-8, NULL, bigint, datetime, decimal, blob, FK e volume significativo;
- `mysqldump` por stdout;
- compressão Zstd streaming;
- decode streaming;
- restore em outro MySQL;
- comparação dos dados e schema;
- Ctrl+C e limpeza do parcial.

**Aceite:** teste automatizado reproduzível, RAM aproximadamente constante e nenhum `.sql` cru persistente.

**Depende de:** RDB-002.

#### RDB-004 — ADR do transporte de credenciais

**Labels:** `priority:p0`, `type:spike`, `area:credentials`, `area:security`

**Status:** concluída — [ADR 0001: transporte de credenciais](adr/0001-credential-transport.md), com serializer e autenticação real validados no spike.

**Objetivo:** fixar uma estratégia única para source e target.

**Decisão esperada:** keyring → `SecretString` → option file temporário restrito → mount read-only → `--defaults-file`.

**Aceite:** testes com espaços, aspas, `#`, `;`, barra invertida e newline; segredo ausente de config, Debug, logs, argv e mensagens de erro.

#### RDB-005 — ADR do contexto central necessário para reproduzir um tenant

**Labels:** `priority:p0`, `type:spike`, `area:tenant`, `area:security`

**Status:** concluída — [ADR 0002: registro local mínimo do tenant Salt](adr/0002-local-salt-tenant-registration.md), validada inicializando o Laravel real com uma fixture sem JSON central completo.

**Objetivo:** determinar quais campos mínimos de `salt_central.tenants` e `domains` devem existir localmente.

**Escopo:**

- inicializar o Salt com tenant local restaurado;
- identificar campos realmente necessários;
- excluir credenciais de integração e outros secrets;
- definir comportamento para `tenant_links`;
- decidir alias/domain local.

**Aceite:** contrato de `LocalTenantRegistration` documentado e fixture automatizada que inicializa tenancy sem copiar o JSON central completo.

### Milestone 1 — Bootstrap da aplicação

Critério da milestone: binário compilável, command tree estável, testes unitários em macOS/Linux e composição fora dos handlers do clap.

#### RDB-010 — Criar o projeto Rust e CI

**Labels:** `priority:p0`, `type:feature`, `area:cli`, `area:cross-platform`

**Status:** implementada — crate principal, MSRV 1.88 e CI macOS/Linux adicionados; a primeira execução remota do workflow ainda depende de push. O MSRV foi elevado de 1.85 porque `keyring` 4, adotado na RDB-022, requer Rust 1.88.

**Escopo:**

- `Cargo.toml` e `Cargo.lock`;
- `src/lib.rs` como API testável;
- `src/main.rs` como composition root;
- `rustfmt`, Clippy e testes;
- CI unitário em Linux e macOS;
- MSRV documentada.

**Aceite:** `cargo fmt --check`, `cargo clippy -- -D warnings` e `cargo test` passam nos dois sistemas.

**Depende de:** RDB-003, para evitar consolidar uma arquitetura antes do spike.

#### RDB-011 — Implementar a árvore inicial do clap com TDD

**Labels:** `priority:p0`, `type:feature`, `area:cli`

**Status:** implementada — árvore tipada e testes de processo adicionados; handlers retornam erro explícito até suas respectivas issues.

**Escopo:** `setup`, `profile`, `doctor`, `dump`, `restore`, `pull` e `cache` com argumentos previstos.

**Aceite:** testes de `--help`, `--version`, comando inválido, tenant ausente e subcommand ausente usando `assert_cmd`.

#### RDB-012 — Definir erros, exit codes e logging seguro

**Labels:** `priority:p1`, `type:feature`, `area:cli`, `area:security`

**Status:** implementada — categorias e [exit codes](exit-codes.md) estáveis, tracing opt-in e testes contra vazamento de argumentos adicionados.

**Escopo:** erros tipados por categoria, mapeamento estável para exit code, mensagens acionáveis e `tracing` opt-in por `RUST_LOG`.

**Aceite:** nenhum erro inclui secret, SQL do dump ou argv sensível; Ctrl+C é reservado para exit code 130.

#### RDB-013 — Definir infraestrutura de testes e fixtures

**Labels:** `priority:p1`, `type:test`, `area:mysql`, `area:docker`

**Escopo:** builders, clock fake, credential store em memória, diretórios temporários, client runtime fake e fixtures SQL.

**Aceite:** testes de aplicação não dependem do keyring nem do Docker; integrações reais são marcadas e executadas separadamente.

### Milestone 2 — Configuração, credenciais e setup

Critério da milestone: target Docker e múltiplos source profiles podem ser configurados sem secrets no TOML.

#### RDB-020 — Implementar paths e config TOML transacional

**Labels:** `priority:p0`, `type:feature`, `area:config`

**Status:** implementada — toda a aplicação usa a raiz previsível `~/.reprodb` (`REPRODB_HOME` permite isolamento), schema estrito versionado, escrita atômica sob lock e permissões privadas possuem cobertura automatizada. Configuração criada pelo layout nativo anterior é lida como fallback e migra na próxima escrita, sem apagar o original.

**Escopo:** raiz local única, `schema_version`, parse estrito, escrita em temporário + rename, permissões e lock de escrita.

**Aceite:** primeira execução, TOML inválido, concorrência, falha de escrita e path abstraído são testados.

#### RDB-021 — Implementar value objects do domínio

**Labels:** `priority:p0`, `type:feature`, `area:tenant`, `area:security`

**Status:** implementada — value objects não permitem input cru chegar a database, container ou credential key e possuem testes de limites/injection.

**Escopo:** `ProfileName`, `TenantLookup`, `TenantId`, `DomainAlias`, `DatabaseName`, `ContainerName`, `ContainerId`, `CredentialKey` e `MysqlVersion`.

**Aceite:** limites de tamanho/charset, databases administrativos, nomes maiores que o limite MySQL e inputs de injection são rejeitados.

#### RDB-022 — Implementar CredentialStore e option files temporários

**Labels:** `priority:p0`, `type:feature`, `area:credentials`, `area:security`

**Status:** implementação concluída e integração com o Keychain do macOS validada; a execução real contra Secret Service no Linux permanece pendente antes de encerrar o aceite cross-platform.

**Escopo:** `OsCredentialStore`, `MemoryCredentialStore`, `SecretString`, arquivo temporário restrito e rollback de cadastro incompleto.

**Aceite:** cobre o contrato da RDB-004 e funciona em keychain do macOS e backend suportado no Linux.

#### RDB-023 — Implementar catálogo e runtime de client Docker

**Labels:** `priority:p0`, `type:feature`, `area:mysql`, `area:docker`

**Status:** implementação concluída e validada no Docker Desktop/macOS contra o `mysql-8` local; compilação Linux validada, com execução real em Docker Engine Linux/VPN ainda acompanhada pela RDB-002.

**Escopo:** catálogo versionado, imagem fixada por digest, `image inspect`, `pull`, mount de option file, rede Mac/Linux e execução sem shell interpolation.

**Aceite:** client incompatível ou digest divergente é recusado; ausência de rede produz erro acionável.

**Depende de:** RDB-002, RDB-004 e RDB-022.

#### RDB-024 — Implementar comandos de profile

**Labels:** `priority:p0`, `type:feature`, `area:config`, `area:credentials`

**Status:** implementada no macOS — `add`, `list`, `use` e `remove` operam sobre configuração e credential store reais. O cadastro usa senha mascarada por asteriscos, aceita texto colado, aplica TLS explícito, prepara o client Docker aprovado e faz conexão real com detecção de versão antes da persistência. Nenhum profile é salvo quando a verificação falha. A integração com `mysql-8` passou usando `REQUIRED`; a execução do fluxo completo com Secret Service permanece pendente no Linux.

**Escopo:** `add`, `list`, `use`, `remove`; prompts interativos; senha mascarada sem revelar conteúdo; teste da conexão antes do commit.

**Aceite:** profile duplicado, remoção do ativo, keyring indisponível, conexão inválida e cleanup da credencial são testados.

**Depende de:** RDB-020, RDB-022 e RDB-023.

#### RDB-025 — Implementar `reprodb setup`

**Labels:** `priority:p0`, `type:feature`, `area:docker`, `area:security`

**Status:** em andamento — descoberta e seleção de containers existentes, rejeição de context remoto, alertas de bind, inicialização confirmada de container parado, senha mascarada, verificação real por ID e persistência transacional estão implementadas e validadas contra `mysql-8` no macOS. Criação de target dedicado e execução real em Linux permanecem pendentes.

**Escopo:**

- validar Docker e context local;
- listar containers candidatos em JSON;
- mostrar imagem, estado, versão, porta e redes;
- mostrar o endereço de bind da porta e alertar para `0.0.0.0`/`::`;
- permitir escolher existente;
- oferecer criar container dedicado;
- pedir credencial do target;
- guardar container name + ID;
- validar conexão real.

**Aceite:** funciona com o container local atual, com container parado, sem candidatos e com um container customizado que realmente executa MySQL.

**Depende de:** RDB-020, RDB-022 e RDB-023.

#### RDB-026 — Implementar `reprodb doctor`

**Labels:** `priority:p0`, `type:feature`, `area:config`, `area:mysql`, `area:docker`

**Status:** implementada e validada no macOS contra o `mysql-8` real. O comando agrega checks de configuração, credenciais, filesystem, Docker, identidade do target, client aprovado, conexões, versões e TLS sem baixar imagem, iniciar o target ou persistir configuração. Os probes usam containers efêmeros `--rm` do client. Compilação Linux está validada; execução real em Docker Engine Linux permanece na RDB-002/RDB-061.

**Escopo:** config, profiles, keyring, Docker context, client image, target ID, source/target connection, versões, vendor, TLS, espaço livre e matriz de compatibilidade.

**Aceite:** cada check falha isoladamente com ação recomendada; doctor não altera bancos.

**Depende de:** RDB-020 a RDB-025.

### Milestone 3 — Resolução de tenant Salt

Critério da milestone: um alias como `sagatec` resolve de modo seguro para tenant e database reais.

#### RDB-030 — Implementar PatternTenantResolver

**Labels:** `priority:p1`, `type:feature`, `area:tenant`

**Status:** implementada — pattern compilado exige exatamente um `{tenant}`, aceita somente literais seguros para database e sempre reconstrói `TenantId`/`DatabaseName` validados. Configuração inválida é recusada antes da persistência.

**Escopo:** pattern com exatamente um `{tenant}`, validação no carregamento e validação final de `DatabaseName`.

**Aceite:** casos válidos, traversal, SQL injection, pattern inválido e databases administrativos são cobertos.

#### RDB-031 — Implementar SaltCentralTenantResolver

**Labels:** `priority:p0`, `type:feature`, `area:tenant`, `area:mysql`

**Status:** implementada e validada contra o `salt_central` local — lookup binário por tenant ID/domain, fallback/override de `tenancy_db_name`, saída mínima em hex, ambiguidade e metadata inválida possuem cobertura. Os aliases reais `sagatec` e `polymer` resolveram para `salt_sagatec` e `salt_polymer` sem transportar o JSON central.

**Escopo:** lookup por `tenants.id` ou `domains.domain`, resolução de `tenancy_db_name`, output estruturado e validação final.

**Aceite:** cobre tenants locais observados, alias inexistente, ambiguidade, JSON inválido e database bloqueado; não expõe o JSON `data` em logs.

**Depende de:** RDB-021, RDB-023 e RDB-024.

#### RDB-032 — Detectar overrides de conexão por tenant

**Labels:** `priority:p1`, `type:hardening`, `area:tenant`, `area:security`

**Status:** implementada — a query retorna apenas um booleano calculado no source. Host, porta e usuário são comparados no MySQL com o profile; connection/password presentes bloqueiam. Nenhum valor de override retorna ao processo.

**Escopo:** detectar `tenancy_db_host`, `port`, `username` ou `connection` diferentes do profile sem retornar passwords.

**Aceite:** resolver bloqueia a operação e informa que o tenant requer outro source profile; nunca segue configuração central silenciosamente.

#### RDB-033 — Criar fixtures do `salt_central`

**Labels:** `priority:p1`, `type:test`, `area:tenant`

**Status:** implementada — fixture SQL mínima e inteiramente sintética cobre ID, domains reais de desenvolvimento, override de database, colisão, database bloqueado, connection overrides, secrets fictícios e tenant link. O teste cria/remove um database UUID no `mysql-8` sem escrever no `salt_central` existente.

**Escopo:** tenants por ID, domains, override de database, tenant links e JSON contendo chaves sensíveis fictícias.

**Aceite:** testes provam resolução e ausência de secrets em snapshot/output.

### Milestone 4 — Dump, compressão e cache

Critério da milestone: `reprodb dump TENANT` gera um artefato `.sql.zst` atômico, íntegro e restaurável.

#### RDB-041 — Implementar preflight e DumpPolicy MySQL 8

**Labels:** `priority:p0`, `type:feature`, `area:mysql`, `area:security`

**Status:** implementada — preflight somente leitura retorna apenas agregados, valida charset/collation/GTID e inventaria engines/objetos/definers. A policy v1 exige MySQL 8.4 com client da mesma série, aceita somente InnoDB, bloqueia routines/events e produz argumentos ordenados sem shell. Views/triggers com definer e risco de DDL concorrente viram avisos estruturados. A visibilidade integral de metadata pela credencial continua como gate explícito para produção.

**Flags iniciais:**

```text
--single-transaction
--quick
--no-tablespaces
--hex-blob
--set-gtid-purged=OFF
--triggers
--skip-lock-tables
```

**Escopo adicional:** versão/vendor, engines não InnoDB, DDL concorrente documentado, routines/events, definers e charset/collation do database.

**Aceite:** argumentos são testados como lista ordenada; nenhuma shell string; policy incompatível bloqueia antes do dump.

Detalhes e limites: [`docs/dump-policy.md`](dump-policy.md).

#### RDB-042 — Implementar compressão Zstd streaming e progresso

**Labels:** `priority:p0`, `type:feature`, `area:cache`

**Status:** implementada — blocos fixos de 64 KiB passam por canal limitado para um encoder em tarefa bloqueante; o resultado contém bytes, tempo, throughput/ratio e SHA-256 do SQL e do Zstd. Roundtrip, input sintético de 32 MiB, escrita parcial, falha de input e comparação reproduzível dos níveis 1/3 estão testados. Nível 1 é o default provisório e será reavaliado com SQL real antes de produção.

**Escopo:** `AsyncRead` → contador/hash → encoder → arquivo; nível inicial medido; bytes, tempo e throughput sem percentual falso.

**Aceite:** roundtrip, dump grande e falha de encoder são testados; memória não cresce proporcionalmente ao input.

Detalhes e medição inicial: [`docs/compression.md`](compression.md).

#### RDB-043 — Implementar artefato e metadata atômicos

**Labels:** `priority:p0`, `type:feature`, `area:cache`, `area:security`

**Status:** implementada — staging UUID em diretório `.part`, arquivos privados, `fsync`, metadata tipada e rename atômico do diretório completo. A publicação exige o resultado tipado da compressão, confere bytes/hashes e o tamanho em disco sem uma segunda leitura completa; falhas normais removem o staging e crash simulado deixa somente `.part` invisível para a listagem.

**Metadata:** tenant lookup, tenant ID, database, profile, source fingerprint, source/client version, charset/collation, policy version, timestamps, bytes, checksum e formato.

**Aceite:** somente diretórios completos são visíveis como cache; crash em cada etapa deixa no máximo `.part` recuperável.

Formato e ordem de publicação: [`docs/dump-artifacts.md`](dump-artifacts.md).

#### RDB-044 — Implementar validade, TTL e fingerprint do cache

**Labels:** `priority:p0`, `type:feature`, `area:cache`

**Status:** implementada — consulta os artefatos completos do mais novo para o mais antigo, aplica TTL de duas horas desde `completed_at`, rejeita relógio futuro, identidade/source/policy alterados e valida tamanho mais SHA-256 antes do hit. `--fresh` desvia antes de qualquer acesso ao filesystem; um artefato novo inválido permite fallback para outro ainda válido.

**Escopo:** TTL desde `completed_at`, default de duas horas, config fingerprint, checksum, arquivo ausente/corrompido e relógio futuro.

**Aceite:** hit, miss, expired, profile alterado, policy alterada e `--fresh` são testados.

Regras e custos de validação: [`docs/cache-validity.md`](cache-validity.md).

#### RDB-045 — Implementar locks e cleanup oportunista

**Labels:** `priority:p0`, `type:feature`, `area:cache`

**Status:** implementada — locks advisory possuem namespaces separados para source/target e chaves canônicas por recurso/database. Stagings mantêm lock exclusivo; cache hits carregam lease compartilhada; cleanup exige exclusividade, isola por rename e recupera deleções interrompidas. Concorrência real entre processos e liberação após término forçado são testadas.

**Escopo:** advisory lock por source+database, lock separado por target+database, cleanup de expirados e partials órfãos sem remover operação ativa.

**Aceite:** concorrência entre processos, crash e liberação de lock são testados no SO.

Contrato e limites: [`docs/cache-locks-cleanup.md`](cache-locks-cleanup.md).

#### RDB-046 — Entregar `reprodb dump`

**Labels:** `priority:p0`, `type:feature`, `area:cli`, `area:mysql`, `area:cache`

**Status:** implementada — o comando resolve o tenant, adquire o lock do source, executa o preflight, transmite `mysqldump` para Zstandard e publica metadata/checksums atomicamente. A UX informa progresso por bytes, throughput e tempo, além do ID/caminho final. Secrets não entram no argv ou nos diagnósticos e profiles de produção permanecem bloqueados até a RDB-071.

**Aceite:** resolve tenant, faz preflight, gera cache e apresenta caminho/ID/metadata sem executar restore.

Contrato operacional e roteiro de falhas: [`docs/dump-command.md`](dump-command.md).

### Milestone 5 — Restore e pull

Critério da milestone: um comando restaura o tenant no target selecionado e deixa o Salt capaz de inicializá-lo.

#### RDB-050 — Implementar barreira de segurança do LocalTarget

**Labels:** `priority:p0`, `type:hardening`, `area:docker`, `area:restore`, `area:security`

**Status:** implementada — `LocalTargetGate` só produz um `AuthorizedLocalTarget` após credencial, context Unix local, identidade exata ID+nome, estado do container, trust/label, conexão real e compatibilidade vendor/versão. O database central e nomes fora do prefixo configurado (`salt_` por padrão) não podem ser autorizados. Nenhum comando destrutivo foi adicionado nesta issue.

**Escopo:** context local, container ID, vendor/version, credencial, allowlist do database e container dedicado/confirmado.

**Aceite:** context remoto, ID trocado, database administrativo, target incompatível e ausência de setup bloqueiam antes de qualquer `DROP`.

Contrato e invariantes: [`docs/local-target-safety.md`](local-target-safety.md).

#### RDB-051 — Implementar RestoreEngine streaming

**Labels:** `priority:p0`, `type:feature`, `area:restore`, `area:mysql`

**Status:** implementada — artefato e target são capabilities validadas, o lock é adquirido antes do `DROP/CREATE`, charset/collation são preservados e o Zstd é enviado ao `mysql` com memória constante. Estado `incomplete` é persistido antes da destruição e só muda para `ready` após exit code e integridade finais. Validado no macOS contra o container real `mysql-8`.

**Ordem obrigatória:** validar metadata → verificar checksum/Zstd → validar target → adquirir lock → drop/create com charset/collation original → importar → validar exit code.

**Aceite:** dump corrompido não destrói o database existente; falha no import marca estado incompleto e permite retry sem novo dump.

Contrato e evidências: [`docs/restore-engine.md`](restore-engine.md).

#### RDB-052 — Implementar registro central local mínimo

**Labels:** `priority:p0`, `type:feature`, `area:tenant`, `area:restore`, `area:security`

**Status:** implementada — o snapshot fechado de features permitidas acompanha a metadata do dump; o serviço exige target autorizado e capability de restore concluído com o mesmo tenant/database; o writer valida o schema central, bloqueia domain conflitante e overrides de conexão e faz upsert transacional idempotente sem substituir os demais dados que já eram locais. Validado no `salt_central` real do container `mysql-8` sem deixar fixtures.

**Escopo:** implementar o contrato da RDB-005 e fazer upsert seguro no `salt_central` local após o restore tenant.

**Aceite:** Salt inicializa o tenant pelo domain/ID local; nenhum secret central do source é copiado.

Contrato e evidências: [`docs/local-tenant-registration.md`](local-tenant-registration.md).

#### RDB-053 — Entregar `reprodb restore`

**Labels:** `priority:p0`, `type:feature`, `area:cli`, `area:restore`

**Status:** implementada — o comando localiza um UUID único no cache sem consultar o source, valida tenant/metadata/Zstd/checksums, atesta e autoriza o target, mostra o plano destrutivo local, executa restore streaming e registra o tenant no `salt_central`. Retry pelo mesmo ID é idempotente e foi validado no `mysql-8` real.

**Aceite:** aceita apenas `dump-id` gerenciado, mostra source/target, restaura e informa claramente o estado final.

Contrato e evidências: [`docs/restore-command.md`](restore-command.md).

#### RDB-054 — Entregar `reprodb pull`

**Labels:** `priority:p0`, `type:feature`, `area:cli`

**Status:** implementada — o profile ativo e a metadata local permitem decidir o cache antes de qualquer credencial/conexão source. Hit reutiliza o UUID pelo RestoreService; miss e `--fresh` usam o DumpService e depois o mesmo restore. A UX diferencia os caminhos e o teste real no `mysql-8` remove a credencial source antes do segundo pull para provar o hit offline.

**Fluxo:** config → cache → resolver/credencial/dump somente no miss → target gate → restore → registro central → success.

**Aceite:** cache hit não acessa o source; `--fresh` sempre produz novo dump; retry de restore reutiliza o artefato.

Contrato e evidências: [`docs/pull-command.md`](pull-command.md).

#### RDB-055 — Implementar comandos de cache

**Labels:** `priority:p1`, `type:feature`, `area:cache`, `area:cli`

**Status:** implementada — `list` inventaria todos os profiles e verifica metadata, identidade, tamanho e SHA-256 sob lease; `clean` remove somente expirados/partials/deleções interrompidas; `purge TENANT` atua no profile ativo e aceita lookup ou tenant ID canônico.

**Aceite:** nunca remove artefato locked; remoções materiais são reportadas.

Contrato e evidências: [`docs/cache-commands.md`](cache-commands.md).

#### RDB-056 — Selecionar target e database no `pull`

**Labels:** `priority:p0`, `type:feature`, `area:cli`, `area:restore`, `area:docker`

**Status:** implementada — o core separa source/target database; `setup` preserva múltiplos targets e define o último como default; `pull` oferece seletor interativo e `--target`; `pull`/`restore` aceitam `--target` e `--database` validados. O UUID observado do MySQL source é gravado no artefato e comparado ao target antes de qualquer mutação. O E2E cria um segundo MySQL, exporta de A, restaura em B e repete por cache.

**Escopo:** permitir múltiplos targets locais cadastrados pelo `setup`, com um target default. Em terminal interativo, `pull` mostra os targets MySQL disponíveis/configurados e pergunta o container e o nome do database de destino; os defaults são o target ativo e o database vindo do dump. Flags explícitas equivalentes devem manter automação possível.

**Barreiras:** o target continua sendo um container Docker atestado; database customizado passa pelo value object, bloqueio de schemas administrativos e política de prefixo local. Quando um source profile local puder ser associado a um container Docker, persistir sua identidade e impedir `DROP` no mesmo container/database do source.

**Aceite:** teste com dois containers MySQL exporta do A e restaura no B; aceitar os prompts sem editar preserva o nome original; escolher outro nome restaura somente esse database; cache hit também oferece a mesma escolha.

#### RDB-057 — Exibir ETA estimado durante dump

**Labels:** `priority:p1`, `type:feature`, `area:cli`, `area:mysql`, `area:performance`

**Status:** implementada — o preflight soma `information_schema.tables.data_length`; o observer recebe a estimativa antes do stream; dump/pull exibem `ETA ~mm:ss` somente após dois segundos e escondem a previsão quando a base é zero ou já foi ultrapassada.

**Escopo:** coletar no preflight uma estimativa lógica com `information_schema.tables`, combinar o total estimado com os bytes SQL realmente recebidos e recalcular a duração restante depois de uma janela mínima de amostragem.

**UX:** mostrar `ETA ~mm:ss` e identificar visualmente que é uma estimativa. Não prometer percentual exato: BLOBs, escaping, índices e estatísticas do MySQL fazem o tamanho do dump textual divergir do tamanho das tabelas.

**Aceite:** ETA não aparece sem base suficiente, nunca divide por zero, se ajusta durante o stream e deixa de mostrar uma duração enganosa quando a estimativa já foi ultrapassada.

### Milestone 6 — Resiliência, performance e cross-platform

Critério da milestone: falhas deixam estado compreensível e o fluxo é comprovado em macOS e Linux.

#### RDB-060 — Implementar cancelamento e supervisão de processos

**Labels:** `priority:p0`, `type:hardening`, `area:cross-platform`

**Escopo:** Ctrl+C durante dump, compressão e restore; kill + wait; fechamento de stdin; cleanup de option file/partial; liberação de locks.

**Aceite:** nenhum child órfão conhecido, cache completo anterior preservado e exit code 130.

#### RDB-061 — Implementar fault injection

**Labels:** `priority:p0`, `type:test`, `area:mysql`, `area:cache`, `area:restore`

**Cenários:** host/porta/senha inválidos, permission denied, disconnect, disco cheio, stderr grande, Docker parado, container ausente, imagem ausente, Zstd corrompido e falha no rename.

**Aceite:** cada falha possui teste e mensagem acionável sem secret.

#### RDB-062 — E2E real no Linux

**Labels:** `priority:p0`, `type:test`, `area:cross-platform`

**Aceite:** binário real executa `setup` não interativo de teste, profile, pull, cache hit e comparação do target em CI com Docker.

#### RDB-063 — Smoke suite no macOS Intel/ARM

**Labels:** `priority:p0`, `type:test`, `area:cross-platform`

**Escopo:** paths, keychain, Docker Desktop, host gateway, imagem do client, sinais e permissões.

**Aceite:** execução documentada em pelo menos Apple Silicon; Intel fica obrigatório se fizer parte do parque real.

#### RDB-064 — Benchmark com tenant representativo

**Labels:** `priority:p1`, `type:test`, `area:mysql`, `area:cache`

**Métricas:** duração do dump/restore, bytes, ratio, throughput, pico de RAM, cache hit/miss e tempo total.

**Aceite:** benchmark com tenant pequeno e outro de pelo menos centenas de MiB; nível Zstd escolhido com dados.

### Milestone 7 — Hardening para produção

Critério da milestone: source de produção é habilitado somente após compatibilidade, segurança e impacto medidos.

#### RDB-070 — Implementar TLS do source

**Labels:** `priority:p0`, `type:hardening`, `area:mysql`, `area:security`

**Escopo:** `disabled`, `preferred`, `required`, `verify_ca`, `verify_identity`, CA/cert paths sem conteúdo sensível no TOML.

**Aceite:** produção não pode degradar TLS silenciosamente; certificado inválido falha antes do dump.

#### RDB-071 — Implementar políticas para source classificado como produção

**Labels:** `priority:p0`, `type:hardening`, `area:security`

**Escopo:** destaque visual, cache obrigatório, `--fresh` explícito, dump policy conservadora e proibição arquitetural de restore no source.

**Aceite:** o booleano/classificação não é a única barreira; restore continua restrito ao `LocalTarget` validado.

#### RDB-072 — Revisar permissões e objetos reais

**Labels:** `priority:p0`, `type:hardening`, `area:mysql`

**Escopo:** grants mínimos, InnoDB, views, triggers, routines, events, definers, GTID e testes de restore desses objetos.

**Aceite:** matriz documentada e restore funcional dos objetos realmente usados pelo Salt.

#### RDB-073 — Executar piloto controlado

**Labels:** `priority:p0`, `type:test`, `area:mysql`, `area:security`

**Escopo:** tenant pequeno, janela de baixo tráfego, acompanhamento do banco, medição, restore local e comparação funcional.

**Aceite:** vários dumps/restores bem-sucedidos, impacto aceito pelo responsável e nenhuma falha de consistência conhecida.

#### RDB-074 — Documentar segurança e operação

**Labels:** `priority:p1`, `type:docs`, `area:security`

**Escopo:** threat model, retenção, disco criptografado, cache contendo produção, incidentes, cleanup, troubleshooting e limitações de lock local.

**Aceite:** documentação interna revisada antes da liberação ampla.

## 8. Dependências e caminho crítico

```text
RDB-001 ─┐
         ├─> RDB-002 ─> RDB-003 ─> RDB-010
RDB-004 ─┘                         │
                                   ├─> configuração/setup/profile
                                   ├─> resolver Salt Central
                                   └─> dump/cache/restore/pull

RDB-005 ─> registro central local ─> pull realmente utilizável no Salt

MVP local completo
  ─> fault injection + macOS/Linux
  ─> TLS + matriz do source real
  ─> piloto de produção
```

Não iniciar integração de produção antes de concluir as milestones 0 a 6.

## 9. Definition of Done do MVP local

- [ ] `reprodb setup` encontra ou cria um target MySQL Docker e grava sua identidade.
- [ ] `profile add` coleta conexão como o DBeaver, testa e salva a senha no keyring.
- [ ] A versão do MySQL é detectada por consulta, não inferida de `mysql:8`.
- [ ] O client é uma imagem conhecida, fixada e compatível.
- [x] `reprodb doctor` valida o ambiente implementado antes do dump, com os checks específicos do dump adicionados nas issues da milestone 4.
- [x] Alias ou tenant ID resolve via `salt_central`.
- [x] `tenancy_db_name` é respeitado.
- [ ] Dump usa flags conservadoras e `--set-gtid-purged=OFF`.
- [x] Compressão e restore são streaming.
- [ ] Nenhum SQL cru é persistido no fluxo normal.
- [ ] Cache é atômico, possui checksum, TTL e source fingerprint.
- [x] Restore valida o artefato antes de dropar o database.
- [x] Target Docker remoto ou trocado é bloqueado.
- [x] Registro central local mínimo é criado sem copiar secrets.
- [ ] Ctrl+C limpa child, partial, option file e lock.
- [ ] E2E passa no Linux.
- [ ] Smoke suite passa no macOS usado pela equipe.

## 10. Definition of Done para produção

- [ ] Versão/vendor exatos do source foram medidos.
- [ ] Client/source/target pertencem a uma matriz testada.
- [ ] TLS é verificado.
- [ ] Engines e DDL concorrente foram avaliados.
- [ ] Views/triggers/routines/events/definers reais foram restaurados em teste.
- [ ] Permissões mínimas do usuário foram revisadas.
- [ ] Nenhum segredo aparece em arquivo persistente, argv, logs ou erro.
- [ ] Política de retenção de dados de produção foi aprovada.
- [ ] Disco criptografado e diretório de cache adequado são requisitos documentados.
- [ ] Piloto com tenant pequeno mediu duração e impacto.
- [ ] Restore local foi validado funcionalmente no Salt.
- [ ] Limitação de lock apenas local está documentada.

## 11. Fora do MVP

- servidor ou cache central;
- S3, SQS, Redis ou Lambda;
- coordenação entre computadores;
- atualização automática da CLI;
- busca aberta na internet por executáveis;
- download/instalação silenciosa de pacotes no host;
- MariaDB ou Percona sem matriz específica;
- importação arbitrária de `.sql` ou `.sql.zst`;
- cópia integral de `salt_central`;
- cópia de credenciais de integração do tenant;
- sanitização genérica de SQL;
- criptografia própria de credentials;
- parser SQL próprio;
- restore em host MySQL arbitrário;
- restore em Docker context remoto;
- interface web.

## 12. Referências do Salt usadas na análise

- `Salt/config/tenancy.php`: prefix/suffix, central connection e database tenancy.
- `Salt/app/Models/Tenant.php`: modelo `TenantWithDatabase`.
- `Salt/vendor/stancl/tenancy/src/DatabaseConfig.php`: regra efetiva de `tenancy_db_name` ou tenant ID.
- `Salt/app/Support/Testing/TenantBootstrap/TenantSnapshotManager.php`: snapshot existente com `mysqldump`/`mysql`.
- `Salt/app/Support/Testing/TenantBootstrap/TenantCatalog.php`: tenants locais de teste.
- `Salt/.github/workflows/cicd.yml`: serviço configurado com a tag mutável `mysql:8`.
- `Salt/README.md`: baseline declarado de MySQL 8 e documentação do snapshot de tenant.

## 13. Referências técnicas externas

- MySQL option files: <https://dev.mysql.com/doc/refman/8.4/en/option-files.html>
- MySQL option-file handling: <https://dev.mysql.com/doc/refman/8.4/en/option-file-options.html>
- MySQL `mysqldump`: <https://dev.mysql.com/doc/refman/8.0/en/mysqldump.html>
- Docker Official Image — MySQL: <https://hub.docker.com/_/mysql>
- Docker container list: <https://docs.docker.com/reference/cli/docker/container/ls/>
- Docker contexts: <https://docs.docker.com/engine/manage-resources/contexts/>
- Tokio processes: <https://docs.rs/tokio/latest/tokio/process/>
- Async compression: <https://docs.rs/async-compression/latest/async_compression/>

## 14. Regra de manutenção deste documento

Este roadmap é a referência até as issues serem criadas no tracker.

Quando uma decisão mudar:

1. registrar uma ADR ou atualizar a seção de decisões;
2. atualizar a issue afetada;
3. ajustar dependências e Definition of Done;
4. não alterar silenciosamente critérios de aceite já usados em implementação.

Quando as issues forem criadas, registrar neste arquivo o link correspondente ao lado de cada `RDB-NNN`.
