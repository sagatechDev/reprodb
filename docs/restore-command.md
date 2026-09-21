# `reprodb restore` — contrato operacional

`reprodb restore DATABASE --dump-id ID` restaura um artefato gerenciado no target Docker configurado.

```bash
reprodb setup
reprodb dump acme_production
reprodb restore acme_production --dump-id 550e8400-e29b-41d4-a716-446655440000
reprodb restore acme_production --dump-id 550e8400-e29b-41d4-a716-446655440000 --database acme_debug
reprodb restore acme_production --dump-id 550e8400-e29b-41d4-a716-446655440000 --target mysql-target
```

O comando não aceita path de arquivo nem host. O UUID precisa identificar exatamente um dump completo dentro de `~/.reprodb/cache`. `--target` escolhe somente um container já cadastrado pelo `setup`; sem a opção, o target default é usado. `--database` troca somente o nome do database local; sem a opção, o nome original do dump é usado.

## Fluxo

```text
database + dump ID
    -> localizar UUID no cache gerenciado
    -> validar metadata, identidade, tamanho, Zstd e dois SHA-256
    -> atestar context, container, credencial e versão do target
    -> autorizar o database local
    -> mostrar source, target e aviso destrutivo
    -> marcar estado incomplete
    -> DROP/CREATE do database local
    -> Zstd -> stdin do mysql
    -> marcar estado ready
```

O source MySQL não é consultado. Profile, database, versões e encoding vêm da metadata validada no momento do dump. Por isso um restore explícito continua possível quando o source está offline e não depende do profile atualmente ativo.

O `DATABASE` precisa coincidir com o database de origem gravado no dump. Informar `globex_production` para um dump de `acme_production` falha antes de ler credenciais, consultar Docker ou alterar MySQL. UUID ausente ou duplicado também é recusado.

## UX

Depois das validações não destrutivas, a CLI apresenta um plano semelhante a:

```text
Restore plan
  Source:     local-source / acme_production
  Dump ID:    <uuid>
  MySQL:      8.4.4 (client 8.4.4)
  Target:     mysql-8/acme_debug

! The selected local target database will be replaced.
```

Não existe confirmation prompt: executar `restore` com um UUID gerenciado já expressa a intenção. A barreira efetiva é estrutural e limita o `DROP` ao database validado dentro de um target selecionado e atestado pelo `setup`. Databases administrativos do MySQL são recusados antes de qualquer conexão.

O resultado mostra database, container, bytes importados e dump ID.

## Falhas e retry

- corrupção, identidade divergente e target inválido falham antes do `DROP`;
- falha durante recreate/import deixa o estado local como `incomplete`;
- repetir exatamente o mesmo comando recria o database sem acessar o source;
- o artefato completo e sua lease são preservados durante a operação.

Um dump escolhido explicitamente por ID não depende do TTL do cache. A limpeza futura continua podendo remover artefatos expirados quando eles não estiverem em uso.

## Evidências

Os testes unitários cobrem localização global segura pelo UUID, ID duplicado, database divergente, ordem do workflow, escopo do plano visual e categorias de exit code. O teste integrado executa o workflow duas vezes contra o `mysql-8` e compara os dados restaurados:

```bash
cargo test --test restore_integration -- --ignored --nocapture
```

`Ctrl+C` fecha o stdin do import, encerra e aguarda o `docker run`, remove explicitamente o client container efêmero e libera o lock. Como o banco pode ter sido recriado antes da interrupção, seu estado permanece `incomplete` e o mesmo dump pode ser usado novamente sem acessar o source.
