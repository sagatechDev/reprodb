# Resolução de tenants

O reprodb separa três conceitos que não devem ser confundidos:

```text
lookup digitado → tenant ID canônico → database validado
sagatec         → salt_sagatec       → salt_sagatec
polymer         → salt_polymer       → salt_polymer
```

Todos são value objects. Nenhuma entrada recebida pela CLI vira identificador SQL diretamente.

## Resolver por pattern

O resolver simples aceita exatamente um placeholder `{tenant}`:

```toml
[profiles.example.tenant_resolver]
type = "pattern"
pattern = "salt_{tenant}"
```

Prefixo e sufixo só podem conter letras/dígitos ASCII e `_`. O pattern é compilado durante a validação da configuração; placeholder ausente/repetido, traversal, metacaracteres ou controles impedem a persistência. Depois da substituição, o resultado passa novamente por `DatabaseName`, incluindo limite de 64 caracteres e bloqueio de `mysql`, `information_schema`, `performance_schema` e `sys`.

## Resolver do Salt Central

Profiles criados pelo fluxo atual usam `salt-central`. O resolver procura correspondência binária exata em:

1. `salt_central.tenants.id`;
2. `salt_central.domains.domain`, quando `allow_domain_lookup = true`.

Um match por domain retorna o tenant ID canônico. O database é `data.tenancy_db_name` quando existe e é uma string válida; caso contrário, usa o tenant ID, reproduzindo a regra observada no Salt.

A query é fixa e roda pelo client MySQL Docker aprovado. O lookup validado entra como literal hexadecimal, o database central entra como argumento estruturado e nenhuma shell é usada. A saída do MySQL contém somente:

- tenant ID em hex;
- estado tipado do override;
- nome do database em hex;
- origem do match (`tenant ID` ou `domain`).

O JSON completo de `tenants.data` nunca sai do MySQL. JSON inválido, override com tipo incorreto, metadata malformada, múltiplos matches ou database administrativo bloqueiam a resolução sem repetir a linha recebida em erro/debug.

Overrides de host, porta, usuário ou connection incompatíveis com o profile bloqueiam a resolução. A comparação ocorre dentro do MySQL: os valores do profile entram em hex e a query devolve somente um booleano. A presença de `tenancy_db_password` também bloqueia, pois a CLI deliberadamente não lê nem compara esse secret. Nenhum valor de override retorna ao processo.

## Validação local real

O teste abaixo usa o `salt_central` observado no container `mysql-8` e verifica os aliases reais `sagatec` e `polymer`:

```bash
cargo test --test salt_central_resolver_integration -- --ignored --nocapture
```

É possível escolher outro container de fixture com `REPRODB_TEST_MYSQL_CONTAINER`.
