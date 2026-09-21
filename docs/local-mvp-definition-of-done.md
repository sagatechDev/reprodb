# Auditoria do Definition of Done do MVP local

> Revisão realizada em 7 de setembro de 2026. Esta auditoria trata somente do produto local; nenhuma conexão ou ação em produção faz parte dela.

## Resultado

O núcleo do MVP está implementado e funciona no macOS usado no desenvolvimento. O fluxo real já cobre:

```text
setup de target existente
  -> profile source interativo
  -> mysqldump MySQL 8.4
  -> Zstd/cache atômico
  -> restore em outro MySQL Docker
  -> novo pull usando cache com o source desligado
```

O MVP não deve ser declarado concluído para distribuição interna enquanto os três gates Linux não forem executados. A implementação possui 17 de 20 critérios aceitos; o que falta é evidência cross-platform, não funcionalidade central nova.

## Decisão de escopo do setup

O requisito informado para `reprodb setup` é descobrir os containers MySQL existentes, apresentar opções e permitir que o desenvolvedor escolha o target. Esse fluxo está implementado.

Criar, atualizar ou administrar automaticamente um container MySQL não é necessário para reproduzir o fluxo atual e adicionaria decisões sobre volume, porta, senha, lifecycle e imagem. Essa conveniência fica fora do MVP. O target escolhido continua protegido por context local, ID completo, versão/vendor, conexão real e confirmação explícita.

## Evidências concluídas

| Área | Evidência atual |
|---|---|
| CLI e UX | Árvore completa, prompts interativos, senha com `*`, paste, cores opcionais, preview e exit codes testados |
| Configuração | Tudo sob `~/.reprodb`, schema estrito, locks, escrita atômica e permissões privadas |
| Source | Profile testado antes de persistir; versão/vendor e TLS observados por conexão real |
| Client | MySQL 8.4 aprovado por catálogo e digest, executado em container sem senha no argv |
| Dump | Preflight conservador, policy v2, flags estruturadas, streaming e ETA aproximado |
| Cache | Zstd, SHA-256, TTL, fingerprint do source/policy, staging `.part`, rename atômico e locks |
| Restore | Artefato validado antes do `DROP`, target local atestado e source/target com UUID diferentes |
| Pull | E2E real A → B e cache hit com source desligado no Docker Desktop/macOS |
| Resiliência | Ctrl+C, disco cheio, corrupção, falhas de processo, cleanup e memória constante testados |
| Salt local | Restore comparado para FK, `DECIMAL`, `DATETIME`, `BLOB`, `NULL` e UTF-8; registro central mínimo criado |

## Três gates restantes

### 1. Docker Engine Linux e networking

Executar numa máquina Linux real, não apenas dentro de um container Linux no Docker Desktop:

```bash
cargo test --test docker_client_integration -- --ignored --nocapture
cargo test --test docker_discovery_integration -- --ignored --nocapture
cargo test --test setup_integration -- --ignored --nocapture
```

O aceite exige descoberta do context Unix local, acesso do client container ao source publicado no host, seleção/atestation do target e ausência de dependência específica do Docker Desktop.

VPN e acesso a qualquer source externo não fazem parte deste aceite. Eles pertencem à habilitação posterior daquele source profile e não devem atrasar nem contaminar a prova local.

### 2. credential store local Linux

Executar numa sessão de usuário Linux com backend credential store local disponível:

```bash
cargo test native_store_roundtrips_a_temporary_credential -- --ignored --nocapture
```

Depois, percorrer `profile add`, `profile list` e `profile remove` com uma credencial sintética. O E2E de CI usa propositalmente um credential store de arquivo compilado apenas para testes; portanto ele não comprova esta integração nativa.

### 3. E2E do binário real no Linux

Executar o job `MySQL A to B (Linux)` de `.github/workflows/ci.yml`, que sobe source e target MySQL 8.4 isolados e atravessa os processos reais de `setup`, `profile add` e `pull`:

```bash
cargo test \
  --features test-file-credential-store \
  --test pull_integration \
  real_cli_configures_pulls_and_reuses_cache_with_the_source_offline \
  -- --ignored --nocapture
```

O aceite exige primeiro `pull` por dump, restore no target, source desligado, segundo `pull` por cache e comparação final dos dados.

## Ordem para encerrar o MVP

1. disponibilizar o repositório para um runner Linux ou executar os comandos numa máquina Linux de desenvolvimento;
2. fechar o gate Docker/networking;
3. fechar o gate credential store local na sessão real do usuário;
4. executar o E2E Linux;
5. registrar SO, arquitetura, Docker, MySQL e resultado sem incluir hosts ou credenciais;
6. somente então marcar os três checkboxes restantes e declarar o MVP local concluído.

Nenhuma dessas etapas exige profile de produção. Source e target devem continuar sendo MySQLs locais e descartáveis.
