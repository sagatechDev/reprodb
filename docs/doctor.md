# Diagnóstico do ambiente

`reprodb doctor` verifica se o ambiente está pronto antes de um dump ou restore. O comando agrega os resultados: uma falha de credencial, por exemplo, não impede checks independentes de Docker e disco.

```bash
reprodb doctor
```

Os checks reais cobrem:

- arquivo de configuração e profile ativo;
- credenciais do source e target no credential store do sistema;
- espaço disponível no filesystem do cache, com piso inicial de 5 GiB;
- disponibilidade e identidade do Docker context local;
- nome, ID completo e estado do container target configurado;
- presença local da imagem de client MySQL aprovada;
- conexão somente-leitura com source e target;
- compatibilidade entre séries de server e client;
- versão, vendor e cipher TLS negociado.

O `doctor` não baixa imagens, inicia o target, escreve configuração nem altera bancos. Para testar as conexões ele executa containers efêmeros `--rm` da imagem de client já aprovada e disponível localmente, além de option files temporários privados. Quando a imagem ainda não existe localmente, o check orienta executar `profile add` ou `setup` enquanto houver acesso à internet; esses são os fluxos responsáveis por preparar o client.

Cada linha combina cor e símbolo para continuar legível com `--color never` ou `NO_COLOR=1`:

```text
✓ passou
! aviso não bloqueante
✗ falhou
– não executado porque uma dependência anterior falhou
→ ação recomendada
```

Uma conexão sem TLS em profile local não-production é exibida como aviso. Em profile de produção ela é falha. O target local é verificado com TLS obrigatório.

## Exit code

Sucesso retorna `0`. Quando há falhas, o código representa a primeira categoria requerida na ordem da apresentação: configuração (`10`), credencial (`11`), dependência (`20`), source (`30`), filesystem (`50`) ou Docker (`60`). A tabela estável completa fica em [exit-codes.md](exit-codes.md).

## Limite do check de espaço

Os 5 GiB são apenas um piso de segurança para iniciar o MVP, não uma estimativa do dump. O tamanho comprimido e o espaço necessário pelo MySQL target variam por database. Um preflight posterior deverá usar metadata histórica/estimada para exigir espaço proporcional ao artefato selecionado.

## Teste sem efeitos externos

A apresentação simulada continua disponível:

```bash
reprodb doctor --preview
```

O teste de integração real usa configuração temporária e credential store em memória:

```bash
cargo test --test doctor_integration -- --ignored --nocapture
```

Ele exige o container local `mysql-8` em execução (ou `REPRODB_TEST_MYSQL_CONTAINER`) e não usa a configuração real do desenvolvedor.
