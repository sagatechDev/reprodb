# Barreira de segurança do target local

Nenhuma operação destrutiva recebe host, porta ou `SourceProfile`. O `RestoreEngine` aceita somente um `AuthorizedLocalTarget`, tipo que não pode ser construído pelo parser da CLI nem pela configuração isoladamente.

## Provas exigidas

```text
config validada
    -> credencial target no OS store
    -> Docker context atual é Unix local
    -> context atual == context salvo
    -> container ID + nome == identidade salva
    -> container está running
    -> label obrigatória quando target é reprodb-managed
    -> conexão real pelo namespace do container
    -> @@server_uuid do target != UUID registrado no dump source
    -> vendor e série MySQL compatíveis com client aprovado
    -> database pertence ao prefixo permitido e não é o central
    -> AuthorizedLocalTarget
```

O ID completo do container, e não apenas `mysql-8`, é a identidade principal. Recriar um container com o mesmo nome invalida a configuração e exige novo `reprodb setup`.

O ID do container prova qual target Docker foi autorizado; o `@@server_uuid` prova que o MySQL target não é o mesmo servidor que originou o dump. A comparação acontece antes do lock, do estado `incomplete` e de qualquer `DROP`, e bloqueia também a tentativa de usar outro nome de database no mesmo servidor source.

Targets criados futuramente pelo reprodb usarão a label `com.sagatech.reprodb.target=true`; perder essa label invalida um target registrado como gerenciado. Um container existente é registrado como `user-confirmed` somente após a seleção explícita durante o setup.

## Allowlist de databases

O setup salva `tenant_database_prefix = "salt_"` por padrão. A autorização aceita, por exemplo:

```text
salt_sagatec
salt_polymer
```

e recusa:

```text
salt_central
mysql
information_schema
customer_data
```

Os databases administrativos já são impossíveis de representar como `DatabaseName`. A barreira acrescenta duas condições: o database central configurado nunca é um target de restore de tenant e o nome precisa começar pelo prefixo explicitamente salvo.

O prefixo pode ser configurado no setup para instalações do Salt com outra convenção. Ele aceita somente 1–32 letras ASCII, dígitos ou `_`; não é um fragmento SQL.

## Integração com o restore

A RDB-050 produz a identidade autorizada. A RDB-051 implementa `DROP`, `CREATE` e import numa API que exige esse tipo e também um `ValidatedRestoreArtifact`. Assim, pular uma das barreiras exige alterar deliberadamente a arquitetura, em vez de apenas passar outro profile ou caminho para uma função genérica.

As conexões internas de restore e registro exigem TLS, inclusive para suportar corretamente o `caching_sha2_password` padrão do MySQL 8.4. O pipeline completo e seu estado recuperável estão em [`restore-engine.md`](restore-engine.md).
