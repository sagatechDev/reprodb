# DumpPolicy MySQL 8

Este documento registra a policy de dump versionada pelo `reprodb`. Ela é deliberadamente conservadora: observar o source não autoriza um dump quando a ferramenta ainda não sabe preservar e restaurar aquele formato com segurança.

## Policy v2

A policy atual aceita somente:

- servidor cujo vendor observado é MySQL;
- servidor MySQL 8.4 e client `mysqldump` aprovado da mesma série;
- database válido, com charset e collation validados;
- tabelas base exclusivamente InnoDB;
- nenhuma view;
- nenhum trigger;
- nenhum stored procedure/function;
- nenhum event;
- nenhum objeto com `DEFINER` visível no preflight.

O bloqueio é deliberado: o Salt local observado hoje usa somente tabelas InnoDB e não possui esses objetos. Aceitá-los sem fixtures de dump/restore e sem uma política explícita de `DEFINER` poderia criar uma cópia local incompleta ou restaurar identidades de autorização do source. O suporte futuro a cada tipo de objeto exige testes próprios e uma nova versão da policy.

O plano aprovado produz, nesta ordem, argumentos separados:

```text
--single-transaction
--quick
--no-tablespaces
--hex-blob
--set-gtid-purged=OFF
--triggers
--skip-routines
--skip-events
--skip-lock-tables
--default-character-set=<charset observado e validado>
<database validado>
```

Eles serão entregues diretamente ao processo. Não existe `sh -c`, interpolação em uma command line ou database arbitrário.

Triggers são explicitados mesmo sendo habilitados por padrão no `mysqldump`. Routines e events são desabilitados explicitamente e sua presença visível no preflight bloqueia o dump. O mesmo vale para views, triggers e qualquer `DEFINER`.

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

As tabelas de `information_schema` respeitam a visibilidade da credencial conectada. Antes de produção, um DBA deve provar que a conta de auditoria enxerga todos os tipos de objeto relevantes; caso contrário, objetos podem ser omitidos do inventário. A conta operacional de dump permanece restrita ao mínimo necessário e não recebe privilégios globais apenas para contornar essa limitação.

O inventário do Salt local, a matriz de grants e o gate necessário para produção estão em [Salt — matriz de schemas, objetos e grants](research/salt-schema-object-matrix-2026-09-07.md).

## Referências

- [MySQL 8.4 — mysqldump](https://dev.mysql.com/doc/refman/8.4/en/mysqldump.html)
- [MySQL 8.4 — stored object access control](https://dev.mysql.com/doc/refman/8.4/en/stored-objects-security.html)
