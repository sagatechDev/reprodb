# Source profiles

Um source profile descreve uma conexão MySQL da qual o reprodb poderá gerar dumps. Senhas não pertencem ao profile serializado: o TOML guarda somente a chave da entrada correspondente no Keychain do macOS ou no Secret Service do Linux.

## Comandos

O ciclo de vida completo do profile é funcional:

```bash
reprodb profile add salt-source
reprodb profile list
reprodb profile use NAME
reprodb profile remove NAME
```

`profile add` pergunta host, porta, usuário e senha. Durante a senha, a CLI mostra um `*` por caractere para deixar a digitação visível sem revelar seu conteúdo. Texto colado também é aceito; use o atalho do seu terminal (`Cmd+V` normalmente no macOS e `Ctrl+Shift+V` normalmente no Linux). Em seguida a CLI pergunta se o source é produção e define a política TLS — `REQUIRED` é o default e é obrigatório para produção.

Antes de salvar, o comando:

1. descobre e valida o Docker context atual;
2. prepara a imagem de client aprovada e fixada por digest;
3. cria um option file temporário privado com a política TLS;
4. testa a conexão e detecta versão/vendor do servidor;
5. recusa uma série ainda ausente do catálogo;
6. salva a credencial e persiste o profile de forma transacional;
7. torna o novo profile ativo.

Falha de Docker, rede, autenticação, versão ou configuração não deixa um profile salvo. Se o TOML falhar depois que o credential store for atualizado, o fluxo tenta apagar imediatamente a credencial nova.

`profile list` é somente leitura e mostra apenas nome, endpoint, série MySQL, política e indicação do profile ativo. Ele não consulta nem revela credenciais.

`profile use` valida que o nome existe antes de trocar o profile ativo e persiste o TOML por escrita transacional.

`profile remove` pede confirmação, exceto com `--yes`. A configuração deixa de referenciar a credencial antes de o credential store ser alterado. Remover o profile ativo deixa o projeto sem profile selecionado e a saída explica como escolher o próximo.

Se a credencial já não existir, a remoção do profile termina com aviso. Se o credential store falhar, o comando informa a chave órfã e retorna exit code `11`, sem restaurar um profile removido nem mostrar o segredo.

Ainda é possível avaliar a experiência sem criar estado:

```bash
reprodb profile add salt-source --preview
```

O catálogo inicial aceita o source MySQL 8.4 validado no ambiente Salt. A detecção é automática, mas uma série diferente falha de forma explícita até possuir combinação de client, imagem e testes aprovada.
