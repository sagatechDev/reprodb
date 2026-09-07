# Configuração do target Docker

`reprodb setup` configura o MySQL local que receberá restores. O fluxo atual trabalha com containers existentes e foi validado no macOS/Docker Desktop contra o container `mysql-8`.

```bash
reprodb setup
```

O comando:

1. detecta o Docker context atual e exige um endpoint local por Unix socket;
2. lista e inspeciona os containers por JSON estruturado;
3. considera candidatos com imagem MySQL, porta `3306/tcp` ou label do reprodb;
4. mostra imagem, estado, healthcheck e portas publicadas;
5. alerta quando a porta está publicada em `0.0.0.0` ou `::`;
6. permite escolher o target interativamente;
7. pede usuário, senha mascarada por `*`, database central local e prefixo permitido para databases de tenant;
8. prepara o client MySQL aprovado;
9. conecta ao namespace de rede do container pelo ID completo e valida versão/vendor;
10. salva o ID, nome, context, tipo de confiança, allowlist e chave da credencial no TOML, mantendo a senha no credential store do sistema;
11. torna o container escolhido o target default e preserva os outros targets já configurados.

O ID completo é a identidade de segurança. O nome fica salvo para apresentação, mas `reprodb restore` recusa um container recriado com o mesmo nome e outro ID.

O prefixo sugerido é `salt_`: ele permite `salt_sagatec` e `salt_polymer`, mas a barreira de restore recusa o `salt_central` configurado. Depois do setup, context, identidade, label quando aplicável, conexão, vendor e versão são atestados novamente antes de qualquer operação destrutiva.

Contrato completo: [`docs/local-target-safety.md`](local-target-safety.md).

Containers parados aparecem na lista. Se um deles for escolhido, a CLI pede confirmação, inicia exatamente o ID selecionado e tenta a conexão por até 10 segundos enquanto o MySQL fica pronto. O MVP configura containers existentes; criação e administração automática de um target dedicado ficam para uma conveniência futura.

Cada container precisa passar pelo `setup` uma vez para que identidade, política e credencial sejam verificadas. Escolher um container ainda não cadastrado o adiciona e o torna default sem remover os anteriores. Escolher novamente um container cadastrado pede confirmação antes de substituir somente a configuração e credencial desse target. A nova configuração e credencial são publicadas antes da remoção da credencial antiga; uma falha de cleanup informa a chave órfã sem desfazer o target válido.

Com dois targets configurados, o `pull` interativo pergunta qual receberá o restore. Para automação, use `--target CONTAINER`.

## Limites atuais

- execução real em Docker Engine Linux ainda precisa ser validada;
- apenas a série MySQL 8.4 está no catálogo aprovado;
- criação automática de container não pertence ao MVP;
- `setup` não altera automaticamente um bind exposto em todas as interfaces;
- o comando não cria usuário MySQL.

O preview sem efeitos colaterais continua disponível:

```bash
reprodb setup --preview
```

Depois do setup, valide a identidade e a conexão sem alterar o container:

```bash
reprodb doctor
```
