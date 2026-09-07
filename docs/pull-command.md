# `reprodb pull` — fluxo principal

`reprodb pull TENANT` é o fluxo principal do reprodb: reutiliza um dump local válido ou cria um novo e, em seguida, restaura o tenant no MySQL escolhido por `reprodb setup`.

```bash
reprodb pull sagatec
reprodb pull polymer
reprodb pull sagatec --fresh
reprodb pull sagatec --database salt_sagatec_debug
```

## Cache primeiro

O comando carrega o profile ativo e calcula seu fingerprint apenas com a configuração local. Em seguida, procura sob `~/.reprodb/cache/profiles/<profile>` por metadata cujo lookup original ou tenant ID canônico corresponda ao `TENANT` informado.

Um candidato somente vira hit se profile, identidade do diretório, fingerprint, DumpPolicy, TTL, tamanho e SHA-256 comprimido continuarem válidos. A lease compartilhada permanece viva enquanto o validator completo do restore adquire sua própria lease, portanto o cleanup não consegue remover o artefato entre as duas fases.

No hit:

- nenhuma credencial source é lida;
- o `salt_central` source não é consultado;
- `mysqldump` não é executado;
- o artefato ainda passa pela validação completa de Zstd e checksum SQL antes do `DROP` local.

No miss, o fluxo usa o mesmo [`DumpService`](dump-command.md) do comando `dump`, publica um novo artefato gerenciado e passa seu UUID ao mesmo [`RestoreService`](restore-command.md) do comando `restore`.

`--fresh` produz um miss deliberado antes de ler os arquivos de cache. Ele não apaga dumps anteriores, mas sempre acessa o source e cria um UUID novo se o dump for bem-sucedido.

## Ordem operacional

```text
config + profile ativo
    -> cache por profile + tenant lookup
       -> hit: reutilizar UUID
       -> miss/fresh: resolver source -> preflight -> mysqldump -> Zstd -> publicar UUID
    -> validar integralmente o artefato
    -> atestar e autorizar target Docker local
    -> mostrar source/target e aviso de substituição
    -> recriar e importar database
    -> registrar tenant/domain no salt_central local
    -> informar database, container, domain, cache e dump ID
```

O target continua exigindo sua própria credencial mesmo num cache hit. Cache local evita acesso ao source; ele não desabilita as barreiras do restore.

Em um terminal interativo, depois do cache hit ou do novo dump, a CLI pergunta `Local restore database` usando o nome do source como default. Pressionar Enter preserva esse nome. `--database` escolhe antecipadamente outro nome e evita a pergunta; em execução não interativa, o default é aplicado automaticamente.

O nome escolhido ainda precisa estar dentro da allowlist `salt_*` do target configurado. A CLI mantém “source database” e “target database” separados no plano, usa o nome local no lock e atualiza `tenancy_db_name` no `salt_central` somente depois do import completo.

## Saída

Um miss mostra que o source será exportado e acompanha bytes, taxa e duração sem inventar percentual. Um hit mostra a idade e o UUID encontrados. Ambos apresentam o plano de restore antes da primeira operação destrutiva e terminam aproximadamente assim:

```text
✓ Tenant ready

  Database:   salt_sagatec
  Container:  mysql-8
  Domain:     sagatec
  Cache:      reused (34m)
  Dump ID:    <uuid>
```

A CLI mostra o domain conhecido, mas não inventa URL ou porta HTTP do Salt.

## Falhas e limites

- configuração sem profile ativo falha antes de source ou Docker;
- cache inválido vira miss e tenta gerar um dump novo;
- se o dump falhar, nenhum restore é iniciado;
- se o restore falhar, o artefato completo continua disponível para retry;
- falha no registro central informa que o database foi importado, sem apagar o resultado;
- profiles de produção continuam bloqueados para novos dumps até o hardening correspondente;
- cancelamento completo por `Ctrl+C` pertence à RDB-060.

O TTL inicial continua fixado em duas horas. Os comandos de inspeção/limpeza estão disponíveis em [`cache-commands.md`](cache-commands.md); tornar o TTL configurável permanece uma evolução separada.

## Evidência real

O teste integrado cria um tenant e database efêmeros no `mysql-8`. A primeira chamada produz o dump; depois a credencial source é removida do store em memória e a segunda chamada precisa concluir pelo cache. Ao final, dados, registro central e UUID são comparados e todas as fixtures são removidas:

```bash
cargo test --test pull_integration -- --ignored --nocapture
```
