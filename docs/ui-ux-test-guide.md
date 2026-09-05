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

Esse cenário mostra como o futuro `setup` apresentará containers MySQL encontrados no Docker e escolherá o target local. Avalie se imagem, estado, porta e finalidade do container estão claros.

### 3.2 Cadastrar uma conexão source

```bash
$REPRODB profile add salt-local --preview
```

Avalie principalmente:

- a ordem de host, porta, usuário e senha;
- se fica claro que essa conexão é a origem do dump;
- se a escolha da série do MySQL faz sentido;
- se a diferença entre resolver tenant e escolher database está compreensível;
- se o resumo final é suficiente antes de salvar;
- se a localização da senha está clara sem expor seu valor.

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

### 3.4 Reproduzir um tenant

Fluxo normal, com consulta ao cache:

```bash
$REPRODB pull guerra --preview
```

Fluxo que força um dump novo:

```bash
$REPRODB pull guerra --fresh --preview
```

Compare as mensagens de cache e confirme se source, target, progresso sem percentual e resultado final ficam claros.

## 4. Passeio completo em um comando

O script abaixo compila o binário e executa a sequência de descoberta e os quatro cenários de prévia:

```bash
./scripts/preview-cli.sh
```

Ele também para imediatamente se algum cenário retornar erro inesperado.

## 5. Limite atual intencional

Sem `--preview`, os handlers ainda falham explicitamente em vez de fingir que fizeram algo:

```bash
$REPRODB setup
echo $?
```

Por enquanto, o resultado esperado é `command 'setup' is not implemented yet` e exit code `1`. Isso mudará conforme RDB-024, RDB-025 e RDB-026 forem concluídas.

## 6. Checklist para feedback

Durante o teste, anote:

- qual comando ou pergunta gerou dúvida;
- qual termo parece técnico demais;
- informação que faltou antes de confirmar uma ação;
- informação redundante;
- se uma linha parece afirmar que algo real aconteceu durante a prévia;
- mensagem que não indica o próximo passo;
- diferença de renderização no terminal usado e no sistema operacional.

O feedback mais útil inclui o comando executado, o trecho da saída e como você esperava que ele se comportasse.
