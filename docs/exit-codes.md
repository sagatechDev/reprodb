# Exit codes do reprodb

Os códigos abaixo fazem parte da interface da CLI. Novas mensagens podem ser adicionadas dentro de uma categoria, mas o código da categoria deve permanecer estável.

| Código | Categoria | Uso |
|---:|---|---|
| 0 | sucesso | Comando concluído |
| 1 | geral | Falha interna ou funcional sem categoria mais específica |
| 2 | uso | Argumentos ou subcommands inválidos; emitido pelo `clap` |
| 10 | configuração | Config ausente, inválida ou inconsistente |
| 11 | credencial | Keyring ou credencial indisponível |
| 20 | dependência | Executável, imagem ou requisito local ausente |
| 30 | conexão source | DNS, TCP, TLS ou autenticação no source |
| 31 | resolução de tenant | Alias, tenant ou database não resolvido/permitido |
| 40 | dump | Preflight ou `mysqldump` falhou |
| 50 | cache/filesystem | Artefato, lock, checksum, espaço ou I/O local |
| 60 | Docker | Context, container ou runtime inválido |
| 70 | restore | Recriação/importação/validação local falhou |
| 130 | interrompido | Ctrl+C/SIGINT tratado pela aplicação |

No fluxo de restore, UUID inválido é erro de uso (`2`); artefato ausente, duplicado ou corrompido pertence ao cache (`50`); identidade do target pertence a Docker/configuração (`60`/`10`); e falha depois do início da recriação/importação pertence ao restore (`70`).

## Regras de mensagem e logging

- mensagens para o usuário vão a stderr e precisam sugerir uma ação quando possível;
- logs de diagnóstico são desabilitados por padrão e ativados por `RUST_LOG`, por exemplo `RUST_LOG=reprodb=debug`;
- senha, conteúdo SQL, JSON central completo e argv do processo nunca são campos de erro ou tracing;
- erros de processo podem incluir stderr limitado e redigido, nunca o comando completo;
- tipos que carregam secrets não implementam `Display` e usam `secrecy::SecretString` para `Debug` redigido;
- input do usuário só aparece quando necessário e depois de validado; mensagens de autenticação não repetem username/password.

`ErrorCategory::Interrupted` reserva 130 desde o bootstrap, mesmo antes da implementação completa de sinais na RDB-060.
