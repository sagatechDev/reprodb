# `reprodb push` — design

Data: 2026-10-04 · Status: implementado

> **Revisão durante a implementação.** O config real tinha o profile de produção com `production = false`, então a flag sozinha não protege. Mudanças em relação ao desenho abaixo:
> - destino exige opt-in explícito (`reprodb profile allow-push NAME`, campo `push_destination`); produção nunca pode ser liberada;
> - recusa destino que compartilha host:porta com profile não liberado (loopbacks normalizados);
> - recusa destino cujo `@@server_uuid` aparece em dump em cache de profile não liberado ou removido;
> - o servidor de origem do dump só é aceito se o profile de origem também estiver liberado;
> - cada sessão de escrita revalida `@@server_uuid` antes do primeiro comando;
> - confirmação digitada é `profile/database`; sem terminal e sem `--yes`, falha antes do dump.
> Contrato atual: [`docs/push-command.md`](../../push-command.md).

## Objetivo

Importar um dump gerenciado (por exemplo, de produção) em um database de **outro profile** — tipicamente um sandbox — sem passar pelo MySQL local.

```console
$ reprodb push salt_sagatec

  Destination profile:  sandbox
  Dump:                 9f1c2b7a… (prod-source, 34m)   [ou: gerar dump novo agora]
  Remote database [salt_sagatec]: salt_sagatec_qa

Push plan
  Source:       prod-source / salt_sagatec
  Dump ID:      9f1c2b7a-5f0e-4b31-9c2f-7a0d8e1b4c66
  Destination:  sandbox (sandbox.db.internal:3306) / salt_sagatec_qa
  MySQL:        8.0.45 -> 8.4.4

! Tables present in the dump will be replaced in the remote database.
  Type the database name to confirm: salt_sagatec_qa

✓ Push complete
```

## Decisões

| Tema | Decisão |
|---|---|
| Banco destino já existe | Importa por cima. Sem `DROP DATABASE`. O dump usa o `--opt` default do mysqldump (`--add-drop-table`), então cada tabela do dump é recriada; tabelas que só existem no destino permanecem. |
| Banco destino não existe | `CREATE DATABASE IF NOT EXISTS` com charset/collation da metadata do dump. |
| Origem do dump | Seletor lista os dumps do cache para o database e oferece "gerar dump novo agora" no topo. Dump novo usa o profile **ativo** como origem, igual ao `pull`. |
| Destino de produção | Profile com `production = true` é recusado como destino, sem override. |
| Confirmação | Interativo: digitar o nome do database remoto. `--yes` pula. |

## Interface

```text
reprodb push <DATABASE>
    [--profile <PROFILE>]          # profile de destino
    [--dump-id <ID> | --fresh]     # dump do cache ou dump novo
    [--database <NAME>]            # nome do database remoto (default: DATABASE)
    [--yes]                        # pula a confirmação digitada
```

- `DATABASE` é o database de origem gravado no dump (mesma regra do `restore`).
- Sem TTY, cada valor que seria perguntado precisa vir por flag; faltando algum, o comando falha listando as opções válidas (mesmo padrão do `CliRestoreDumpSelector`).
- O seletor de profile lista apenas profiles não-produção. Se nenhum existir, falha com instrução de `reprodb profile add`.

## Arquitetura

Fluxo paralelo ao restore local. `AuthorizedLocalTarget`, `LocalTargetGate` e `RestoreEngine` não são alterados — o restore local mantém exatamente as mesmas garantias.

```text
CLI push (src/cli/push.rs)
  └─ PushService (src/application/push_service.rs)
       ├─ seleção de dump: cache existente ou DumpService (profile ativo)
       ├─ ValidatedRestoreArtifact (validação existente: tamanho, zstd, SHA-256)
       ├─ RemoteTargetGate → AuthorizedRemoteTarget
       └─ RemoteImportExecutor (src/infrastructure/mysql/remote_import_executor.rs)
            └─ streaming compartilhado extraído de restore_executor.rs
```

### Unidades

- **`stream_import` (extraído de `restore_executor.rs`)** — recebe um `ProcessSpec` já montado, o artefato, o docker context, o nome do container efêmero e o `CancellationToken`; faz zstd → stdin, captura stderr limitado, trata Ctrl+C e retorna `RestoreMetrics`. `DockerMysqlRestoreExecutor::import` passa a chamá-lo sem mudar comportamento.
- **`RemoteTargetGate`** — dado o profile de destino e o database:
  1. recusa `production = true`;
  2. recusa databases de sistema (`mysql`, `information_schema`, `performance_schema`, `sys`), reutilizando a validação existente;
  3. lê a credencial do profile e conecta (client efêmero com option file + TLS do profile, como em `dump_executor`), lendo `@@server_uuid` e `@@version`;
  4. recusa se `server_uuid` do destino == `source_server_uuid` do dump **e** o nome do database for igual ao de origem;
  5. aplica a mesma regra de compatibilidade de versão do restore local (mesma major, destino ≥ origem; client do profile na série do servidor).
  Retorna `AuthorizedRemoteTarget` (construtor privado ao módulo, como o tipo local).
- **`RemoteImportExecutor`** — `ensure_database` (`CREATE DATABASE IF NOT EXISTS …`) e `import` (`mysql --binary-mode --database=… --default-character-set=…`). O `ProcessSpec` usa a imagem de client do profile, `--add-host=host.docker.internal:host-gateway` e o option file com host/porta/usuário/senha/TLS do profile — nunca `--network=container:`.
- **`PushService`** — orquestra: resolve dump → valida artefato → autoriza destino → apresenta plano → confirma → adquire lock `(profile, database)` → ensure → import → compara bytes importados com `uncompressed_bytes`.

### Estado

Push não grava `LocalRestoreStateStore` (é estado do target local). O lock usa o `OperationLockManager` existente com uma chave nova `OperationLockKey::remote(profile, database)`.

## Erros

| Situação | Exit code |
|---|---|
| Destino é produção, database de sistema, colisão com a origem | `10` (configuração) |
| Credencial do profile ausente/ilegível | `11` |
| Destino inacessível / TLS / autenticação | `30` |
| Cache/artefato inválido | `50` |
| Docker indisponível | `60` |
| Versão incompatível, falha de CREATE/import, sem privilégio, tamanho importado divergente | `70` |

Nenhuma escrita acontece antes de todos os guards passarem. Falha no meio do import deixa o database remoto parcialmente importado; a mensagem diz isso e que repetir o mesmo comando reimporta por cima.

## Testes

- Unitários do gate: produção recusada, database de sistema, colisão uuid+nome, mesmo servidor com nome diferente aceito, versões (8.0→8.4 ok, 8.4→8.0 recusado).
- Unitários do `ProcessSpec` remoto: sem `--network=container:`, sem `-t`, `--defaults-file` montado readonly, `--binary-mode`.
- Unitário do `PushService` com executor fake: ordem gate → confirmação → lock → ensure → import; confirmação negada não chama executor; tamanho divergente falha.
- Regressão: testes existentes de `restore_executor` continuam passando após a extração de `stream_import`.
- Integração `#[ignore]`: sobe um segundo container MySQL como "remoto", cadastra profile não-produção, faz push duas vezes e compara os dados.

## Docs

- Novo `docs/push-command.md`.
- README: linha na tabela de comandos e ajuste na garantia "nada é escrito na origem" → só o `push` escreve em host remoto, e nunca em profile de produção.

## Fora de escopo

- Push para profile de produção (nem com flag).
- Drop do database remoto ou limpeza de tabelas extras.
- Push de arquivos `.sql` arbitrários fora do cache gerenciado.
