# Streaming pipeline spike

Crate descartável da RDB-003. Ele prova dois caminhos:

```text
dockerized mysqldump stdout -> async Zstd -> FILE.part -> atomic rename
FILE.zst -> async Zstd decode -> dockerized mysql stdin
```

Não é a estrutura final do reprodb. As decisões aprovadas serão posteriormente movidas para o crate principal.

Para executar todo o aceite automatizado do spike:

```bash
./run-all.sh
```

## E2E local automatizado

O harness principal sobe dois servidores MySQL efêmeros e independentes, publica portas aleatórias somente em loopback e remove os containers ao sair:

```bash
./run-ephemeral-e2e.sh
```

Há também um harness rápido que utiliza o container local `mysql-8`. Ele recusa executar se os databases reservados do spike já existirem:

```bash
./run-local-e2e.sh
```

O harness cria e remove somente:

```text
reprodb_spike_source
reprodb_spike_target
```

Os dois verificam dados, UTF-8, NULL, decimal, BLOB, FK, trigger, view, collation e ausência de `.part`. O harness efêmero compara snapshots produzidos separadamente pelo source e pelo target.

O cancelamento é exercitado deterministicamente com um adapter Docker fake que mantém os streams bloqueados:

```bash
./run-cancellation-test.sh
```

Esse teste espera exit code 130, remove o parcial do dump e preserva um dump completo quando o restore é cancelado.

O comportamento de memória pode ser comparado com streams sintéticos de aproximadamente 5 MiB e 98 MiB:

```bash
./run-memory-test.sh
```

O teste usa o RSS máximo reportado pelo sistema e falha se o stream 20 vezes maior consumir mais de três vezes a memória do menor. Ele é uma proteção inicial contra buffering acidental, não um benchmark de produção.

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

- os harnesses ainda preparam source/target fora do binário;
- a publicação não executa `fsync`;
- sinais diferentes de Ctrl+C não foram tratados;
- um `SIGKILL` do processo Rust pode deixar um client container temporário;
- métricas, checksum e metadata pertencem às issues posteriores;
- o cancelamento determinístico usa um Docker fake; a integração com `docker kill` real ainda terá cobertura própria no runtime final;
- Linux e VPN ainda precisam de validação nas issues cross-platform.
