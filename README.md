<h1 align="center">reprodb</h1>

<p align="center">
  Copie um banco MySQL de um host para o seu MySQL local — em um comando.
</p>

<p align="center">
  <a href="https://github.com/sagatechDev/reprodb/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/sagatechDev/reprodb/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="platform" src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey">
  <img alt="rust" src="https://img.shields.io/badge/rust-1.88%2B-orange">
</p>

```console
$ reprodb pull acme_production

  Source:     prod-source (PRODUCTION SOURCE)
  Local restore database [acme_production]: acme_debug

✓ Database ready

  Database:   acme_debug
  Container:  mysql-8
  Cache:      reused (34m)
  Dump ID:    9f1c2b7a-5f0e-4b31-9c2f-7a0d8e1b4c66
```

`reprodb` faz o dump de um database remoto, guarda em cache local por 2h e restaura num container MySQL do seu Docker — sem `mysqldump` instalado na máquina, sem senha em texto puro no seu repositório e sem risco de escrever no banco errado.

---

## Instalação

```bash
curl -fsSL https://raw.githubusercontent.com/sagatechDev/reprodb/main/install.sh | bash
```

Baixa o binário pronto, confere o SHA-256 e instala em `~/.local/bin/reprodb`. Não precisa de Rust, nem de `git`, nem de conta no GitHub. Confirme com `reprodb --version`.

<details>
<summary>Outras formas de instalar</summary>

**Versão específica ou outro diretório**

```bash
curl -fsSL https://raw.githubusercontent.com/sagatechDev/reprodb/main/install.sh \
  | REPRODB_VERSION=v0.1.0 REPRODB_INSTALL_DIR=/usr/local/bin bash
```

**Download manual**

```bash
curl -fsSLO https://github.com/sagatechDev/reprodb/releases/latest/download/reprodb-aarch64-apple-darwin.tar.gz
tar -xzf reprodb-*.tar.gz
install -m 755 reprodb ~/.local/bin/
```

Binários publicados: `aarch64-apple-darwin`, `x86_64-apple-darwin` e `x86_64-unknown-linux-gnu`.

**Do código-fonte** (precisa de Rust 1.88+)

```bash
git clone https://github.com/sagatechDev/reprodb.git
cd reprodb && cargo install --path .
```

**Atualizar**: rode o mesmo comando de instalação de novo.
**Desinstalar**: `rm ~/.local/bin/reprodb && rm -rf ~/.reprodb`.

</details>

Se `reprodb` não for encontrado depois da instalação, `~/.local/bin` não está no seu `PATH`:

```fish
fish_add_path ~/.local/bin                                 # fish
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc    # bash/zsh
```

## Pré-requisitos

O binário é autocontido, mas o reprodb depende do ambiente local:

| Requisito | Por quê | Como obter |
|---|---|---|
| **Docker** rodando | o MySQL local e os clients `mysql`/`mysqldump` rodam em containers — nada é instalado na sua máquina | Docker Desktop (macOS) ou Docker Engine (Linux) |
| **Um container MySQL local** (8.0 ou 8.4) | é o destino dos restores | ex.: `docker run -d --name mysql-8 -e MYSQL_ROOT_PASSWORD=secret -p 127.0.0.1:3306:3306 mysql:8.4` |
| **~5 GiB livres** | espaço mínimo exigido antes de um dump | — |
| **Acesso de rede ao MySQL de origem** | o dump é remoto | VPN, se for o caso |

## Começando

```bash
# 1. escolha o container MySQL local que vai receber os restores
reprodb setup

# 2. cadastre a origem dos dumps (host, usuário, senha, TLS)
reprodb profile add prod-source

# 3. veja quais bancos existem na origem
reprodb db list

# 4. traga o banco
reprodb pull acme_production
```

O `pull` recebe o nome do database na origem e pergunta com que nome ele será criado localmente (Enter mantém o mesmo). Rode de novo dentro de 2h e ele reusa o dump em cache — sem tocar na origem.

## Comandos

| Comando | O que faz |
|---|---|
| `reprodb setup` | Configura o container MySQL local de destino |
| `reprodb profile add <nome>` | Cadastra uma origem MySQL (senha vai para o cofre do sistema) |
| `reprodb profile list` / `use` / `remove` | Gerencia as origens cadastradas |
| `reprodb doctor` | Diagnostica config, credenciais, Docker, disco e conexões — sem alterar nada |
| `reprodb db list` | Lista os databases disponíveis na origem ativa |
| `reprodb pull <database>` | **Fluxo principal**: dump (ou cache) + restore |
| `reprodb dump <database>` | Só gera o dump em cache |
| `reprodb restore <database> --dump-id <id>` | Só restaura um dump já existente |
| `reprodb cache list` / `clean` / `purge <database>` | Inspeciona e limpa os dumps locais |

Flags úteis:

```bash
reprodb pull acme_production --fresh                  # ignora o cache
reprodb pull acme_production --database acme_debug    # nome local, sem perguntar
reprodb pull acme_production --target mysql-target    # escolhe o container de destino
reprodb pull acme_production --preview                # mostra o plano, não executa nada
reprodb --help                                        # ajuda completa
```

`--preview` existe em `setup`, `profile add`, `doctor` e `pull`: mostra a experiência sem tocar em Docker, MySQL ou credenciais.

## Como funciona

```text
origem MySQL ──mysqldump──► dump.sql.zst (~/.reprodb/cache, TTL 2h)
                                  │
                                  ├─ valida tamanho + SHA-256
                                  ├─ atesta identidade do container por ID
                                  └─ recria o database e importa ──► MySQL local
```

**Garantias de segurança** — por que dá para rodar isso apontando para produção:

- a origem é lida apenas com `mysqldump`; nada é escrito nela;
- senhas ficam fora do TOML, em `~/.reprodb/credentials/` (diretório `0700`, arquivos `0600`); o TOML guarda só chaves opacas;
- databases de sistema (`mysql`, `information_schema`, `performance_schema`, `sys`) são recusados como destino;
- o container de destino é identificado pelo ID completo — um container recriado com o mesmo nome é recusado;
- perfis marcados como produção exigem TLS `verify-identity` com CA;
- cache hit conclui sem ler a credencial de produção;
- `Ctrl+C` cancela, aguarda os processos, limpa o estado parcial e retorna `130`.

## Configuração

Todo o estado local fica em `~/.reprodb/`:

```text
~/.reprodb/
├── reprodb.toml    # perfis, targets, políticas (nunca senhas)
├── credentials/    # senhas, 0700/0600
├── cache/          # dumps comprimidos
└── data/
```

| Variável | Uso |
|---|---|
| `REPRODB_HOME` | Raiz alternativa (path absoluto) para testes e automação |
| `RUST_LOG` | Logs de diagnóstico, ex.: `RUST_LOG=reprodb=debug` |
| `NO_COLOR` | Desliga cores (também há `--color never`) |

Detalhes do schema: [`docs/configuration.md`](docs/configuration.md).

## Quando algo dá errado

Comece sempre por:

```bash
reprodb doctor
```

Ele checa cada dependência isoladamente e sugere a ação para cada falha. Os exit codes são estáveis e por categoria — `10` configuração, `11` credencial, `20` dependência, `30` origem, `40` dump, `50` cache/disco, `60` Docker, `70` restore. Tabela completa: [`docs/exit-codes.md`](docs/exit-codes.md).

## Desenvolvimento

Requer Rust 1.88+, macOS ou Linux, e Docker para os testes de integração.

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test

./scripts/preview-cli.sh   # percorre a UX sem tocar em Docker/MySQL/Keychain
```

Testes que precisam de Docker real são `#[ignore]`:

```bash
cargo test --test pull_integration -- --ignored --nocapture
```

**Release**: suba a versão no `Cargo.toml`, crie a tag (`git tag v0.2.0 && git push --tags`) e o workflow [`release.yml`](.github/workflows/release.yml) compila os três alvos, gera `SHA256SUMS` e publica o release.

## Documentação

| Tema | Doc |
|---|---|
| Target Docker local | [`setup.md`](docs/setup.md) · [`local-target-safety.md`](docs/local-target-safety.md) |
| Perfis de origem | [`profiles.md`](docs/profiles.md) · [`configuration.md`](docs/configuration.md) |
| Fluxo principal | [`pull-command.md`](docs/pull-command.md) · [`dump-command.md`](docs/dump-command.md) · [`restore-command.md`](docs/restore-command.md) |
| Cache | [`cache-commands.md`](docs/cache-commands.md) · [`cache-validity.md`](docs/cache-validity.md) · [`compression.md`](docs/compression.md) |
| Diagnóstico | [`doctor.md`](docs/doctor.md) · [`exit-codes.md`](docs/exit-codes.md) |
| Decisões e roadmap | [`docs/adr`](docs/adr) · [`implementation-roadmap.md`](docs/implementation-roadmap.md) · [`local-mvp-definition-of-done.md`](docs/local-mvp-definition-of-done.md) |

## Status

Fluxo local implementado e validado no macOS. Pendências do MVP: validação em Docker Engine no Linux, Secret Service e E2E real — detalhes em [`local-mvp-definition-of-done.md`](docs/local-mvp-definition-of-done.md).
