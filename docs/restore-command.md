# `reprodb restore` — contrato operacional

`reprodb restore TENANT --dump-id ID` restaura um artefato gerenciado no target Docker configurado e atualiza o registro mínimo do tenant no `salt_central` local.

Exemplo com o Salt local:

```bash
reprodb setup
reprodb dump sagatec
reprodb restore sagatec --dump-id 550e8400-e29b-41d4-a716-446655440000
```

O comando não aceita path de arquivo, host, container ou nome de database. O UUID precisa identificar exatamente um dump completo dentro de `~/.reprodb/cache`.

## Fluxo

```text
tenant + dump ID
    -> localizar UUID no cache gerenciado
    -> validar metadata, identidade, tamanho, Zstd e dois SHA-256
    -> construir o registro local sanitizado
    -> atestar context, container, credencial e versão do target
    -> autorizar o database pelo prefixo local
    -> mostrar source, target e aviso destrutivo
    -> marcar estado incomplete
    -> DROP/CREATE do database local
    -> Zstd -> stdin do mysql
    -> marcar estado ready
    -> upsert do tenant/domain no salt_central local
```

O source MySQL não é consultado. Profile, tenant canônico, database, versões e features locais vêm da metadata validada no momento do dump. Por isso um restore explícito continua possível quando o source está offline e não depende do profile atualmente ativo.

O `TENANT` precisa coincidir com o lookup usado para criar o dump ou com seu tenant ID canônico. Informar `polymer` para um dump de `sagatec` falha antes de ler credenciais, consultar Docker ou alterar MySQL. UUID ausente ou duplicado também é recusado.

## UX

Depois das validações não destrutivas, a CLI apresenta um plano semelhante a:

```text
Restore plan
  Source:     salt-local
  Lookup:     sagatec
  Tenant ID:  salt_sagatec
  Database:   salt_sagatec
  Dump ID:    <uuid>
  MySQL:      8.4.4 (client 8.4.4)
  Target:     mysql-8/salt_sagatec
  Domain:     sagatec

! The configured local tenant database will be replaced.
```

Não existe confirmation prompt: executar `restore` com um UUID gerenciado já expressa a intenção. A barreira efetiva é estrutural e limita o `DROP` ao database do artefato dentro do target selecionado pelo `setup`.

O resultado mostra database, container, domain, bytes importados e dump ID. A CLI não inventa uma URL: o reprodb não conhece a porta HTTP nem o `APP_CENTRAL_DOMAIN` usados pela instância local do Salt.

## Falhas e retry

- corrupção, identidade divergente e target inválido falham antes do `DROP`;
- falha durante recreate/import deixa o estado local como `incomplete`;
- falha no registro central informa explicitamente que o database foi restaurado;
- repetir exatamente o mesmo comando recria o database e executa o upsert idempotente sem acessar o source;
- o artefato completo e sua lease são preservados durante a operação.

Um dump escolhido explicitamente por ID não depende do TTL do cache. A limpeza futura continua podendo remover artefatos expirados quando eles não estiverem em uso.

## Evidências

Os testes unitários cobrem localização global segura pelo UUID, ID duplicado, tenant divergente, ordem do workflow, escopo do plano visual e categorias de exit code. O teste integrado executa o workflow duas vezes contra o `mysql-8`, compara os dados restaurados e confirma o upsert local idempotente:

```bash
cargo test --test restore_integration -- --ignored --nocapture
```

O tratamento completo de `Ctrl+C` e supervisão explícita dos processos continua pertencendo à RDB-060.
