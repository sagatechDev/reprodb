# Listagem de tenants

`reprodb tenant list` consulta o catálogo do profile ativo para que o desenvolvedor descubra o tenant antes de executar `pull`:

```bash
reprodb tenant list
reprodb tenant list --limit 200
```

O limite padrão é 100 e o máximo é 500. Quando há mais registros, a saída informa que foi truncada.

## Banco central

A consulta usa exclusivamente o database configurado no próprio source profile. Para corrigi-lo sem recriar o profile ou tocar na credencial:

```bash
reprodb profile edit sandbox --central-database outro_central
```

O comando `profile edit` é local: modifica apenas `~/.reprodb/reprodb.toml`. Já `tenant list` abre uma conexão com o source e requer Docker, a imagem de client aprovada e a credencial existente no store do sistema operacional.

## Contrato de somente leitura

`tenant list` executa uma única instrução `SELECT`, sem transação de escrita e sem `INSERT`, `UPDATE`, `DELETE`, DDL ou mudança de sessão. A resposta transporta apenas campos permitidos:

- ID do tenant;
- nome efetivo do database, incluindo um `tenancy_db_name` válido quando existir;
- um domínio primário, quando existir.

O JSON completo de `tenants.data`, passwords e overrides de conexão não são retornados ao processo, exibidos ou registrados. Identificadores e metadata inválidos fazem o comando falhar em vez de produzir uma sugestão insegura para `pull`.

O comando não altera automaticamente a configuração caso o database central esteja incorreto. Primeiro ajuste explicitamente o profile e depois repita a listagem.
