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

O observer de progresso recebe bytes lidos e tempo decorrido, além de uma estimativa opcional informada antes do stream. A estimativa vem da soma de `information_schema.tables.data_length` observada no preflight; ela descreve páginas/dados estimados pelo MySQL, não o tamanho exato do SQL textual.

A UI mostra tamanho, throughput e duração. Depois de dois segundos de amostragem, pode acrescentar `ETA ~mm:ss`, recalculado pela vazão desde o início. O `~` é deliberado: escaping, BLOBs, statements e estatísticas do InnoDB fazem o stream divergir da estimativa. Não existe percentual. Se a estimativa for zero, a amostra ainda for insuficiente ou o SQL já tiver ultrapassado o total estimado, o ETA é omitido em vez de apresentar um prazo enganoso.

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
