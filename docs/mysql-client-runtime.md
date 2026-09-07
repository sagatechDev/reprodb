# Runtime Docker dos clients MySQL

O reprodb não pesquisa versões na internet em tempo de execução e não instala `mysql` ou `mysqldump` no host. A escolha é feita por um catálogo compilado no binário, contendo apenas combinações testadas de série, versão exata e digest imutável.

## Catálogo inicial

| Série do source | Client | Imagem aprovada |
|---|---|---|
| MySQL 8.4 | 8.4.4 | `mysql:8.4.4@sha256:1d967fb75a64dc3c2894c69285becfc2304ae0c3c4f4c715c297f3c12d60b01c` |

MySQL 8.0 ainda não possui uma entrada porque a matriz não foi validada. Uma tag mutável como `mysql:8` ou uma imagem com outro digest é recusada antes de qualquer container ser iniciado.

## Preparação

Para preparar um client, o runtime:

1. valida o Docker context;
2. compara a configuração com o catálogo;
3. executa `docker image inspect`;
4. usa `docker image pull --quiet` somente quando a imagem está ausente;
5. inspeciona novamente e confere `RepoDigests`;
6. executa `mysql --version` com `--pull=never`;
7. devolve um `PreparedMysqlClient` associado ao mesmo Docker context.

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
- execução real do client MySQL 8.4.4;
- conexão TCP real ao container local `mysql-8` pela porta publicada;
- leitura de `VERSION()` e `@@version_comment`;
- option file montado read-only;
- segredo ausente dos argumentos.

O crate também compila no MSRV para `x86_64-unknown-linux-gnu`. A execução em um Docker Engine Linux e na VPN corporativa continua obrigatória antes do profile de produção.

## Referências

- [Docker: pull por digest](https://docs.docker.com/reference/cli/docker/image/pull/#pull-an-image-by-digest-immutable-identifier)
- [Docker: inspect e output JSON](https://docs.docker.com/reference/cli/docker/inspect/)
- [Docker: `host-gateway` e `--add-host`](https://docs.docker.com/reference/cli/docker/container/run/#add-entries-to-container-hosts-file---add-host)
