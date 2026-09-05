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

## 6. Estado funcional atual

O setup de um container existente e o ciclo de source profiles já são reais:

```bash
$REPRODB profile add salt-local
$REPRODB profile list
$REPRODB profile use salt-local
$REPRODB profile remove salt-local
```

Esses comandos podem acessar Docker, MySQL, configuração e Keychain/Secret Service. Use `--preview` quando quiser apenas avaliar a apresentação. `doctor` já executa checks reais somente-leitura. `dump`, `restore`, `pull` e `cache` ainda falham explicitamente em vez de fingir que fizeram algo:

```bash
$REPRODB dump sagatec
echo $?
```

Por enquanto, o resultado esperado é `command 'dump' is not implemented yet` e exit code `1`. Isso mudará com a milestone de dump.

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
