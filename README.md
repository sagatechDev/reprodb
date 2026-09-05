# reprodb

CLI local em Rust para obter dumps MySQL de tenants do Salt, mantê-los em cache temporário e restaurá-los num MySQL Docker para reprodução de problemas.

O projeto está no início da implementação. O backlog e as decisões técnicas ficam em:

- [`docs/implementation-roadmap.md`](docs/implementation-roadmap.md);
- [`docs/adr`](docs/adr);
- [`docs/research`](docs/research).
- [`docs/exit-codes.md`](docs/exit-codes.md).

## Desenvolvimento

Requisitos atuais:

- Rust 1.85 ou mais recente;
- macOS ou Linux;
- Docker para os spikes de integração.

Validação local:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

Os spikes são deliberadamente separados do crate principal e não representam a arquitetura final da aplicação.
