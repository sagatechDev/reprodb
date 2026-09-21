# RDB-002 — Spike do client MySQL em Docker

> **Histórico.** Pesquisa datada, anterior à remoção do conceito de tenant. Mantida como registro.

> Status: em andamento. Caminho macOS validado em 5 de setembro de 2026; Linux e VPN ainda pendentes.

## Objetivo

Provar que o reprodb pode executar `mysql` e `mysqldump` em um container efêmero, mantendo a conexão descrita como host/porta e sem instalar clients no host.

## Cenário macOS validado

| Componente | Valor |
|---|---|
| Host | macOS ARM64 |
| Runtime | Docker Desktop |
| Docker context | `desktop-linux` por socket Unix local |
| Client container | MySQL 8.4.4 Linux ARM64 |
| Client image | `mysql@sha256:1d967fb75a64dc3c2894c69285becfc2304ae0c3c4f4c715c297f3c12d60b01c` |
| Source | container `mysql-8`, porta 3306 publicada no host |
| Endpoint visto pelo client | `host.docker.internal:3306` |
| Schema do dump de teste | `salt_test` |

## Transporte da credencial no spike

O teste utilizou um option file temporário criado com `umask 077` e modo `0600`:

```ini
[client]
host=host.docker.internal
port=3306
user=root
password="<redacted>"
protocol=TCP
```

O arquivo foi montado no container como read-only:

```text
type=bind,src=<temporary-file>,dst=/run/secrets/reprodb.cnf,readonly
```

O client recebeu somente:

```text
--defaults-file=/run/secrets/reprodb.cnf
```

A senha não fez parte do argv. No spike ela foi obtida da configuração do container local sem ser impressa; no produto virá do `CredentialStore`.

O option file permaneceu disponível até o término do processo e foi removido no cleanup.

## Conectividade

O container foi iniciado com:

```text
--add-host=host.docker.internal:host-gateway
```

Isso preserva uma forma única de endpoint para Docker Desktop e prepara o adapter para o caminho equivalente em Docker Engine Linux.

Consulta executada:

```sql
SELECT VERSION(), @@version_comment;
```

Resultado técnico:

```text
8.4.4  MySQL Community Server - GPL
```

## Dump executado

O teste executou `mysqldump` com argumentos estruturados:

```text
--defaults-file=/run/secrets/reprodb.cnf
--single-transaction
--quick
--no-tablespaces
--hex-blob
--set-gtid-purged=OFF
--triggers
--skip-lock-tables
salt_test
```

O stdout foi enviado diretamente a um contador, sem persistir SQL cru.

Resultado:

```text
exit code: 0
stdout: 10.269 bytes
```

Isso comprova no macOS:

- resolução de `host.docker.internal`;
- conexão TCP ao mesmo host/porta usados pelo client do host;
- imagem selecionada por digest;
- option file montado read-only;
- execução de `mysql` e `mysqldump`;
- stdout utilizável como stream;
- ausência de SQL cru persistente no teste;
- remoção automática do client container por `--rm`.

## O que ainda falta para concluir RDB-002

- [ ] repetir com Docker Engine nativo em Linux;
- [ ] validar `host-gateway` em Linux;
- [ ] validar conexão a um host externo comum;
- [ ] validar conexão pela VPN usada para acessar o source Salt;
- [ ] capturar stdout e stderr concorrentemente pelo protótipo Rust;
- [ ] cancelar uma execução lenta e confirmar exit code/process cleanup;
- [ ] testar erro de DNS, TCP e TLS;
- [ ] confirmar que o option file não é gravável dentro do container;
- [ ] testar senha com caracteres especiais conforme RDB-004.

## Decisão provisória

O runtime de client em Docker permanece como escolha preferida para o MVP porque funcionou no caminho macOS local e elimina instalação de `mysql`/`mysqldump` no host.

Essa decisão só se torna definitiva depois do teste Linux e da VPN. Se a VPN não encaminhar tráfego originado por Docker, será aberta uma ADR comparando:

1. client Docker com configuração de rede específica;
2. client host descoberto e validado;
3. distribuição gerenciada de binários MySQL para macOS/Linux.

Não será implementada busca aberta na internet por executáveis.
