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
7. pede usuário, senha mascarada por `*` e database central local;
8. prepara o client MySQL aprovado;
9. conecta ao namespace de rede do container pelo ID completo e valida versão/vendor;
10. salva o ID, nome, context e chave da credencial no TOML, mantendo a senha no credential store do sistema.

O ID completo é a identidade de segurança. O nome fica salvo para apresentação, mas um restore futuro deverá recusar um container recriado com o mesmo nome e outro ID.

Containers parados aparecem na lista. Se um deles for escolhido, a CLI pede confirmação, inicia exatamente o ID selecionado e tenta a conexão por até 10 segundos enquanto o MySQL fica pronto. A criação de um container dedicado pelo próprio reprodb continua pendente na RDB-025.

Se já houver um target, o comando pede confirmação antes de substituí-lo. A nova configuração e credencial são publicadas antes da remoção da credencial antiga; uma falha de cleanup informa a chave órfã sem desfazer o target válido.

## Limites atuais

- execução real em Docker Engine Linux ainda precisa ser validada;
- apenas a série MySQL 8.4 está no catálogo aprovado;
- criação automática de um container novo ainda não foi implementada;
- `setup` não altera automaticamente um bind exposto em todas as interfaces;
- o comando não cria usuário MySQL.

O preview sem efeitos colaterais continua disponível:

```bash
reprodb setup --preview
```
