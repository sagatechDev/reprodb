# RDB-003 — Spike end-to-end dump → Zstd → restore

> **Histórico.** Pesquisa datada, anterior à remoção do conceito de tenant. Mantida como registro.

> Status: concluído no ambiente macOS em 5 de setembro de 2026. A validação equivalente em Linux permanece nas issues cross-platform RDB-002 e RDB-061.

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
./run-all.sh
```

O harness E2E principal sobe dois servidores MySQL independentes em containers efêmeros e publica portas aleatórias somente em loopback. Os scripts removem os recursos que criaram ao sair.

Também existe um harness mais curto contra o container local `mysql-8`. Ele usa nomes de database reservados, recusa sobrescrever bancos preexistentes e obtém a senha por `docker inspect` apenas para o teste local; no produto a senha virá do `CredentialStore`.

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

Resultado do E2E com os servidores independentes:

```text
dump stream completed: 1756309 uncompressed bytes
restore stream completed: 1756309 uncompressed bytes
validation=ok
source_target_servers=independent
compressed_bytes=89363
```

Resultado dos testes determinísticos de cancelamento:

```text
dump_cancellation_exit=130
dump_partial_removed=yes
restore_cancellation_exit=130
completed_dump_preserved=yes
```

O teste de memória comparou streams sintéticos com diferença de 20 vezes:

```text
small_stream_bytes=5125000
small_max_rss_bytes=3293184
large_stream_bytes=102500000
large_max_rss_bytes=3309568
memory_validation=ok
```

O RSS máximo cresceu aproximadamente 16 KiB enquanto o stream aumentou de cerca de 5 MiB para 98 MiB, sustentando a propriedade de memória aproximadamente constante para o pipeline exercitado.

Após o teste também foram confirmados:

- nenhum database reservado do spike permaneceu no servidor;
- nenhum diretório temporário do harness permaneceu em `/tmp`;
- nenhum `.part` permaneceu;
- nenhum client container nomeado pelo spike permaneceu;
- `cargo fmt --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings` e `cargo build --release` passaram;
- os três testes unitários de validação de database e caminho parcial passaram.

## Descoberta importante

O primeiro restore fechava o pipe com `Broken pipe` e stderr vazio. A causa era o client container ter sido criado sem `docker run -i`: sem stdin aberto, o processo `mysql` recebia EOF imediatamente.

Isso deve virar uma asserção da futura construção de comandos do `DockerCliRuntime`: toda operação que recebe stream precisa incluir `-i`, mas nunca `-t`.

## Limites deste resultado

- o spike usou um Docker fake para manter os streams bloqueados; a RDB-060 incorporou token cooperativo, `kill + wait` do child e remoção explícita do container efêmero nomeado;
- Linux e rede/VPN continuam pendentes em RDB-002 e RDB-061;
- `SIGKILL` não permite cleanup pelo processo e pode deixar um client container até uma limpeza oportunista futura;
- persistência durável com `fsync`, checksum e metadata pertencem à implementação do cache, não a este spike;
- a fixture de aproximadamente 1,7 MiB prova compatibilidade funcional, não representa o impacto de um tenant real no source.

RDB-003 prova a viabilidade técnica do núcleo. Ela não encerra a milestone 0 e não autoriza conexão com produção enquanto RDB-002 e RDB-005 estiverem abertas.
