# Roteiro de teste da UI/UX da CLI

Este roteiro permite avaliar a experiência planejada do `reprodb` antes de os fluxos reais estarem completos.

Os comandos com `--preview` são simulações determinísticas e sem efeitos colaterais. Eles não leem nem escrevem configuração, não acessam Keychain/Secret Service, não executam Docker e não conectam em MySQL. Os containers, versões, bancos e resultados exibidos são exemplos.

## 1. Preparação

Na raiz do repositório:

```bash
rustc --version
cargo build
```

O projeto requer Rust 1.88 ou mais recente. Depois do build, use diretamente o binário para não misturar mensagens do Cargo com a experiência da CLI:

```bash
REPRODB=./target/debug/reprodb
```

## 2. Primeiro contato e descoberta

Comece como alguém que ainda não conhece a ferramenta:

```bash
$REPRODB --help
$REPRODB profile --help
$REPRODB profile add --help
$REPRODB pull --help
```

Observe:

- se os nomes dos comandos deixam clara a intenção;
- se os argumentos obrigatórios são descobertos sem consultar documentação;
- se `source profile`, `local target`, `tenant` e `dump` parecem conceitos distintos;
- se `--fresh` comunica corretamente que ignora um cache válido.

## 3. Jornada principal simulada

### 3.1 Escolher o MySQL local

```bash
$REPRODB setup --preview
```

Esse cenário simula como o `setup` apresenta containers MySQL encontrados no Docker e escolhe o target local. Avalie se imagem, estado, porta e finalidade do container estão claros.

O fluxo real também pode ser percorrido sem salvar: execute o comando abaixo, selecione um container e pressione `Ctrl+C` durante os prompts de credencial.

```bash
$REPRODB setup --color never
```

Sem `--preview`, a descoberta consulta o Docker real. Se você concluir todos os prompts com uma credencial válida, o target será testado e salvo no Keychain/Secret Service e no TOML local.

### 3.2 Cadastrar uma conexão source

```bash
$REPRODB profile add salt-source --preview
```

Avalie principalmente:

- a ordem de host, porta, usuário e senha;
- se fica claro que essa conexão é a origem do dump;
- se a detecção automática da versão e a seleção do client fazem sentido;
- se a diferença entre resolver tenant e escolher database está compreensível;
- se o resumo final é suficiente antes de salvar;
- se a localização da senha está clara sem expor seu valor.

A prévia usa asteriscos para representar o feedback do prompt real. Para conferir o comportamento em um terminal sem concluir o cadastro:

```bash
$REPRODB profile add ux-mask-check --color never
```

Aceite os defaults de host e porta, informe um usuário, digite ou cole uma senha e observe um `*` por caractere. Pressione `Ctrl+C` ainda nos prompts para sair antes da conexão e da persistência. Em geral, colar usa `Cmd+V` no macOS e `Ctrl+Shift+V` no Linux, mas o atalho é definido pelo terminal.

O nome do profile já passa pela validação real. Experimente um nome inválido:

```bash
$REPRODB profile add 'nome com espaço' --preview
echo $?
```

O resultado esperado é erro de uso com exit code `2`, sem imprimir o valor recebido.

### 3.3 Diagnosticar o ambiente

```bash
$REPRODB doctor --preview
```

Observe se a divisão entre configuração, client MySQL, target local e source ajuda a localizar um problema rapidamente.

O diagnóstico real também está disponível e é somente-leitura:

```bash
$REPRODB doctor --color always
echo $?
```

Ele não baixa a imagem do client, inicia o target ou altera bancos/configuração. Os probes executam containers efêmeros `--rm` do client já presente. É normal obter falhas acionáveis enquanto `profile add` ou `setup` ainda não estiverem completos. Confira se checks independentes continuam aparecendo depois da primeira falha e se as seções `Configuration`, `Storage`, `Docker`, `Source` e `Local target` deixam claro onde agir.

### 3.4 Reproduzir um tenant

Fluxo normal, com consulta ao cache:

```bash
$REPRODB pull sagatec --preview
```

Fluxo que força um dump novo:

```bash
$REPRODB pull sagatec --fresh --preview
```

Compare as mensagens de cache e confirme se source, target, progresso sem percentual e resultado final ficam claros.

Repita com outro database observado no ambiente local:

```bash
$REPRODB pull polymer --preview
```

As resoluções ilustradas são `sagatec → salt_sagatec` e `polymer → salt_polymer`. Para qualquer outro alias, a prévia mostra placeholders em vez de inventar um nome de database.

## 4. Cores e acessibilidade

Por padrão, cores são usadas somente quando a saída está ligada a um terminal compatível. Os símbolos e textos continuam comunicando o estado sem depender da cor.

Force ou desabilite as cores para comparar:

```bash
$REPRODB pull sagatec --preview --color always
$REPRODB pull sagatec --preview --color never
NO_COLOR=1 $REPRODB pull sagatec --preview
```

Observe se verde comunica sucesso, amarelo chama atenção sem parecer falha concluída e ciano indica seleção/contexto. O modo sem cor precisa continuar inteiramente compreensível.

## 5. Passeio completo em um comando

O script abaixo compila o binário e executa a sequência de descoberta e os quatro cenários de prévia:

```bash
./scripts/preview-cli.sh
```

Ele também para imediatamente se algum cenário retornar erro inesperado.

## 6. Testar o dump real

O setup de um container existente e o ciclo de source profiles já são reais:

```bash
$REPRODB profile add salt-local
$REPRODB profile list
$REPRODB profile use salt-local
$REPRODB profile remove salt-local
```

Esses comandos podem acessar Docker, MySQL, configuração e Keychain/Secret Service. Use `--preview` quando quiser apenas avaliar a apresentação. `doctor` executa checks reais somente-leitura.

Depois de configurar um source MySQL local, o dump já pode ser exercitado de ponta a ponta:

```bash
$REPRODB profile use salt-local
$REPRODB doctor
$REPRODB dump sagatec
echo $?
```

Esse comando é uma operação real: ele consulta o source, resolve `sagatec` pelo `salt_central`, lê `salt_sagatec` com `mysqldump` e grava um `.sql.zst` no cache da aplicação. Para avaliar a UX sem acessar produção, use um profile local. Um profile classificado como produção deve exibir `PRODUCTION SOURCE` em vermelho antes de qualquer acesso e só deve ser usado durante o piloto autorizado descrito na RDB-073.

Uma execução bem-sucedida deve terminar aproximadamente assim:

```text
✓ Dump ready

  Profile:   salt-local
  Tenant:    salt_sagatec
  Database:  salt_sagatec
  Dump ID:   <uuid>
  MySQL:     8.4.4 (client 8.4.4)
  Data:      <tamanho SQL> → <tamanho Zstd>
  Duration:  <mm:ss>
  Cache:     <caminho local>
```

Durante a exportação em um terminal interativo, observe se a linha única de progresso com bytes, taxa e duração é legível e não polui o histórico. Com stdout redirecionado, a saída deve permanecer estável e sem animação.

Copie o `Dump ID` retornado e exercite o restore real no target escolhido pelo `setup`:

```bash
$REPRODB restore sagatec --dump-id <uuid>
echo $?
```

Antes de substituir o database local, confira se o plano mostra o profile source gravado no dump, `salt_sagatec`, o container `mysql-8`, o mesmo UUID e o domain `sagatec`. O comando não consulta novamente o source, mas executa `DROP/CREATE` no database tenant local, importa o SQL e atualiza `salt_central`; não use um target que contenha dados locais que você queira preservar.

Repita o mesmo comando para avaliar o retry idempotente. Um UUID inválido deve terminar com exit code `2`, e um UUID válido mas ausente do cache deve terminar com exit code `50`, ambos antes de acessar Docker:

```bash
$REPRODB restore sagatec --dump-id ../../dump.sql.zst
$REPRODB restore sagatec --dump-id 550e8400-e29b-41d4-a716-446655440000
```

O fluxo completo também está disponível. A primeira chamada normalmente cria um dump e a segunda deve mostrar um cache hit com o mesmo UUID:

```bash
$REPRODB pull sagatec
$REPRODB pull sagatec
$REPRODB pull sagatec --fresh
$REPRODB pull sagatec --database salt_sagatec_debug
$REPRODB pull polymer --target mysql-target --database salt_polymer_debug
```

Compare a indicação `new dump`/`reused`, a idade do cache e o aviso antes da substituição local. Com dois containers cadastrados por `setup`, o prompt de container deve listar ambos e marcar o default; `--target` deve pular essa escolha. Em seguida, o prompt de database deve oferecer `salt_sagatec`; pressionar Enter mantém o default. `--fresh` deve gerar outro UUID. A opção `--database` deve restaurar no nome alternativo permitido e atualizar o registro correspondente no `salt_central` do target escolhido.

Inspecione então a pasta local e compare os UUIDs, idades e estados de integridade:

```bash
$REPRODB cache list
$REPRODB cache clean
$REPRODB cache purge sagatec
```

`cache list` lê os dumps completos para validar SHA-256 e pode demorar em caches grandes. `clean` remove somente itens vencidos/abandonados; `purge` remove os dumps do tenant somente no profile ativo e não altera o database já restaurado no `mysql-8`.

A prévia continua disponível para avaliar o fluxo sem tocar nos bancos:

```bash
$REPRODB pull sagatec --preview
```

## 7. Checklist para feedback

Durante o teste, anote:

- qual comando ou pergunta gerou dúvida;
- qual termo parece técnico demais;
- informação que faltou antes de confirmar uma ação;
- informação redundante;
- se uma linha parece afirmar que algo real aconteceu durante a prévia;
- mensagem que não indica o próximo passo;
- diferença de renderização no terminal usado e no sistema operacional.

O feedback mais útil inclui o comando executado, o trecho da saída e como você esperava que ele se comportasse.
