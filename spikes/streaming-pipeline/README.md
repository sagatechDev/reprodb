# Streaming pipeline spike

Crate descartável da RDB-003. Ele prova dois caminhos:

```text
dockerized mysqldump stdout -> async Zstd -> FILE.part -> atomic rename
FILE.zst -> async Zstd decode -> dockerized mysql stdin
```

Não é a estrutura final do reprodb. As decisões aprovadas serão posteriormente movidas para o crate principal.

## E2E local automatizado

Por padrão, o harness utiliza o container local `mysql-8`. Ele recusa executar se os databases reservados do spike já existirem.

```bash
./run-local-e2e.sh
```

O harness cria e remove somente:

```text
reprodb_spike_source
reprodb_spike_target
```

Ele também verifica dados, UTF-8, NULL, decimal, BLOB, FK, trigger, view, collation, ausência de `.part` e ausência de client container órfão.

## Uso

O option file deve ser absoluto, possuir permissões restritas e seguir o formato MySQL:

```ini
[client]
host=host.docker.internal
port=3306
user=root
password="<secret>"
protocol=TCP
```

Dump:

```bash
cargo run -- \
  dump \
  mysql:<tag>@sha256:<digest> \
  /absolute/path/source.cnf \
  salt_test \
  /tmp/salt_test.sql.zst
```

Restore, depois que o database target já existir:

```bash
cargo run -- \
  restore \
  mysql:<tag>@sha256:<digest> \
  /absolute/path/target.cnf \
  reprodb_spike_target \
  /tmp/salt_test.sql.zst
```

## Propriedades exercitadas

- argumentos estruturados, sem shell;
- password fora de argv;
- option file montado read-only;
- compressão/decompressão assíncrona;
- stderr drenado concorrentemente e limitado a 64 KiB;
- client container nomeado e encerrado no Ctrl+C;
- `.part` removido em erro conhecido;
- rename somente após exit code zero;
- database administrativo bloqueado.

## Limitações deliberadas

- o harness ainda prepara source/target fora do binário;
- a publicação não executa `fsync`;
- sinais diferentes de Ctrl+C não foram tratados;
- um `SIGKILL` do processo Rust pode deixar um client container temporário;
- métricas, checksum e metadata pertencem às issues posteriores;
- Linux e VPN ainda precisam de validação.
