# RDB-003 — Spike end-to-end dump → Zstd → restore

> Status: em andamento. O núcleo do pipeline e o harness local no macOS foram validados em 5 de setembro de 2026.

## O que foi implementado

O crate descartável em [`spikes/streaming-pipeline`](../../spikes/streaming-pipeline/README.md) executa estes pipelines:

```text
dockerized mysqldump stdout
  -> cópia assíncrona
  -> encoder Zstd nível 1
  -> dump.sql.zst.part
  -> rename após exit code zero

dump.sql.zst
  -> decoder Zstd assíncrono
  -> dockerized mysql stdin
```

O processo Rust não usa shell para montar os comandos. A credencial fica em um option file temporário com modo `0600`, montado read-only no client container, e não aparece no argv.

O stdout do dump e o stdin do restore são processados como streams. O stderr é drenado em uma task concorrente e limitado a 64 KiB para diagnóstico sem crescimento ilimitado de memória.

## Harness automatizado

O comando reproduzível é:

```bash
cd spikes/streaming-pipeline
./run-local-e2e.sh
```

O harness usa nomes reservados, recusa sobrescrever bancos preexistentes e remove os recursos que criou ao sair. A obtenção da senha pelo `docker inspect` pertence somente ao ambiente local do spike; no produto a senha virá do `CredentialStore`.

A fixture cobre:

- UTF-8, acentos e emoji;
- `NULL`;
- `BIGINT`;
- `DATETIME(6)`;
- `DECIMAL`;
- `BLOB`;
- chave estrangeira e índice unique;
- trigger;
- view;
- 10.000 registros de detalhe;
- charset e collation do database.

O restore é comparado ao source por contagem e checksums lógicos, além da presença da FK, trigger, view e collation.

## Resultado observado no macOS

Ambiente:

| Componente | Valor |
|---|---|
| Host | macOS ARM64 |
| Docker | Docker Desktop, context `desktop-linux` |
| Servidor usado pelo harness | MySQL 8.4.4 no container local `mysql-8` |
| Client | imagem oficial fixada pelo digest `sha256:1d967f...b01c` |
| Transporte | TCP por `host.docker.internal:3306` |

Resultado de uma execução limpa:

```text
dump stream completed: 1756325 uncompressed bytes
restore stream completed: 1756325 uncompressed bytes
validation=ok
compressed_bytes=89904
orphan_client_container=no
```

Após o teste também foram confirmados:

- nenhum database reservado do spike permaneceu no servidor;
- nenhum diretório temporário do harness permaneceu em `/tmp`;
- nenhum `.part` permaneceu;
- nenhum client container nomeado pelo spike permaneceu;
- `cargo fmt --check`, `cargo test`, `cargo clippy --all-targets --all-features -- -D warnings` e `cargo build --release` passaram;
- os três testes unitários de validação de database e caminho parcial passaram.

## Descoberta importante

O primeiro restore fechava o pipe com `Broken pipe` e stderr vazio. A causa era o client container ter sido criado sem `docker run -i`: sem stdin aberto, o processo `mysql` recebia EOF imediatamente.

Isso deve virar uma asserção da futura construção de comandos do `DockerCliRuntime`: toda operação que recebe stream precisa incluir `-i`, mas nunca `-t`.

## O que este resultado ainda não prova

RDB-003 permanece em andamento porque o aceite completo também exige:

- source e target em dois servidores MySQL efêmeros e independentes;
- execução equivalente no Docker Engine nativo em Linux;
- medição de memória com dump significativamente maior para sustentar a hipótese de RAM aproximadamente constante;
- teste automatizado de Ctrl+C durante dump e restore;
- comprovação automática da remoção de `.part` e client container nos caminhos de erro/cancelamento;
- tratamento e teste de sinais além do caminho atual de Ctrl+C quando aplicável;
- persistência durável (`fsync`) antes da publicação atômica, a ser definida na implementação real.

O spike valida a viabilidade do desenho, mas ainda não autoriza encerrar a milestone 0 nem conectar em produção.
