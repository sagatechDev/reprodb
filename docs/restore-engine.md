# RestoreEngine streaming

> RDB-051 implementa o núcleo; a RDB-053 o conecta ao comando público `reprodb restore` documentado em [`restore-command.md`](restore-command.md).

O restore não aceita caminho de arquivo, host MySQL nem `SourceProfile`. Sua API exige simultaneamente:

- `ValidatedRestoreArtifact`, emitido somente pelo validator do cache gerenciado;
- `AuthorizedLocalTarget`, emitido somente depois da barreira do target Docker local.

## Ordem e barreiras

```text
metadata estrita
    -> tamanho + SHA-256 do .zst
    -> decode Zstd completo + tamanho/SHA-256 do SQL
    -> target local atestado e database autorizado
    -> UUID do servidor target diferente do UUID source do dump
    -> compatibilidade source/client/target
    -> lock local por context + container ID + database
    -> estado persistido como incomplete
    -> DROP/CREATE com charset e collation originais
    -> decode Zstd -> stdin do mysql
    -> exit code zero + bytes/SHA-256 conferidos novamente
    -> estado ready
```

Um arquivo arbitrário não pode ser convertido diretamente em `ValidatedRestoreArtifact`. O comando localiza um UUID único sob `~/.reprodb/cache/profiles`, recupera profile e tenant ID dos diretórios tipados e então valida a identidade completa. O validator mantém uma lease compartilhada durante todo o restore e recusa metadata divergente, corrupção, truncamento e stream Zstd inválido antes de qualquer operação destrutiva.

O checksum do SQL é conferido uma segunda vez durante a importação. Isso detecta uma alteração entre a validação e o consumo, sem carregar o dump na memória.

## Pipeline e memória

O decoder Zstd síncrono roda numa tarefa bloqueante e entrega no máximo dois blocos de 64 KiB ao runtime assíncrono. Esses blocos são escritos diretamente no stdin de:

```text
docker --context <local> run --rm -i
  --pull=never
  --network=container:<ID completo>
  --mount type=bind,src=<private-temp-dir>,dst=/run/secrets/reprodb,readonly
  <imagem MySQL aprovada por digest>
  mysql --defaults-file=/run/secrets/reprodb/client.cnf --no-login-paths ...
```

Não há shell, `docker cp`, TTY ou SQL cru intermediário. `-i` é obrigatório para manter o stdin do client container aberto; `-t` é proibido porque um pseudo-terminal pode alterar o stream.

A senha fica somente no `SecretString` e no option file efêmero `0600` criado no diretório temporário seguro do sistema operacional. Ela não aparece no argv, metadata, estado ou diagnóstico. A conexão com o MySQL target exige TLS, inclusive para autenticação `caching_sha2_password` no MySQL 8.4. O stderr é drenado em paralelo, limitado a 64 KiB e usado apenas para classificar a falha; seu conteúdo não é devolvido nos erros públicos.

## Recriação e estado recuperável

O database target pode manter o nome do source ou usar outro nome explicitamente escolhido dentro da allowlist local. O `DROP DATABASE` e `CREATE DATABASE` usam apenas `DatabaseName`, charset e collation já validados, com o encoding registrado no source no momento do dump. Mesmo com outro nome, o restore é recusado se `@@server_uuid` indicar que target e source são o mesmo MySQL.

Antes do `DROP`, o engine grava atomicamente:

```text
~/.reprodb/data/restores/<identidade-opaca>.json
```

com status `incomplete`. A identidade do arquivo é um SHA-256 de context, container ID e database, sem expor esses nomes no path. Somente depois de import e exit code bem-sucedidos o status vira `ready`. Assim, falha no recreate, import ou queda da CLI nunca preserva um estado `ready` antigo.

O artefato completo não é removido numa falha. Um retry adquire novamente o lock, recria o database e reutiliza o mesmo dump sem acessar o source.

## Validação real atual

O teste ignorado `tests/restore_integration.rs` foi executado no macOS contra o container local `mysql-8` e o client MySQL 8.4.4 fixado por digest. Ele cria um database único `salt_reprodb_restore_*`, restaura tabela InnoDB com UTF-8/emoji, `NULL` e BLOB, compara os bytes armazenados e remove o database ao final.

Execução manual:

```bash
cargo test --test restore_integration -- --ignored --nocapture
```

O teste pode usar `REPRODB_TEST_MYSQL_CONTAINER` para selecionar outro container local compatível. A cobertura equivalente em Linux permanece na milestone cross-platform.

O E2E de `pull` cria ainda um segundo container MySQL efêmero, exporta do `mysql-8`, restaura no segundo servidor e comprova a reutilização do cache sem credencial source:

```bash
cargo test --test pull_integration -- --ignored --nocapture
```

## Limites mantidos para as próximas issues

- RDB-052 registra o tenant mínimo no `salt_central` local somente após o tenant estar pronto;
- RDB-053 liga lookup, dump ID, target gate, engine, registro central e UX no comando `restore`;
- RDB-060 adicionou cancelamento explícito do client container, espera do child, cleanup e exit code 130;
- nenhum restore em host arbitrário, source profile ou Docker context remoto é suportado.
