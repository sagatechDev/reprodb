# Matriz de fault injection

Esta matriz é o contrato da RDB-061. Os testes são determinísticos e não dependem de desligar o Docker real, lotar o disco do developer ou expor credenciais verdadeiras.

| Falha simulada | Fronteira testada | Resultado seguro esperado |
|---|---|---|
| senha inválida | classificação da conexão do client MySQL | `AuthenticationFailed`, sem copiar stderr ou senha |
| host inválido | classificação da conexão do client MySQL | `SourceNetworkUnavailable`, orientação para revisar conexão/VPN |
| porta sem listener | classificação da conexão do client MySQL | `SourceNetworkUnavailable`, sem repetir endpoint sensível |
| permissão insuficiente | dump e restore | categoria `Permission`, mensagem orientada à policy necessária |
| conexão perdida no stream | leitura da compressão e classificação do dump | dump falha, `.part` é descartado e dump anterior é preservado |
| disco cheio | writer Zstd injetado | erro recomenda verificar espaço/permissões; nunca publica sucesso |
| stderr maior que o limite | reader assíncrono compartilhado | drena o pipe inteiro, retém no máximo 64 KiB e marca truncamento |
| Docker indisponível | `ProcessRunner` fake | `DockerUnavailable`, distinto de imagem ausente |
| container configurado ausente | attestation fake do target | falha antes de produzir `AuthorizedLocalTarget` |
| imagem do client ausente | catálogo/runtime com `ProcessRunner` fake | read-only não baixa; fluxo mutável baixa somente a imagem aprovada |
| Zstd ou checksums corrompidos | validator do artefato | rejeita antes de qualquer `DROP DATABASE` |
| rename final falha | colisão não substituível no artifact store | remove o stage e não torna o dump visível no cache |
| Ctrl+C | token, compressor, dump service e restore engine | child/lock/partial limpos, estado local `incomplete`, exit 130 |

Testes centrais:

- `infrastructure::mysql::docker_client::tests::invalid_password_host_and_port_are_classified_without_retaining_diagnostics`
- `infrastructure::process::tests::bounded_reader_drains_large_diagnostics_without_retaining_them`
- `infrastructure::compression::tests::reports_a_full_disk_without_claiming_compression_success`
- `infrastructure::artifact_store::tests::failed_publish_rename_cleans_the_stage_and_never_exposes_a_complete_dump`
- `infrastructure::restore_artifact::tests::corruption_is_rejected_before_a_validated_artifact_exists`
- `application::local_target_gate::tests::configured_container_absence_is_reported_before_restore_authorization`

Erros externos são reduzidos a categorias tipadas. O stderr serve apenas para classificação limitada e nunca integra `Display`/`Debug` dos erros devolvidos ao usuário.
