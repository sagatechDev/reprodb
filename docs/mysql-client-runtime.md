# Runtime Docker dos clients MySQL

O reprodb não pesquisa versões na internet em tempo de execução e não instala `mysql` ou `mysqldump` no host. A escolha é feita por um catálogo compilado no binário, contendo apenas combinações testadas de série, versão exata e digest imutável.

## Catálogo aprovado

| Série do source | Client | Imagem aprovada |
|---|---|---|
| MySQL 8.0 | 8.0.46 | `mysql:8.0.46@sha256:7dcddc01f13bab2f15cde676d44d01f61fc9f99fe7785e86196dfc07d358ae2b` |
| MySQL 8.4 | 8.4.4 | `mysql:8.4.4@sha256:1d967fb75a64dc3c2894c69285becfc2304ae0c3c4f4c715c297f3c12d60b01c` |

Uma tag mutável como `mysql:8` ou uma imagem com outro digest é recusada antes de qualquer container ser iniciado.

`8.4` não é uma versão global fixa. Ela é usada apenas como client de descoberta porque já está disponível e consegue executar a consulta mínima de versão. Depois de `SELECT VERSION()`, o runtime seleciona a entrada que corresponde à série detectada, baixa a imagem automaticamente se estiver ausente, verifica digest e versão do binário e repete a conexão com o client definitivo. Séries fora do catálogo são recusadas com a versão observada e a lista de séries suportadas.

## Preparação

Para descobrir e preparar um client, o runtime:

1. valida o Docker context;
2. prepara o client de descoberta aprovado;
3. consulta a versão/vendor do servidor;
4. resolve a série detectada no catálogo;
5. executa `docker image inspect` para o client definitivo;
6. usa `docker image pull --quiet` somente quando a imagem está ausente;
7. inspeciona novamente e confere `RepoDigests`;
8. executa `mysql --version` com `--pull=never`;
9. repete a conexão usando o client selecionado;
10. devolve um `PreparedMysqlClient` associado ao mesmo Docker context.

O tipo preparado é exigido pelas operações seguintes. Assim, uma chamada de conexão não consegue receber diretamente uma string de imagem que não passou pelas verificações.

## Rede e credenciais

Hosts loopback informados pelo usuário (`127.0.0.1`, `localhost` e `::1`) são traduzidos dentro do adapter para `host.docker.internal`. O container recebe:

```text
--add-host=host.docker.internal:host-gateway
```

Hosts DNS ou IP externos permanecem inalterados. Essa regra mantém no profile o mesmo endpoint conceitual usado no DBeaver e concentra a diferença de namespace no runtime Docker.

O option file é montado como:

```text
type=bind,src=<diretório-temporário>,dst=/run/secrets/reprodb,readonly
```

`--defaults-file` é o primeiro argumento do client. A senha não aparece no argv, em variáveis de ambiente do container ou nos erros devolvidos pelo runtime.

O client 8.4 recebe também `--no-login-paths`. O client 8.0.46 não reconhece essa opção, portanto ela é omitida nessa série. Isso não introduz estado implícito: cada operação roda numa imagem oficial imutável em container efêmero, sem montar o diretório home do host; somente o option file privado do reprodb é montado.

O runtime monta o diretório temporário privado em `/run/secrets/reprodb` e usa `/run/secrets/reprodb/client.cnf`. O option file recebe explicitamente um dos cinco modos TLS. CA/cert/key são copiados para nomes internos fixos no mesmo mount read-only, sem colocar o path original no option file ou no argv.

`profile add` usa `REQUIRED` por padrão local. Produção exige `VERIFY_IDENTITY`, CA e confirmação de um cipher negociado; o preflight repete essa confirmação imediatamente antes do dump.

## Diagnóstico

Erros externos são convertidos para categorias sem repetir o stderr bruto:

- Docker/context indisponível;
- falha de acesso ao registry;
- digest divergente;
- versão incompatível;
- rede/VPN/host/porta inacessível;
- autenticação rejeitada;
- metadata inválida.

Os comandos são representados por programa e vetor de argumentos e executados diretamente, sem `sh -c`, `bash -c` ou interpolação de shell.

## Validação local

No macOS, os testes opt-in comprovaram:

- inspeção do digest aprovado no context `desktop-linux`;
- execução real dos clients MySQL 8.0.46 e 8.4.4;
- conexão TCP real ao container local `mysql-8` pela porta publicada;
- leitura de `VERSION()` e `@@version_comment`;
- option file montado read-only;
- segredo ausente dos argumentos.

O crate também compila no MSRV para `x86_64-unknown-linux-gnu`. A execução em um Docker Engine Linux e na VPN corporativa continua obrigatória antes do profile de produção.

## Referências

- [Docker: pull por digest](https://docs.docker.com/reference/cli/docker/image/pull/#pull-an-image-by-digest-immutable-identifier)
- [Docker: inspect e output JSON](https://docs.docker.com/reference/cli/docker/inspect/)
- [Docker: `host-gateway` e `--add-host`](https://docs.docker.com/reference/cli/docker/container/run/#add-entries-to-container-hosts-file---add-host)
