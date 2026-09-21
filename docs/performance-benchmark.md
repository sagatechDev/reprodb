# Benchmark local de dump, restore e cache

A RDB-064 usa o mesmo pipeline real do E2E: dois MySQL 8.4 efêmeros, `mysqldump`, Zstd, publicação no cache, restore em outro servidor e um segundo restore por cache hit sem credencial source.

O benchmark é opt-in e roda em `--release`:

```bash
./scripts/benchmark-local.sh
```

Ele executa três casos:

1. fixture mínima de duas linhas com Zstd 1;
2. 450 mil linhas e aproximadamente 220 MiB de payload com Zstd 1;
3. a mesma massa determinística com Zstd 3.

O tamanho pode ser reduzido durante o desenvolvimento:

```bash
REPRODB_BENCHMARK_LARGE_ROWS=50000 ./scripts/benchmark-local.sh
```

O limite do harness é 500 mil linhas. Cada linha grande contém 512 bytes derivados de SHA-256, evitando uma massa artificialmente composta somente por texto repetido.

## Métricas

Cada execução imprime uma linha estável com:

```text
rows
zstd_level
logical_payload_bytes
dump_input_bytes
compressed_bytes
ratio
dump_seconds
restore_seconds
total_seconds
cache_hit_seconds
```

`dump_seconds` vem do pipeline `mysqldump → Zstd → arquivo`; `restore_seconds` inclui validação/atestation, import e registro local; `total_seconds` cobre o `pull` inteiro. O tempo externo inclui também criação e seed dos containers, portanto não deve ser confundido com o tempo do produto.

O script usa `/usr/bin/time`: macOS reporta `maximum resident set size` em bytes e Linux em KiB. Essa medição é um teto do processo de teste que hospeda o core e lança os clients Docker, não uma medição isolada do daemon Docker. Comparar massas pequenas e grandes é o que comprova que a memória do pipeline não cresce proporcionalmente ao dump.

## Baseline Apple Silicon

Medição em 7 de setembro de 2026, macOS arm64, Docker Desktop 27.5.1 e MySQL/client 8.4:

| Massa | Zstd | SQL observado | Artefato | Razão | Dump | Restore | Pull total | Cache hit | RSS máximo |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 2 linhas | 1 | 2.129 B | 913 B | 42,88% | 0,404 s | 2,616 s | 5,259 s | 2,208 s | 58,1 MiB |
| 450 mil linhas / 219,7 MiB lógicos | 1 | 232,8 MiB | 115,3 MiB | 49,52% | 6,924 s | 10,629 s | 19,498 s | 11,223 s | 59,0 MiB |
| 450 mil linhas / 219,7 MiB lógicos | 3 | 232,8 MiB | 119,5 MiB | 51,31% | 9,171 s | 13,409 s | 24,601 s | 11,337 s | 58,5 MiB |

O RSS permaneceu praticamente constante entre 2 KiB e 244 MB de dump SQL. Nesta fixture, Zstd 3 foi mais lento e produziu um artefato maior. Zstd 1 continua sendo o default. Isso não substitui uma medição futura com um database real e autorizado antes de produção.
