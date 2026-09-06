# DumpPolicy MySQL 8

Este documento registra a policy de dump versionada pelo `reprodb`. Ela é deliberadamente conservadora: observar o source não autoriza um dump quando a ferramenta ainda não sabe preservar e restaurar aquele formato com segurança.

## Policy v1

A primeira policy aceita somente:

- servidor cujo vendor observado é MySQL;
- servidor MySQL 8.4 e client `mysqldump` aprovado da mesma série;
- database válido, com charset e collation validados;
- tabelas base exclusivamente InnoDB;
- nenhum stored procedure/function;
- nenhum event.

Views e triggers são permitidos e contados. A quantidade de objetos que possuem `DEFINER` gera um aviso estruturado no plano. A futura etapa de restore deverá verificar os privilégios do target e provar o comportamento desses definers antes de importar o artefato.

O plano aprovado produz, nesta ordem, argumentos separados:

```text
--single-transaction
--quick
--no-tablespaces
--hex-blob
--set-gtid-purged=OFF
--triggers
--skip-lock-tables
--default-character-set=<charset observado e validado>
<database validado>
```

Eles serão entregues diretamente ao processo. Não existe `sh -c`, interpolação em uma command line ou database arbitrário.

Triggers são explicitados mesmo sendo habilitados por padrão no `mysqldump`. Routines e events não são silenciosamente ignorados: quando visíveis no preflight, sua presença bloqueia o dump até existir uma policy de restore correspondente.

## Preflight somente leitura

O preflight consulta apenas agregados do database resolvido:

- charset e collation;
- engine e quantidade de tabelas por engine;
- quantidades de views, triggers, routines e events;
- quantidades de objetos com definer;
- modo GTID.

Nomes de tabelas, nomes de objetos e valores de `DEFINER` não retornam ao processo. A query é fixa; o database entra como argumento estruturado e previamente validado do client MySQL.

O resultado só se torna um `ApprovedDumpPlan` depois de passar pela policy. Código futuro de dump deve exigir esse tipo e não aceitar uma lista de flags montada pelo handler da CLI.

## Limites que permanecem reais

`--single-transaction` mantém uma visão consistente para InnoDB, mas DDL concorrente (`ALTER`, `CREATE`, `DROP`, `RENAME` ou `TRUNCATE TABLE`) pode invalidar ou fazer o dump falhar. O plano sempre carrega o aviso `ConcurrentDdlMustBePrevented`; a habilitação em produção precisa definir uma janela/processo operacional para isso.

Existe uma janela inevitável entre observar metadata e iniciar o dump. O engine deve iniciar o processo imediatamente após o preflight e nunca reutilizar indefinidamente uma aprovação antiga.

As tabelas de `information_schema` respeitam a visibilidade da credencial conectada. Antes de produção, a auditoria de privilégios deve provar que a conta enxerga todos os tipos de objeto relevantes; caso contrário, routines/events podem ser omitidos do inventário. Isso é um bloqueio conhecido para o hardening de produção, não uma garantia já resolvida pela v1.

## Referências

- [MySQL 8.4 — mysqldump](https://dev.mysql.com/doc/refman/8.4/en/mysqldump.html)
- [MySQL 8.4 — stored object access control](https://dev.mysql.com/doc/refman/8.4/en/stored-objects-security.html)

