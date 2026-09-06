# Compressão streaming

O `ZstdCompressor` recebe um `AsyncRead` e um `Write` já aberto, produzindo um frame Zstandard sem materializar o dump inteiro. A leitura usa blocos de 64 KiB e um canal com capacidade para dois blocos; compressão, hashing e escrita rodam em uma tarefa bloqueante dedicada.

O resultado contém:

- bytes de entrada;
- bytes comprimidos;
- SHA-256 da entrada;
- SHA-256 do artefato comprimido;
- duração;
- throughput médio calculável;
- razão de compressão calculável.

O observer de progresso recebe somente bytes lidos e tempo decorrido. A UI pode mostrar tamanho, throughput e duração, mas não um percentual, pois o tamanho total do `mysqldump` não é conhecido.

## Nível inicial

O nível inicial é Zstd 1. Em 5 de setembro de 2026, um ensaio local reproduzível com 64 MiB de SQL sintético e altamente repetitivo mediu:

| Nível | Throughput no build de teste | Tamanho comprimido |
|---|---:|---:|
| 1 | 20,1 MiB/s | 6.241 bytes |
| 3 | 19,9 MiB/s | 6.241 bytes |

Esse ensaio serve apenas para escolher o default inicial: o input sintético comprime muito melhor que um dump real e o build de teste não representa a performance do binário release. O teste ignorado `benchmarks_candidate_zstd_levels_on_sql_like_input` permite repetir a comparação. Antes de produção, a escolha deve ser reavaliada com dumps reais pequenos e sem guardar seu conteúdo como fixture.

## Falhas e lifecycle

Falha de leitura, falha de escrita/encoder e encerramento inesperado da tarefa são categorias diferentes. Se escrita e envio ao canal falham juntos, o erro original do encoder tem precedência, preservando diagnósticos como disco cheio.

Este componente não abre caminhos, publica nem remove arquivos. A RDB-043 é responsável por abrir o writer com `create_new`, usar diretório privado e extensão `.part`, executar `fsync`, gravar metadata e publicar atomicamente. Cancelar o future fecha o canal e faz o worker finalizar o fragmento que já recebeu; esse fragmento só poderá existir em uma área parcial administrada pela camada de artefatos.
