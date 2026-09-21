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

O nível inicial é Zstd 1. Em 7 de setembro de 2026, o ensaio sintético em build release mediu:

| Nível | Throughput no build de teste | Tamanho comprimido |
|---|---:|---:|
| 1 | 313,2 MiB/s | 6.241 bytes |
| 3 | 308,7 MiB/s | 6.241 bytes |

O input acima comprime muito melhor que um dump real. Por isso, a decisão também foi medida no [benchmark MySQL](performance-benchmark.md) com 244.138.406 bytes de SQL e 230.399.010 bytes de payload pseudoaleatório. Nessa massa, nível 1 gerou 120.904.230 bytes em 6,924 s; nível 3 gerou 125.268.601 bytes em 9,171 s. O nível 1 permanece como default. Antes de produção, a escolha ainda deve ser confirmada com um database real e autorizado, sem guardar seu conteúdo como fixture.

## Falhas e lifecycle

Falha de leitura, falha de escrita/encoder e encerramento inesperado da tarefa são categorias diferentes. Se escrita e envio ao canal falham juntos, o erro original do encoder tem precedência, preservando diagnósticos como disco cheio.

Este componente não abre caminhos, publica nem remove arquivos. A RDB-043 é responsável por abrir o writer com `create_new`, usar diretório privado e extensão `.part`, executar `fsync`, gravar metadata e publicar atomicamente. Cancelar o future fecha o canal e faz o worker finalizar o fragmento que já recebeu; esse fragmento só poderá existir em uma área parcial administrada pela camada de artefatos.
