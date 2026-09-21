# Validade do cache local

Um diretório publicado só é considerado cache hit depois de passar por todas as validações abaixo. O validator procura os artefatos mais novos primeiro e pode reutilizar um artefato anterior se um candidato mais novo estiver corrompido ou for incompatível com a consulta atual.

## Chave e identidade

A busca parte de `profile + nome do database`. A identidade do diretório precisa coincidir com a metadata, que ainda precisa conter:

- dump ID do diretório;
- profile solicitado;
- database resolvido;
- fingerprint atual do source;
- versão atual da política de dump.

O fingerprint SHA-256 usa campos canônicos e separados por tamanho: nome do profile, host, porta, username, família e série MySQL, modo TLS, imagem imutável do client, e flag de produção. A `credential_key` e a senha ficam fora: rotacionar uma credencial sem trocar o source não invalida um dump.

## Tempo

O TTL de frescor começa em `completed_at`, não no início da exportação. O default é uma hora e o limite é exclusivo: ao atingir exatamente `completed_at + TTL`, o artefato deixa de ser candidato para o `pull`.

Esse TTL responde apenas "o dado está recente?". Ele nunca autoriza remover o artefato: um dump fora do prazo de frescor continua restaurável por `restore` e só é destruído por `cache prune`, que usa a retenção de sete dias descrita em [`cache-commands.md`](cache-commands.md).

Metadata concluída no futuro em relação ao relógio local é rejeitada. Uma soma que ultrapasse o limite de `u64` satura sem transformar um artefato válido em expirado.

`--fresh` encerra a consulta antes de acessar o filesystem e sempre produz um miss explícito. Ele não apaga dumps existentes.

## Integridade

Antes de retornar um hit, o validator:

1. lê no máximo 64 KiB de metadata JSON;
2. desserializa campos estritos e value objects novamente;
3. confere o tamanho real de `dump.sql.zst`;
4. lê o arquivo em blocos de 64 KiB e compara seu SHA-256 com a metadata.

Essa leitura completa adiciona I/O proporcional ao tamanho do artefato em todo cache hit. É uma escolha deliberada nesta fase: restore não pode receber um dump cuja integridade foi apenas presumida. Se a medição com dumps reais mostrar custo relevante, a otimização precisa preservar a mesma garantia ou unir esta leitura à validação Zstd anterior ao restore.

Arquivo removido durante a consulta vira miss, não falha geral do comando. Outros erros reais de I/O, como falta de permissão, continuam sendo reportados; tratá-los como miss esconderia uma configuração quebrada.

## Motivos de miss

O domínio diferencia: `fresh`, ausente, em uso, metadata corrompida, identidade alterada, source alterado, política alterada, relógio futuro, expirado, tamanho divergente e checksum divergente. A CLI transforma esses motivos em mensagens estáveis sem depender do texto de erros internos.

Um hit mantém uma lease compartilhada desde a leitura da metadata/checksum até o consumidor descartá-lo, impedindo que o cleanup remova o arquivo antes do restore. Os locks e a remoção segura estão detalhados em [`cache-locks-cleanup.md`](cache-locks-cleanup.md).

O cache continua estritamente local. Não há coordenação ou lock entre máquinas.
