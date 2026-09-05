# Configuração local

O reprodb usa `directories::ProjectDirs` para localizar os diretórios nativos da aplicação. Isso evita depender de paths específicos de Unix e mantém a mesma abstração no macOS e no Linux. O arquivo principal se chama `reprodb.toml`.

A primeira leitura sem arquivo retorna uma configuração vazia na versão atual sem criar nada no disco. A primeira alteração cria o diretório e persiste o documento.

## Formato inicial

```toml
schema_version = 1
active_profile = "salt-local"

[client_runtime]
type = "docker"
docker_context = "desktop-linux"

[local_target]
docker_context = "desktop-linux"
container_name = "mysql-8"
container_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
username = "root"
credential_key = "target:550e8400-e29b-41d4-a716-446655440001"
central_database = "salt_central"

[profiles.salt-local]
host = "127.0.0.1"
port = 3306
username = "root"
credential_key = "source:550e8400-e29b-41d4-a716-446655440000"
mysql_family = "mysql"
mysql_series = "8.4"
production = false
tls_mode = "required"

[profiles.salt-local.client]
image = "mysql:8.4.4@sha256:1d967fb75a64dc3c2894c69285becfc2304ae0c3c4f4c715c297f3c12d60b01c"

[profiles.salt-local.tenant_resolver]
type = "salt-central"
central_database = "salt_central"
allow_domain_lookup = true
```

Senha não é um campo válido do schema. O TOML armazena apenas chaves opacas com escopo `source` ou `target`; a credencial é persistida pelo credential store do sistema operacional.

O Docker context detectado durante o cadastro fica em `client_runtime.docker_context`. O `setup` deve selecionar um target no mesmo context, impedindo que verificação, dump e restore sejam executados acidentalmente em Engines diferentes.

Os modos TLS iniciais são `required`, `preferred` e `disabled`. Um profile marcado como `production = true` somente é válido com `tls_mode = "required"`. O modo padrão de novos profiles é `required`; `preferred` e `disabled` precisam ser escolhidos explicitamente para fontes que não sejam de produção.

## Garantias de persistência

- campos desconhecidos e versões de schema não suportadas são rejeitados;
- value objects são validados também durante a desserialização;
- o conteúdo novo é escrito e sincronizado em um arquivo temporário no mesmo diretório;
- o arquivo temporário substitui o anterior por rename;
- escritores concorrentes são coordenados por um lock advisory local;
- no macOS/Linux, o diretório usa modo `0700` e os arquivos usam `0600`;
- erros de parse não repetem o conteúdo potencialmente sensível do TOML.

Leitores veem o documento antigo ou o novo; nunca um arquivo parcialmente escrito pelo fluxo normal do reprodb.
