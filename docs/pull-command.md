# `reprodb pull` — fluxo principal

`reprodb pull DATABASE` é o fluxo principal do reprodb: reutiliza um dump local válido ou cria um novo e, em seguida, restaura o database no MySQL escolhido por `reprodb setup`.

```bash
reprodb pull acme_production
reprodb pull globex_production
reprodb pull acme_production --fresh
reprodb pull acme_production --database acme_debug
reprodb pull globex_production --target mysql-target --database globex_debug
```

## Cache primeiro

O comando carrega o profile ativo e calcula seu fingerprint apenas com a configuração local. Em seguida, procura sob `~/.reprodb/cache/profiles/<profile>/<database>` por um artefato do `DATABASE` informado.

Um candidato somente vira hit se profile, identidade do diretório, fingerprint, DumpPolicy, TTL, tamanho e SHA-256 comprimido continuarem válidos. A lease compartilhada permanece viva enquanto o validator completo do restore adquire sua própria lease, portanto o cleanup não consegue remover o artefato entre as duas fases.

No hit:

- nenhuma credencial source é lida;
- `mysqldump` não é executado;
- o artefato ainda passa pela validação completa de Zstd e checksum SQL antes do `DROP` local.

Essas garantias também valem para produção. A CLI mostra `PRODUCTION SOURCE` em vermelho antes da consulta ao cache. Um hit pode concluir sem carregar a credencial de produção; um miss acessa o source e ainda precisa publicar um dump gerenciado antes de iniciar o restore.

No miss, o fluxo usa o mesmo [`DumpService`](dump-command.md) do comando `dump`, publica um novo artefato gerenciado e passa seu UUID ao mesmo [`RestoreService`](restore-command.md) do comando `restore`.

`--fresh` produz um miss deliberado antes de ler os arquivos de cache. Ele não apaga dumps anteriores, mas sempre acessa o source e cria um UUID novo se o dump for bem-sucedido.

## Ordem operacional

```text
config + profile ativo
    -> cache por profile + database
       -> hit: reutilizar UUID
       -> miss/fresh: resolver source -> preflight -> mysqldump -> Zstd -> publicar UUID
    -> validar integralmente o artefato
    -> atestar e autorizar target Docker local
    -> mostrar source/target e aviso de substituição
    -> recriar e importar database
    -> informar database, container, cache e dump ID
```

O target continua exigindo sua própria credencial mesmo num cache hit. Cache local evita acesso ao source; ele não desabilita as barreiras do restore.

Em um terminal interativo com mais de um target configurado, a CLI pergunta primeiro `Local restore container` e destaca o default definido pelo último `reprodb setup`. Com um único target, ele é escolhido sem um prompt redundante. `--target CONTAINER` faz a seleção explicitamente e falha antes do dump se o container ainda não foi cadastrado pelo setup.

Depois do cache hit ou do novo dump, a CLI pergunta `Local restore database` usando o nome do source como default. Pressionar Enter preserva esse nome. `--database` escolhe antecipadamente outro nome e evita a pergunta; em execução não interativa, o default é aplicado automaticamente.

Databases administrativos do MySQL são recusados. A CLI mantém “source database” e “target database” separados no plano e usa o nome local no lock.

## Saída

Um miss mostra que o source será exportado e acompanha bytes, taxa, duração e, após amostra suficiente, `ETA ~mm:ss`, sem inventar percentual. A previsão é omitida se a estimativa do MySQL for insuficiente ou já tiver sido ultrapassada. Um hit mostra a idade e o UUID encontrados. Ambos apresentam o plano de restore antes da primeira operação destrutiva e terminam aproximadamente assim:

```text
✓ Database ready

  Database:   acme_debug
  Container:  mysql-8
  Cache:      reused (34m)
  Dump ID:    <uuid>
```

## Falhas e limites

- configuração sem profile ativo falha antes de source ou Docker;
- cache inválido vira miss e tenta gerar um dump novo;
- se o dump falhar, nenhum restore é iniciado;
- se o restore falhar, o artefato completo continua disponível para retry;
- falha no registro central informa que o database foi importado, sem apagar o resultado;
- profiles de produção exigem TLS verificado, cache gerenciado e a dump policy conservadora;
- `--fresh` é a única forma de ignorar deliberadamente um hit válido; não existe modo implícito ou configuração persistente que force dumps frescos;
- o restore recebe somente um `AuthorizedLocalTarget` atestado; nunca recebe host, porta, credencial ou profile do source;
- `Ctrl+C` cancela dump ou restore, aguarda os processos, limpa o estado parcial e retorna 130.

O TTL de frescor é de uma hora e continua fixo em código. Ele decide apenas se o `pull` reutiliza o dump: passado esse prazo, o `pull` exporta de novo e o dump anterior permanece no disco, restaurável por `restore`. Os comandos de inspeção e remoção estão em [`cache-commands.md`](cache-commands.md); tornar os prazos configuráveis permanece uma evolução separada.

## Evidência real

O teste integrado de serviços cria um database efêmero no source `mysql-8` e inicia um segundo container MySQL temporário como target. A primeira chamada produz o dump de A e restaura em B; depois a credencial source é removida do store em memória e a segunda chamada precisa concluir pelo cache. Ao final, dados e UUID são comparados e os containers são removidos:

```bash
cargo test --test pull_integration -- --ignored --nocapture
```

O CI Linux usa um cenário adicional que atravessa o executável real e persiste estado entre processos. Ele executa `setup`, `profile add` e dois `pull`; antes do segundo `pull`, desliga o source para provar que o cache hit não depende mais dele:

```bash
cargo test --test pull_integration \
  real_cli_configures_pulls_and_reuses_cache_with_the_source_offline \
  -- --ignored --nocapture
```

`REPRODB_TEST_CREDENTIAL_DIR` é somente infraestrutura de teste: permite que processos do binário compartilhem credenciais fictícias em um diretório absoluto e descartável.

O resumo final do `pull` também informa tempos separados. Em cache miss mostra `dump`, `restore` e `total`; em cache hit mostra `restore` e `total`, pois não houve dump naquela execução. Essas métricas são durações observadas, enquanto o ETA durante o dump continua sendo uma projeção aproximada.
