# Source profiles

Um source profile descreve uma conexão MySQL da qual o reprodb poderá gerar dumps. Senhas não pertencem ao profile serializado: o TOML guarda somente a chave da entrada correspondente no Keychain do macOS ou no Secret Service do Linux.

## Estado atual

Os seguintes comandos já são funcionais:

```bash
reprodb profile list
reprodb profile use NAME
reprodb profile remove NAME
```

`profile list` é somente leitura e mostra apenas nome, endpoint, série MySQL, política e indicação do profile ativo. Ele não consulta nem revela credenciais.

`profile use` valida que o nome existe antes de trocar o profile ativo e persiste o TOML por escrita transacional.

`profile remove` pede confirmação, exceto com `--yes`. A configuração deixa de referenciar a credencial antes de o credential store ser alterado. Remover o profile ativo deixa o projeto sem profile selecionado e a saída explica como escolher o próximo.

Se a credencial já não existir, a remoção do profile termina com aviso. Se o credential store falhar, o comando informa a chave órfã e retorna exit code `11`, sem restaurar um profile removido nem mostrar o segredo.

O cadastro real ainda está em implementação. Para avaliar sua experiência sem criar estado:

```bash
reprodb profile add salt-source --preview
```

O próximo incremento adicionará os prompts reais, transporte da senha sem eco, política TLS, detecção da versão do source e teste da conexão antes de salvar qualquer coisa.
