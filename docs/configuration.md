# Configuração local

O reprodb mantém seu estado local sob uma única raiz:

```text
~/.reprodb/
├── reprodb.toml
├── reprodb.lock
├── credentials/
├── cache/
└── data/
```

Isso vale para macOS e Linux. Testes, CI ou instalações que precisem de isolamento podem definir um caminho absoluto em `REPRODB_HOME`; por exemplo, `REPRODB_HOME=/tmp/reprodb-test`. O override nunca é aceito como path relativo.

A primeira leitura sem arquivo retorna uma configuração vazia na versão atual sem criar nada no disco. A primeira alteração cria o diretório e persiste o documento.

## Formato inicial

```toml
schema_version = 1
active_profile = "local-source"

[client_runtime]
type = "docker"
docker_context = "desktop-linux"

[local_target]
docker_context = "desktop-linux"
container_name = "mysql-8"
container_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
username = "root"
credential_key = "target:550e8400-e29b-41d4-a716-446655440001"
trust = "user-confirmed"

# Targets adicionais cadastrados anteriormente pelo setup.
[local_targets.mysql-target]
docker_context = "desktop-linux"
container_name = "mysql-target"
container_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
username = "root"
credential_key = "target:550e8400-e29b-41d4-a716-446655440002"
trust = "user-confirmed"

[profiles.local-source]
host = "127.0.0.1"
port = 3306
username = "root"
credential_key = "source:550e8400-e29b-41d4-a716-446655440000"
mysql_family = "mysql"
mysql_series = "8.4"
production = false
tls_mode = "required"

# CA is mandatory for verify-ca / verify-identity. Paths must be absolute.
[profiles.local-source.tls_material]
ca = "/Users/developer/.mysql/ca.pem"
# cert = "/Users/developer/.mysql/client-cert.pem"
# key = "/Users/developer/.mysql/client-key.pem"

[profiles.local-source.client]
image = "mysql:8.4.4@sha256:1d967fb75a64dc3c2894c69285becfc2304ae0c3c4f4c715c297f3c12d60b01c"
```

Senha não é um campo válido do schema. O TOML armazena apenas chaves opacas com escopo `source` ou `target`; a credencial é persistida em `~/.reprodb/credentials/`, com o diretório em `0700` e cada arquivo em `0600`.

O Docker context detectado durante o cadastro fica em `client_runtime.docker_context`. O `setup` seleciona um target no mesmo context, impedindo que verificação, dump e restore sejam executados acidentalmente em Engines diferentes.

`local_target` é o target default (o último confirmado pelo `setup`). `local_targets` preserva os demais pelo nome validado do container. Cada entrada mantém identidade e credential key próprias; a senha continua fora do TOML. `trust` registra se o container foi criado/rotulado pelo reprodb ou se um container existente foi explicitamente confirmado.

Os modos TLS são `disabled`, `preferred`, `required`, `verify-ca` e `verify-identity`. `required` impede uma conexão sem criptografia, mas não valida a identidade do servidor. `verify-ca` valida a cadeia apresentada pelo servidor; `verify-identity` também compara o hostname do profile com o certificado.

Um profile marcado como `production = true` somente é válido com `tls_mode = "verify-identity"` e `tls_material.ca`. O modo padrão para sources locais é `required`; `preferred` e `disabled` precisam ser escolhidos explicitamente. Certificado e chave de cliente são opcionais, mas devem ser configurados juntos.

O TOML guarda somente paths absolutos, nunca o conteúdo dos certificados. A cada conexão, o reprodb valida os arquivos, limita cada um a 1 MiB, copia o material para um diretório temporário privado e monta esse diretório como somente leitura no container efêmero do client MySQL. O option file referencia apenas paths fixos internos ao container; o diretório e as cópias desaparecem ao final da operação.

## Garantias de persistência

- campos desconhecidos e versões de schema não suportadas são rejeitados;
- value objects são validados também durante a desserialização;
- o conteúdo novo é escrito e sincronizado em um arquivo temporário no mesmo diretório;
- o arquivo temporário substitui o anterior por rename;
- escritores concorrentes são coordenados por um lock advisory local;
- no macOS/Linux, o diretório usa modo `0700` e os arquivos usam `0600`;
- erros de parse não repetem o conteúdo potencialmente sensível do TOML.

Leitores veem o documento antigo ou o novo; nunca um arquivo parcialmente escrito pelo fluxo normal do reprodb.

Option files que contêm a senha do MySQL e as cópias transitórias de material TLS continuam no diretório temporário seguro do sistema operacional e são removidos pelo guard ao final do processo. Eles são deliberadamente excluídos de `~/.reprodb`: um secret transitório não deve se tornar estado navegável ou persistente. Stagings de dumps, por outro lado, ficam em `~/.reprodb/cache` com sufixo `.part` até a publicação atômica.
