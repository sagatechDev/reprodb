# ADR 0001 — Transporte de credenciais para clients MySQL

- Status: aceita
- Data: 5 de setembro de 2026
- Issue: RDB-004

## Contexto

O reprodb precisa usar a mesma credencial em `mysql` e `mysqldump`, executados em containers efêmeros. A senha não pode aparecer:

- no TOML persistente;
- nos argumentos do reprodb, Docker ou client MySQL;
- em `Debug`, logs ou mensagens de erro;
- no SQL transportado;
- em uma variável de ambiente visível por `docker inspect`.

O stdin também não pode transportar a senha porque ele carrega o SQL durante o restore.

Option files MySQL são texto puro. Eles não são criptografia at-rest; servem aqui como um transporte temporário de vida curta entre o keyring e o client container.

## Decisão

O fluxo será:

```text
prompt sem echo
  -> SecretString
  -> OS credential store

execução
  -> keyring get
  -> SecretString
  -> option file temporário 0600
  -> bind mount read-only
  -> --defaults-file como primeiro argumento do client
  -> mysql/mysqldump
  -> wait do processo
  -> remoção pelo guard
```

### Identidade persistente

O keyring usa:

```text
service = com.sagatech.reprodb
account = source:<credential-uuid>
```

ou:

```text
account = target:<credential-uuid>
```

O TOML guarda somente o UUID estável em `credential_key`. O nome editável do profile não será a identidade da credencial.

Source e target sempre possuem entradas diferentes, mesmo que hoje usem a mesma senha.

### Contrato Rust

Na aplicação, o contrato continua assíncrono para não bloquear o runtime enquanto macOS Keychain ou Linux Secret Service interagem com o sistema:

```rust
#[async_trait]
trait CredentialStore: Send + Sync {
    async fn get(&self, key: &CredentialKey) -> Result<SecretString>;
    async fn set(&self, key: &CredentialKey, value: SecretString) -> Result<()>;
    async fn delete(&self, key: &CredentialKey) -> Result<()>;
}
```

`OsCredentialStore` encapsula a API síncrona do crate `keyring` em `spawn_blocking`. `MemoryCredentialStore` é determinístico e não acessa o SO nos testes.

Nenhum dos tipos de erro possui campo com `SecretString` ou texto retornado pelo usuário.

### Option file temporário

O arquivo contém somente um grupo `[client]`:

```ini
[client]
host="..."
port=3306
user="..."
password="..."
protocol=TCP
```

Regras obrigatórias:

1. criar dentro de diretório temporário privado;
2. usar criação exclusiva, sem substituir path existente;
3. aplicar modo `0600` já no `open` em macOS/Linux;
4. escrever valores entre aspas e escapar backspace, tab, newline, carriage return, backslash e aspas duplas;
5. rejeitar NUL com erro que nomeia apenas o campo;
6. executar `flush` e `sync_all` antes do mount;
7. montar em `/run/secrets/reprodb.cnf` com `readonly`;
8. passar `--defaults-file=/run/secrets/reprodb.cnf` antes das demais opções;
9. adicionar `--no-login-paths` quando suportado pela série aprovada, evitando estado implícito do client;
10. manter o guard vivo até `kill`/`wait` do processo filho;
11. remover o arquivo em sucesso, erro, cancelamento e unwind.

O serializer é responsável por sintaxe, mas host e username continuam sujeitos à validação de domínio própria.

### Operações transacionais de profile

Em `profile add`:

1. coletar e validar todos os campos;
2. gerar um UUID novo;
3. salvar a credencial;
4. persistir o TOML por arquivo parcial + rename;
5. se a persistência falhar, apagar a nova entrada do keyring;
6. se o rollback também falhar, reportar a chave órfã sem exibir a senha.

Em `profile remove`, a configuração deixa de referenciar a chave antes da tentativa de apagar o keyring. Falha no delete gera aviso de credencial órfã e instrução de limpeza, sem restaurar um profile parcialmente removido.

## Evidência experimental

O spike em `spikes/streaming-pipeline` implementa o serializer e executa:

```bash
./run-credential-test.sh
```

O teste autenticou com MySQL 8.4.4 usando uma senha contendo:

- espaços no início e fim;
- aspas simples e duplas;
- `#` e `;`;
- barra invertida;
- newline, tab e carriage return.

Resultado:

```text
special_character_authentication=ok
option_file_mode=600
option_file_mount_read_only=yes
secret_in_container_config=no
secret_in_authentication_error=no
```

Os testes Rust também verificam `create_new`, modo `0600`, escaping, rejeição de NUL e ausência do segredo em `Debug`/erro.

A implementação incorporada ao crate principal adiciona ainda:

- `OsCredentialStore` sobre `keyring` 4, executado via `spawn_blocking`;
- `MemoryCredentialStore` sem acesso ao sistema operacional;
- preflight que impede sobrescrever uma credencial existente;
- rollback quando a persistência transacional do TOML falha;
- erro com a chave órfã quando config e rollback falham;
- guard que remove o diretório privado e o option file no `Drop`.

Em 5 de setembro de 2026, o teste ignorado `native_store_roundtrips_a_temporary_credential` passou localmente no Keychain do macOS, salvando, lendo e apagando uma entrada UUID temporária. O crate também passou em `cargo check` para `x86_64-unknown-linux-gnu` no MSRV; a execução real do mesmo teste permanece pendente em um desktop Linux com Secret Service disponível. Testes comuns usam somente o backend em memória.

## Alternativas rejeitadas

### Senha em argumento

Rejeitada porque aparece em argv, ferramentas de diagnóstico e possivelmente histórico/logs.

### `MYSQL_PWD` ou variável equivalente

Rejeitada porque o ambiente do container pode ser inspecionado e porque amplia os lugares que precisam de redaction.

### Senha no TOML

Rejeitada porque transforma uma configuração durável e copiável em secret store.

### `mysql_config_editor`

Rejeitada no MVP porque exige outro processo e um login path persistente, adiciona estado implícito e não elimina a necessidade de lifecycle seguro. A ofuscação do login file não substitui o keyring do SO.

### Prompt do próprio client

Rejeitado porque quebra automação e conflita com stdin usado no restore.

### Docker secrets

Rejeitado porque o MVP usa containers standalone locais e não deve introduzir Swarm ou infraestrutura adicional.

## Consequências

- a senha existe brevemente em memória e em texto puro num arquivo `0600`; isso é compatível com o threat model local, mas precisa de cleanup rigoroso;
- controle total da máquina continua permitindo captura do secret;
- crash recuperável é limpo pelo guard; `SIGKILL` e queda da máquina exigem limpeza oportunista de temporários antigos no startup;
- testes de macOS Keychain e Linux Secret Service continuam pertencendo à RDB-022;
- o backend Linux ausente ou bloqueado precisa produzir erro acionável no `doctor`, nunca fallback silencioso para arquivo.

## Referências

- [MySQL 8.4 — Using Option Files](https://dev.mysql.com/doc/refman/8.4/en/option-files.html)
- [MySQL 8.4 — Options that affect option-file handling](https://dev.mysql.com/doc/refman/8.4/en/option-file-options.html)
- [secrecy 0.10 — SecretString](https://docs.rs/secrecy/0.10.3/secrecy/type.SecretString.html)
- [keyring](https://docs.rs/keyring/latest/keyring/)
