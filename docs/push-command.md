# `reprodb push` — contrato operacional

`reprodb push DATABASE` importa um dump gerenciado em um database de **outro profile**, por exemplo levar um dump de produção para um sandbox, sem passar pelo MySQL local.

```bash
reprodb push salt_sagatec        # lista e pergunta tudo: destino, dump, nome, downgrade, confirmação
```

No terminal nada precisa de flag. As flags só existem como atalho e para automação sem terminal:

```bash
reprodb push salt_sagatec --profile sandbox --fresh --database salt_qa --yes [--allow-downgrade]
reprodb profile allow-push sandbox   # libera sem passar pelo menu
reprodb profile deny-push sandbox    # revoga
```

## Quem pode receber um push

O menu de destino lista os profiles não-produção. Os já liberados vêm primeiro; os outros aparecem marcados e, ao escolher um, a CLI pergunta se pode liberá-lo. A resposta só é gravada se o push for confirmado no final. O profile ativo (origem dos dumps novos) não aparece, a menos que tenha sido liberado de propósito.

O destino precisa passar por todas as travas abaixo, nesta ordem, antes de qualquer SQL de escrita:

1. o profile existe e **não** é `production = true` (esses nunca podem ser liberados);
2. foi liberado (pelo menu ou `reprodb profile allow-push NAME`; `profile list` mostra `accepts push`);
3. nenhum outro profile **não liberado** usa o mesmo host:porta (`localhost`, `127.x`, `::1` e `host.docker.internal` contam como o mesmo host);
4. conecta com a credencial e o TLS do próprio profile e lê `@@server_uuid`; se algum dump em cache de um profile não liberado (ou removido) veio desse servidor, recusa, seja qual for o hostname;
5. o servidor de origem do dump só é aceito se o profile de origem também estiver liberado, e mesmo assim nunca o próprio database de origem;
6. versão: mesma série ou mais nova passa direto; série mais antiga da mesma major (ex.: dump 8.4 → sandbox 8.0) pergunta se pode importar mesmo assim (`--allow-downgrade` sem terminal); major diferente é recusada.

Cada sessão que escreve (`CREATE DATABASE` e o import) começa com uma checagem de `@@server_uuid` contra o servidor validado. Se o host passar a apontar para outro servidor entre a validação e a escrita, o `mysql` aborta na linha 1 e nada é escrito.

## Fluxo

```text
profile destino (só liberados) -> travas 1-4
dump: cache existente ou "gerar dump novo agora" (profile ativo como origem)
    -> validar metadata, tamanho, Zstd e dois SHA-256
nome do database remoto (default: o da origem) -> travas 5-6
downgrade? pergunta
plano + digitar `profile/database` para confirmar (--yes pula)
    -> grava a liberação do profile, se foi pedida agora
    -> CREATE DATABASE IF NOT EXISTS (charset/collation do dump)
    -> Zstd -> stdin do mysql (--binary-mode)
    -> conferir bytes importados
```

## Semântica de escrita

- Não há `DROP DATABASE`. O dump recria cada tabela que contém (`DROP TABLE IF EXISTS` + `CREATE TABLE`); tabelas que só existem no destino continuam lá.
- Sem terminal, profile e confirmação precisam vir de `--profile` e `--yes`, e o dump de `--dump-id` ou `--fresh`; sem `--yes` o comando para antes de qualquer dump.
- Downgrade importa tabelas e dados normalmente; recursos que só existem na série mais nova podem falhar no import.
- O usuário do profile destino precisa de `CREATE`, `DROP`, `INSERT`, `ALTER`, `INDEX`, `REFERENCES` e `TRIGGER` no database.

## Falhas

Falha ou `Ctrl+C` no meio do import deixa o database remoto parcialmente importado; rodar o mesmo comando importa por cima de novo. Recusas das travas saem com `10`, destino inacessível ou servidor trocado com `30`, versão incompatível, falta de privilégio ou erro de import com `70`. Ver [`exit-codes.md`](exit-codes.md).
