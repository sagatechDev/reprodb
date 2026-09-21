# Comandos de cache local

Todo o estado local do reprodb continua sob `~/.reprodb`. Os dumps gerenciados ficam em:

```text
~/.reprodb/cache/profiles/<profile>/<database>/<dump-id>/
├── dump.sql.zst
├── metadata.json
└── .artifact.lock
```

`REPRODB_HOME` pode apontar para uma raiz absoluta diferente em testes e automações. Os comandos de cache não acessam MySQL, Docker nem o credential store.

## Listar e verificar

```bash
reprodb cache list
```

A listagem percorre todos os profiles presentes no cache e mostra lookup, database, profile, idade, expiração, tamanho e UUID. Cada diretório de artefato gerenciado recebe uma classificação explícita, inclusive quando falta um dos arquivos esperados:

- `ready`: metadata, identidade, tamanho, SHA-256, profile, policy e TTL válidos;
- `stale` / `prunable`: conteúdo íntegro, fora do TTL de frescor ou da retenção (ver a tabela adiante);
- `in use`: outra operação mantém a lease e a inspeção não interfere nela;
- `profile/source/policy changed`: íntegro, porém incompatível com a configuração atual;
- `bad metadata`, `identity`, `size` ou `checksum mismatch`: artefato inconsistente;
- `future clock`: timestamp posterior ao relógio local.

A verificação de integridade lê todo o `.sql.zst` para calcular SHA-256. Portanto, `cache list` pode levar tempo proporcional ao volume armazenado, mas não descomprime o SQL nem carrega o dump inteiro em memória.

## Estados de um dump

O cache separa duas perguntas que antes compartilhavam um único TTL:

| Estado | Significado |
|---|---|
| `fresh` | Dentro do TTL de frescor de uma hora; `pull` reutiliza o dump. |
| `stale` | Passou do TTL de frescor. `pull` cria um dump novo, mas `restore` continua aceitando este. |
| `prunable` | Passou da retenção de sete dias; `cache prune` sem flags o remove. |

Um dump `stale` nunca é apagado implicitamente. `dump` e `pull` varrem apenas partials abandonados e deleções interrompidas — lixo que ninguém consegue restaurar.

## Remover dumps

```bash
reprodb cache prune                          # retenção default: 7 dias
reprodb cache prune --older-than 24h         # também aceita 90m, 3d, 3600
reprodb cache prune --keep-last 2
reprodb cache prune --all
reprodb cache prune acme_production --all
```

`prune` é o único comando que destrói dumps completos. Sem flags, remove o que passou da retenção default. As três flags são mutuamente exclusivas.

`--keep-last N` conta por par `(profile, database)`: com três databases, `--keep-last 1` mantém três dumps, um de cada.

O argumento `DATABASE` filtra pelo nome do database em **todos os profiles**, do mesmo modo que `restore` localiza um dump. Um dev pedindo o disco de `acme_production` de volta não quer cópias sobrando sob outro profile.

Antes de remover, o comando lista o que será destruído com o total a recuperar e pede confirmação. `--yes` pula o prompt; sem terminal e sem `--yes`, o comando falha em vez de adivinhar.

Um artefato com lease ativa nunca é removido: ele aparece no relatório como mantido, de modo que um restore em andamento não perde o dump debaixo dele. Metadata inválida e timestamps futuros também são preservados e contabilizados — apagá-los esconderia um problema. A remoção faz rename atômico para um nome isolado antes de apagar, sem seguir symlinks.

Nenhum desses comandos remove configuração ou senha, e `prune` não altera o database restaurado no `mysql-8`; ele afeta somente os dumps locais.
