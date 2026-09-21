# Matriz de objetos e privilégios do Salt — 2026-09-07

> **Histórico.** Pesquisa datada, anterior à remoção do conceito de tenant. Mantida como registro.

> Levantamento local para a RDB-072. Nenhuma conexão com produção foi feita e nenhum dado de negócio foi lido ou persistido.

## Evidência observada

O container local `mysql-8` usa MySQL `8.4.4`, `GTID_MODE=OFF` e binary log ativo. A consulta agregada em `information_schema` encontrou 12 schemas `salt_*`, somando 5.350 tabelas:

| Schema | Tabelas | Engines | Views | Triggers | Routines | Events |
|---|---:|---|---:|---:|---:|---:|
| `acme_production` | 519 | InnoDB | 0 | 0 | 0 | 0 |
| `globex_production` | 504 | InnoDB | 0 | 0 | 0 | 0 |
| Demais 10 schemas locais | 4.327 | InnoDB | 0 | 0 | 0 | 0 |
| **Total** | **5.350** | **100% InnoDB** | **0** | **0** | **0** | **0** |

Também foi feita busca estática nas cinco cópias locais do Salt (`Salt`, `Salt-02`, `Salt-clean`, `Salt-ct` e `Salt-develop-cleanup`). Não foram encontrados migrations ou SQL criando `VIEW`, `TRIGGER`, `PROCEDURE`, `FUNCTION` ou `EVENT`. Isso é evidência do ambiente de desenvolvimento, não prova sobre produção: SQL montado dinamicamente ou estado criado fora das migrations pode não aparecer na busca.

## Decisão da dump policy v2

O conjunto observado que o reprodb suporta é:

- tabelas InnoDB, inclusive PK, FK e índices;
- `NULL`, `DECIMAL`, `DATETIME`, UTF-8/emoji, `BLOB` e textos grandes;
- GTID `OFF` no ambiente local; o dump sempre usa `--set-gtid-purged=OFF`;
- nenhuma view, trigger, routine, event ou identidade `DEFINER`.

A policy v2 falha antes de `mysqldump` se encontrar tabela não-InnoDB ou qualquer stored object/`DEFINER`. Routines e events também são desabilitados explicitamente com `--skip-routines` e `--skip-events`. Essa decisão evita restaurar identidades de autorização do source ou produzir uma cópia silenciosamente incompleta. Um suporte futuro a qualquer desses objetos exige fixture de dump/restore, política de `DEFINER` e nova versão da policy.

A mudança de v1 para v2 invalida automaticamente cache anterior; um `pull` não restaura um artefato criado sob a política antiga.

## Privilégios mínimos do source

Para o conjunto atualmente suportado, a credencial precisa de:

| Escopo | Privilégios | Motivo |
|---|---|---|
| `salt_central.*` | `SELECT`, `SHOW VIEW`, `TRIGGER` | resolver tenant e tornar o inventário de objetos visível |
| tenant selecionado | `SELECT`, `SHOW VIEW`, `TRIGGER` | ler tabelas e detectar objetos que a policy deve bloquear |

Exemplo para um tenant piloto, executado por um DBA e não pelo reprodb:

```sql
GRANT SELECT, SHOW VIEW, TRIGGER ON `salt_central`.* TO 'reprodb_reader'@'%';
GRANT SELECT, SHOW VIEW, TRIGGER ON `acme_production`.* TO 'reprodb_reader'@'%';
```

Não são necessários para a combinação atual:

- `LOCK TABLES`, pois usamos `--single-transaction` e aceitamos somente InnoDB;
- `PROCESS`, pois usamos `--no-tablespaces`;
- `RELOAD`/`FLUSH_TABLES`, pois fixamos `--set-gtid-purged=OFF`;
- `FILE`, `INSERT`, `UPDATE`, `DELETE`, `CREATE`, `DROP`, `ALTER`, `SUPER` ou `GRANT OPTION`;
- `EVENT` ou privilégios de routine, pois esses objetos são deliberadamente incompatíveis com a policy v2.

O manual do MySQL documenta a relação entre `SELECT`, `SHOW VIEW`, `TRIGGER`, `LOCK TABLES`, `PROCESS`, GTID e as opções do `mysqldump`: [mysqldump — privileges](https://dev.mysql.com/doc/refman/8.4/en/mysqldump.html). Também documenta que triggers são incluídos por default enquanto routines/events exigem opções próprias: [Dumping Stored Programs](https://dev.mysql.com/doc/refman/8.4/en/mysqldump-stored-programs.html).

## Prova automatizada

O E2E Docker cria source e target MySQL 8.4 isolados. O setup do source usa `root` apenas para criar fixtures e a conta de teste. O `PullService`, o resolver, o preflight e o `mysqldump` recebem somente `reprodb_reader`, com os grants acima.

A fixture do tenant contém duas tabelas InnoDB relacionadas por FK e cobre bigint, decimal, datetime, BLOB, `NULL`, texto e UTF-8/emoji. Depois do restore, o teste compara valores e confirma a FK no target. Em seguida remove a credencial source e repete o `pull` por cache.

```bash
cargo test --test pull_integration \
  pulls_a_real_tenant_then_reuses_cache_without_the_source_credential \
  -- --ignored --nocapture
```

## Gate ainda externo para o piloto

Antes da RDB-073, o responsável pelo banco deve repetir o inventário no tenant real e confirmar que a credencial enxerga todo o schema. `information_schema` respeita privilégios e pode ocultar objetos de uma conta sem acesso; por isso, o zero observado pela CLI não substitui a auditoria do DBA para routines/events. Se produção divergir da matriz, o piloto para antes do dump e a policy não deve ser relaxada sem testes específicos.
