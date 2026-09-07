# `reprodb dump` — contrato operacional

`reprodb dump TENANT` cria um dump lógico comprimido no cache local e não executa restore.

O cache fica em `~/.reprodb/cache`. Um resultado para o profile `local` e tenant `salt_sagatec` é publicado em:

```text
~/.reprodb/cache/profiles/local/salt_sagatec/<dump-id>/
```

Exemplo com os nomes usados no Salt:

```bash
reprodb profile use salt-local
reprodb doctor
reprodb dump sagatec
```

O alias `sagatec` é resolvido pelo `salt_central` para o tenant e database canônicos, por exemplo `salt_sagatec`. O input do terminal nunca é usado diretamente como identificador SQL.

## Fluxo

```text
config/profile
    -> credential store
    -> tenant resolver
    -> lock local por profile+database
    -> preflight do source
    -> mysqldump em client Docker aprovado
    -> compressão Zstandard streaming
    -> metadata e checksums
    -> publicação atômica no cache
```

O comando mostra bytes processados, throughput e duração quando executado em terminal. Depois de uma amostra mínima, acrescenta `ETA ~mm:ss` calculado a partir de `information_schema.tables.data_length` e da vazão real do stream. O sinal `~` deixa explícito que a previsão é aproximada; não há percentual porque o tamanho do SQL não é conhecido exatamente.

Ao concluir, a saída informa o profile, tenant, database, versões do source e client, tamanhos, duração, ID e caminho do artefato. Esse ID é a entrada do [`reprodb restore`](restore-command.md).

## Segurança e integridade

- O client é uma imagem MySQL aprovada pelo catálogo local e fixada por digest.
- A senha fica em um option file temporário com permissões restritas, montado read-only no container; ela não aparece no argv, metadata ou logs.
- Os processos são criados com argumentos estruturados, sem `sh -c` ou interpolação de shell.
- O SQL segue de `mysqldump stdout` para Zstandard e nunca é persistido cru.
- O artefato nasce em um diretório UUID com sufixo `.part` e só fica visível depois de checksums, metadata, `fsync` e rename atômico.
- Falha normal remove o staging. Dumps completos anteriores são preservados.
- Um lock advisory impede dois dumps do mesmo `profile + database` nesta máquina.
- Um profile de produção exige `VERIFY_IDENTITY`, mostra um aviso vermelho antes de acessar a credencial e passa pelo mesmo preflight conservador.
- Não existe saída direta para arquivo arbitrário ou stdout: inclusive em produção, o sucesso exige publicação atômica no cache gerenciado.

O processo usa uma transação consistente para InnoDB, mas ainda gera leitura, I/O e tráfego de rede. Migrações ou outras alterações de schema devem ser evitadas durante a execução.

## Opções de dump aprovadas

Para MySQL 8, o plano atual inclui as opções conservadoras definidas pela dump policy, incluindo:

```text
--single-transaction
--quick
--skip-lock-tables
--no-tablespaces
--hex-blob
--set-gtid-purged=OFF
--triggers
--skip-routines
--skip-events
```

O preflight valida versão/vendor, charset/collation, engines e objetos relevantes, além de coletar a estimativa lógica usada no ETA, antes de criar o staging. A policy v2 aceita somente tabelas InnoDB e bloqueia views, triggers, routines, events e qualquer `DEFINER` visível. Uma policy incompatível encerra o comando antes do dump.

Essa restrição corresponde ao inventário atual dos schemas Salt locais. Suportar um desses objetos futuramente exige fixtures de dump/restore, uma decisão explícita sobre `DEFINER` e nova versão da policy; consulte a [matriz de schemas, objetos e grants](research/salt-schema-object-matrix-2026-09-07.md).

Não há uma implementação alternativa para produção. A classificação ativa políticas adicionais, enquanto resolução, preflight, argumentos estruturados, streaming e publicação permanecem o mesmo pipeline exercitado localmente.

## Falhas e limitações desta etapa

Erros são classificados em configuração, credencial, dependência, conexão source, resolução do tenant, dump, cache ou Docker, cada categoria com exit code estável. O stderr bruto do `mysqldump` não é repetido para evitar vazar dados retornados pelo client.

`Ctrl+C` cancela cooperativamente a compressão, encerra e aguarda o `docker run`, remove explicitamente o client container efêmero e descarta o `.part`. Um dump completo anterior continua disponível e o processo termina com código 130.

O cache criado já possui o formato, checksums e TTL necessários, mas o consumo automático por `pull` e os comandos de inspeção/limpeza entram nas próximas issues.
