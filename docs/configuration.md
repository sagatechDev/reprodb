# Configuração local

O reprodb usa `directories::ProjectDirs` para localizar os diretórios nativos da aplicação. Isso evita depender de paths específicos de Unix e mantém a mesma abstração no macOS e no Linux. O arquivo principal se chama `reprodb.toml`.

A primeira leitura sem arquivo retorna uma configuração vazia na versão atual sem criar nada no disco. A primeira alteração cria o diretório e persiste o documento.

## Formato inicial

```toml
schema_version = 1
active_profile = "salt-local"

[client_runtime]
type = "docker"

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

[profiles.salt-local.client]
image = "mysql:8.4.4@sha256:<digest-aprovado>"

[profiles.salt-local.tenant_resolver]
type = "salt-central"
central_database = "salt_central"
allow_domain_lookup = true
```

Senha não é um campo válido do schema. O TOML armazena apenas chaves opacas com escopo `source` ou `target`; a credencial será persistida pelo credential store do sistema operacional na RDB-022.

## Garantias de persistência

- campos desconhecidos e versões de schema não suportadas são rejeitados;
- value objects são validados também durante a desserialização;
- o conteúdo novo é escrito e sincronizado em um arquivo temporário no mesmo diretório;
- o arquivo temporário substitui o anterior por rename;
- escritores concorrentes são coordenados por um lock advisory local;
- no macOS/Linux, o diretório usa modo `0700` e os arquivos usam `0600`;
- erros de parse não repetem o conteúdo potencialmente sensível do TOML.

Leitores veem o documento antigo ou o novo; nunca um arquivo parcialmente escrito pelo fluxo normal do reprodb.
