# Comandos de cache local

Todo o estado local do reprodb continua sob `~/.reprodb`. Os dumps gerenciados ficam em:

```text
~/.reprodb/cache/profiles/<profile>/<tenant-id>/<dump-id>/
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
- `expired`: conteúdo íntegro, mas fora do TTL de duas horas;
- `in use`: outra operação mantém a lease e a inspeção não interfere nela;
- `profile/source/policy changed`: íntegro, porém incompatível com a configuração atual;
- `bad metadata`, `identity`, `size` ou `checksum mismatch`: artefato inconsistente;
- `future clock`: timestamp posterior ao relógio local.

A verificação de integridade lê todo o `.sql.zst` para calcular SHA-256. Portanto, `cache list` pode levar tempo proporcional ao volume armazenado, mas não descomprime o SQL nem carrega o dump inteiro em memória.

## Limpar por TTL

```bash
reprodb cache clean
```

Remove dumps expirados, partials abandonados há pelo menos uma hora e deleções interrompidas. Metadata inválida e timestamps futuros são mantidos para inspeção; uma entrada locked também é preservada. O relatório diferencia tudo que foi removido e tudo que ficou por segurança.

## Remover um tenant

```bash
reprodb profile use salt-local
reprodb cache purge sagatec
```

`purge` atua somente no profile ativo. O tenant pode ser o lookup original (`sagatec`) ou o ID canônico (`salt_sagatec`), e todos os dumps completos correspondentes são removidos. Entradas em uso permanecem no disco e aparecem no relatório.

Para um alias, metadata ilegível não é removida porque a CLI não consegue provar a qual tenant ela pertence. Informar o tenant ID canônico permite selecionar com segurança pelo diretório tipado. A remoção faz rename atômico para um nome isolado antes de apagar, sem seguir symlinks.

Nenhum desses comandos remove configuração ou senha. `purge` também não altera o database restaurado no `mysql-8`; ele afeta somente os dumps locais.
