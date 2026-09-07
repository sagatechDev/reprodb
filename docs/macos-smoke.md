# Smoke suite no macOS

A smoke suite cobre os limites que mais variam entre Linux e macOS:

- paths e permissões dentro de `.reprodb`;
- lock entre processos e liberação após encerramento;
- cancelamento cooperativo e exit code `130`;
- Keychain nativo com uma credencial aleatória e temporária;
- Docker Desktop e descoberta do context local;
- descoberta do container `mysql-8`;
- client MySQL 8.4 em Docker conectando ao host;
- execução do binário real entre dois MySQL efêmeros, incluindo cache hit com o source desligado.

## Pré-requisitos

- macOS em `arm64` ou `x86_64`;
- Rust compatível com o projeto;
- Docker Desktop iniciado;
- container local `mysql-8` em execução, criado com `MYSQL_ROOT_PASSWORD` e usando a imagem `mysql:8` observada no Salt;
- imagem de client aprovada disponível ou acesso ao registry para obtê-la.

Se o fixture tiver outro nome, informe-o sem alterar a configuração real do reprodb:

```bash
REPRODB_TEST_MYSQL_CONTAINER=meu-mysql-8 ./scripts/smoke-macos.sh
```

No ambiente padrão:

```bash
./scripts/smoke-macos.sh
```

O teste do Keychain cria uma chave com UUID novo e tenta removê-la mesmo após a leitura. Os testes Docker de descoberta e conexão são somente-leitura sobre o fixture existente. O E2E cria containers com nomes UUID e `--rm`, usa um `REPRODB_HOME` temporário e não lê nem escreve `~/.reprodb`.

## Evidência atual

Em 7 de setembro de 2026, a suite passou em:

```text
macOS Darwin 25.5.0
Apple Silicon arm64
Docker Desktop context desktop-linux
Docker client/server 27.5.1
fixture mysql-8 (mysql:8)
MySQL client/server 8.4
```

O E2E do binário completou `setup`, `profile add`, dump, restore e cache hit entre dois containers em aproximadamente 20 segundos. A execução em macOS Intel permanece pendente e só é obrigatória se máquinas `x86_64` ainda fizerem parte do parque suportado.
