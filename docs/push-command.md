# `reprodb push` — contrato operacional

`reprodb push DATABASE` importa um dump gerenciado em um database de **outro profile**, por exemplo levar um dump de produção para um sandbox, sem passar pelo MySQL local.

```bash
reprodb profile allow-push sandbox                          # opt-in, uma vez por profile
reprodb push salt_sagatec                                   # pergunta profile, dump e nome
reprodb push salt_sagatec --profile sandbox --fresh --yes   # dump novo do profile ativo
reprodb push salt_sagatec --profile sandbox \
  --dump-id 550e8400-e29b-41d4-a716-446655440000 \
  --database salt_sagatec_qa --yes
reprodb profile deny-push sandbox                           # revoga
```

## Quem pode receber um push

Nenhum profile aceita push por padrão. O destino precisa passar por todas as travas abaixo, nesta ordem, antes de qualquer SQL de escrita:

1. o profile existe e **não** é `production = true` (esses nunca podem ser liberados);
2. foi liberado explicitamente com `reprodb profile allow-push NAME` (`profile list` mostra `accepts push`);
3. nenhum outro profile **não liberado** usa o mesmo host:porta (`localhost`, `127.x`, `::1` e `host.docker.internal` contam como o mesmo host);
4. conecta com a credencial e o TLS do próprio profile e lê `@@server_uuid`; se algum dump em cache de um profile não liberado (ou removido) veio desse servidor, recusa, seja qual for o hostname;
5. o servidor de origem do dump só é aceito se o profile de origem também estiver liberado, e mesmo assim nunca o próprio database de origem;
6. série/versão compatíveis (mesma regra do restore local: sem downgrade).

Cada sessão que escreve (`CREATE DATABASE` e o import) começa com uma checagem de `@@server_uuid` contra o servidor validado. Se o host passar a apontar para outro servidor entre a validação e a escrita, o `mysql` aborta na linha 1 e nada é escrito.

## Fluxo

```text
profile destino (só liberados) -> travas 1-4
dump: cache existente ou "gerar dump novo agora" (profile ativo como origem)
    -> validar metadata, tamanho, Zstd e dois SHA-256
nome do database remoto (default: o da origem) -> travas 5-6
plano + digitar `profile/database` para confirmar (--yes pula)
    -> CREATE DATABASE IF NOT EXISTS (charset/collation do dump)
    -> Zstd -> stdin do mysql (--binary-mode)
    -> conferir bytes importados
```

## Semântica de escrita

- Não há `DROP DATABASE`. O dump recria cada tabela que contém (`DROP TABLE IF EXISTS` + `CREATE TABLE`); tabelas que só existem no destino continuam lá.
- Sem terminal, profile e confirmação precisam vir de `--profile` e `--yes`, e o dump de `--dump-id` ou `--fresh`; sem `--yes` o comando para antes de qualquer dump.
- O usuário do profile destino precisa de `CREATE`, `DROP`, `INSERT`, `ALTER`, `INDEX`, `REFERENCES` e `TRIGGER` no database.

## Falhas

Falha ou `Ctrl+C` no meio do import deixa o database remoto parcialmente importado; rodar o mesmo comando importa por cima de novo. Recusas das travas saem com `10`, destino inacessível ou servidor trocado com `30`, versão incompatível, falta de privilégio ou erro de import com `70`. Ver [`exit-codes.md`](exit-codes.md).
