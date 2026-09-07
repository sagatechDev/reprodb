# reprodb

CLI local em Rust para obter dumps MySQL de tenants do Salt, mantê-los em cache temporário e restaurá-los num MySQL Docker para reprodução de problemas.

O fluxo local está funcionalmente implementado e validado no macOS; o MVP ainda aguarda os gates de Docker Engine, Secret Service e E2E real no Linux. O backlog e as decisões técnicas ficam em:

- [`docs/implementation-roadmap.md`](docs/implementation-roadmap.md);
- [`docs/adr`](docs/adr);
- [`docs/research`](docs/research).
- [`docs/exit-codes.md`](docs/exit-codes.md).
- [`docs/profiles.md`](docs/profiles.md), para o ciclo de vida dos source profiles;
- [`docs/setup.md`](docs/setup.md), para configurar o target Docker local;
- [`docs/doctor.md`](docs/doctor.md), para diagnosticar o ambiente sem alterá-lo;
- [`docs/tenant-resolution.md`](docs/tenant-resolution.md), para lookup seguro de tenant/database;
- [`docs/restore-engine.md`](docs/restore-engine.md), para as barreiras e o pipeline streaming de restore;
- [`docs/restore-command.md`](docs/restore-command.md), para executar um restore gerenciado por dump ID;
- [`docs/pull-command.md`](docs/pull-command.md), para o fluxo completo de cache, dump e restore;
- [`docs/cache-commands.md`](docs/cache-commands.md), para inspecionar e limpar os dumps sob `~/.reprodb`;
- [`docs/local-tenant-registration.md`](docs/local-tenant-registration.md), para o cadastro seguro pós-restore no `salt_central` local;
- [`docs/ui-ux-test-guide.md`](docs/ui-ux-test-guide.md), para experimentar a CLI sem alterar a máquina.
- [`docs/macos-smoke.md`](docs/macos-smoke.md), para validar a integração local no macOS.
- [`docs/performance-benchmark.md`](docs/performance-benchmark.md), para medir dump, restore, cache, compressão e memória.
- [`docs/local-mvp-definition-of-done.md`](docs/local-mvp-definition-of-done.md), para o estado auditado e os gates restantes do MVP local.

## Desenvolvimento

Requisitos atuais:

- Rust 1.88 ou mais recente;
- macOS ou Linux;
- Docker para os spikes de integração.

Validação local:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

Os spikes são deliberadamente separados do crate principal e não representam a arquitetura final da aplicação.

## Prévia da experiência

Também é possível percorrer a UX planejada sem acessar Docker, MySQL, Keychain ou arquivos de configuração:

```bash
./scripts/preview-cli.sh
```

Cada saída simulada também pode ser executada separadamente com `--preview`. Consulte o [roteiro de teste de UI/UX](docs/ui-ux-test-guide.md).
