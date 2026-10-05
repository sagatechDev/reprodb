# `reprodb push` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `reprodb push <DATABASE>` imports a managed dump (cached or freshly created) into a database on another, non-production source profile.

**Architecture:** A parallel flow to the local restore. `RemoteTargetGate` turns a profile into an attested `AuthorizedRemoteTarget`; `DockerMysqlRemoteImportExecutor` streams the validated dump into an ephemeral `mysql` client container that connects with the profile's option file (host/TLS), reusing a streaming routine extracted from the local restore executor. `PushService` orchestrates selection → gate → optional fresh dump → validation → confirmation → lock → `CREATE DATABASE IF NOT EXISTS` → import. The local restore types (`AuthorizedLocalTarget`, `LocalTargetGate`, `RestoreEngine`) are not changed.

**Tech Stack:** Rust 1.88+, tokio, clap, dialoguer, thiserror, async-trait, Docker CLI running the approved MySQL client image.

**Spec:** `docs/superpowers/specs/2026-10-04-push-command-design.md`

## Global Constraints

- A profile with `production = true` is never a push destination, with no override flag.
- No `DROP DATABASE` on the destination. Only `CREATE DATABASE IF NOT EXISTS \`db\` CHARACTER SET <dump charset> COLLATE <dump collation>;` followed by the dump import.
- Refuse when destination `@@server_uuid` == dump `source_server_uuid` **and** destination database == dump source database. Same server with another database name is allowed.
- Version rule identical to local restore: same major, destination series ≥ source series, client series == server series.
- Interactive confirmation requires typing the destination database name; `--yes` skips it. Without a TTY every choice must come from flags, never a guess.
- "Generate a new dump now" uses the **active** profile as the source, exactly like `pull`.
- Exit codes: production destination / system database / source collision → `10`; credential → `11`; destination unreachable / TLS / auth on connection probe → `30`; cache/artifact → `50`; Docker → `60`; version mismatch, CREATE/import failure, missing privilege, imported size mismatch → `70`; Ctrl+C → `130`.
- No password, option-file content or raw mysql stderr in any output or error message.
- Match surrounding code: `thiserror` enums, `async_trait`, `#[cfg(test)]` helpers named `for_test`, `rtk`-free plain `cargo` commands.
- Gate before every commit: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`.

## Review Focus

1. **"Access denied … to database"** (MySQL error 1044) on the destination is a missing privilege, not bad credentials — the user must be told about privileges, not to re-add the profile. → test in Task 4.
2. **Non-interactive run without `--profile` or `--yes`** must fail with the flag to pass, never pick a profile or skip confirmation. → tests in Task 6.
3. **`--profile` naming a production profile** must fail with a message saying production profiles are never destinations (not "profile not found"). → test in Task 6.
4. **`--dump-id` of a dump that belongs to another database** must fail before connecting anywhere. → test in Task 6.
5. **Ctrl+C mid-import** must exit `130` and the message must tell the user the remote database may be partial and a rerun imports over it. → test in Task 6 (category) and Task 4 (message).

---

## File Structure

| File | Responsibility |
|---|---|
| `src/infrastructure/mysql/restore_executor.rs` (modify) | Extract `stream_import` + `StreamedImport`; expose `classify_failure`, `run_without_stdin` to sibling modules |
| `src/infrastructure/restore_artifact.rs` (modify) | `#[cfg(test)] test_support::validated_artifact` fixture shared by tests |
| `src/application/restore_engine.rs` (modify) | `versions_are_restore_compatible` becomes `pub(crate)` |
| `src/application/remote_target_gate.rs` (create) | `RemoteTargetProbe` trait, `RemoteTargetGate`, `GuardedRemoteTarget`, `AuthorizedRemoteTarget`, `RemoteProfileChoice`, `RemoteTargetGateError` |
| `src/infrastructure/mysql/remote_target_probe.rs` (create) | `DockerRemoteTargetProbe` — prepares client, writes option file, probes server identity |
| `src/infrastructure/mysql/remote_import_executor.rs` (create) | `RemoteImportExecutor` trait, `DockerMysqlRemoteImportExecutor`, `RemoteImportError`, `RemoteFailureKind` |
| `src/infrastructure/operation_lock.rs` (modify) | `OperationLockScope::Remote`, `OperationLockKey::remote` |
| `src/application/restore_service.rs` (modify) | `dump_choices` becomes `pub(crate)` |
| `src/application/push_service.rs` (create) | `PushService`, `PushSelector`, `PushDumpChoice`, `PushPlan`, `PushProgress`, `PushOutcome`, `PushReady`, `PushServiceError`, `PushSelectionError` |
| `src/cli/push.rs` (create) | `CliPushSelector`, `CliPushProgress`, render functions |
| `src/cli/prompt.rs` (modify) | `select_push_profile`, `select_push_dump`, `select_push_database`, `confirm_push_database` |
| `src/cli/mod.rs`, `src/lib.rs`, `src/error.rs` (modify) | `Commands::Push(PushArgs)`, dispatch, exit-code mapping |
| `tests/pull_integration.rs` (modify) | `#[ignore]` real push between two MySQL containers |
| `docs/push-command.md` (create), `README.md`, `docs/exit-codes.md` (modify) | Docs |

---

### Task 1: Extract the shared import streaming routine

**Files:**
- Modify: `src/infrastructure/mysql/restore_executor.rs:109-218` (body of `DockerMysqlRestoreExecutor::import`), `:316` (`run_without_stdin`), `:412` (`classify_failure`)
- Modify: `src/infrastructure/restore_artifact.rs` (append test support module)

**Interfaces:**
- Produces (visible to sibling modules under `infrastructure::mysql`):
  - `pub(super) struct StreamedImport { pub(super) status: std::process::ExitStatus, pub(super) diagnostic: BoundedBytes, bytes: u64, sha256: Sha256Digest }`
  - `impl StreamedImport { pub(super) fn verified_metrics(&self, artifact: &ValidatedRestoreArtifact) -> Result<RestoreMetrics, RestoreExecutorError> }`
  - `pub(super) async fn stream_import(spec: &ProcessSpec, artifact: &ValidatedRestoreArtifact, cancellation: &CancellationToken, docker_context: &str, operation_container: &str) -> Result<StreamedImport, RestoreExecutorError>`
  - `pub(super) async fn run_without_stdin(...)` (unchanged signature, visibility widened)
  - `pub(super) fn classify_failure(stderr: &[u8]) -> RestoreFailureKind` (visibility widened)
  - `#[cfg(test)] pub(crate) async fn crate::infrastructure::restore_artifact::test_support::validated_artifact(cache_root: &Path, sql: &[u8]) -> ValidatedRestoreArtifact`

- [ ] **Step 1: Add the shared test fixture**

Append to `src/infrastructure/restore_artifact.rs`:

```rust
#[cfg(test)]
pub(crate) mod test_support {
    use std::{io::Cursor, path::Path, sync::Arc};

    use crate::{
        domain::{
            DatabaseEncoding, DatabaseName, DumpArtifactCompletion, DumpArtifactContext,
            DumpArtifactMetadata, MysqlVersion, ProfileName, Sha256Digest,
        },
        infrastructure::{
            artifact_store::LocalArtifactStore,
            compression::{NoCompressionProgress, ZstdCompressor},
        },
    };

    use super::{LocalRestoreArtifactValidator, RestoreArtifactRequest, ValidatedRestoreArtifact};

    /// Publishes `sql` as a complete `local-source/acme_production` dump made
    /// on server `11111111-…` (MySQL 8.4.4) and returns it validated.
    pub(crate) async fn validated_artifact(cache_root: &Path, sql: &[u8]) -> ValidatedRestoreArtifact {
        let profile = ProfileName::try_from("local-source").unwrap();
        let database = DatabaseName::try_from("acme_production").unwrap();
        let store = LocalArtifactStore::new(cache_root);
        let stage = store.begin(&profile, &database).unwrap();
        let dump_id = stage.dump_id();
        let metrics = ZstdCompressor::default()
            .compress(
                Cursor::new(sql.to_vec()),
                stage.create_dump_writer().unwrap(),
                Arc::new(NoCompressionProgress),
            )
            .await
            .unwrap();
        let metadata = DumpArtifactMetadata::try_new(
            dump_id,
            DumpArtifactContext {
                database: database.clone(),
                profile: profile.clone(),
                source_fingerprint: Sha256Digest::from_bytes([1; 32]),
                source_server_uuid: "11111111-1111-4111-8111-111111111111".parse().unwrap(),
                source_version: "8.4.4".parse::<MysqlVersion>().unwrap(),
                client_version: "8.4.4".parse::<MysqlVersion>().unwrap(),
                database_encoding: DatabaseEncoding::try_new(
                    "utf8mb4".to_owned(),
                    "utf8mb4_0900_ai_ci".to_owned(),
                )
                .unwrap(),
                policy_version: 1,
            },
            DumpArtifactCompletion {
                created_at_unix_seconds: 100,
                completed_at_unix_seconds: 101,
                uncompressed_bytes: metrics.input_bytes(),
                compressed_bytes: metrics.compressed_bytes(),
                sql_sha256: metrics.input_sha256(),
                artifact_sha256: metrics.compressed_sha256(),
            },
        )
        .unwrap();
        stage.publish(&metadata, &metrics).unwrap();
        LocalRestoreArtifactValidator::new(cache_root)
            .validate(RestoreArtifactRequest {
                profile: &profile,
                database: &database,
                dump_id,
            })
            .await
            .unwrap()
    }
}
```

- [ ] **Step 2: Write the failing streaming test**

Add to the `tests` module of `src/infrastructure/mysql/restore_executor.rs`:

```rust
    #[tokio::test]
    async fn stream_import_feeds_the_whole_validated_dump_to_the_child() {
        let directory = tempfile::tempdir().unwrap();
        let sql = b"CREATE TABLE `items` (`id` BIGINT);\nINSERT INTO `items` VALUES (1);\n";
        let artifact = crate::infrastructure::restore_artifact::test_support::validated_artifact(
            directory.path(),
            sql,
        )
        .await;
        let output = directory.path().join("received.sql");
        let spec = ProcessSpec::new("sh").args([
            "-c".to_owned(),
            format!("cat > '{}'", output.display()),
        ]);

        let streamed = stream_import(
            &spec,
            &artifact,
            &CancellationToken::default(),
            "unused-context",
            "unused-container",
        )
        .await
        .unwrap();

        assert!(streamed.status.success());
        assert_eq!(std::fs::read(&output).unwrap(), sql);
        assert_eq!(
            streamed.verified_metrics(&artifact).unwrap().imported_bytes(),
            sql.len() as u64
        );
    }
```

- [ ] **Step 3: Run it and confirm it fails**

Run: `cargo test --lib stream_import_feeds_the_whole_validated_dump_to_the_child`
Expected: compile error `cannot find function 'stream_import'`.

- [ ] **Step 4: Extract `stream_import`**

In `src/infrastructure/mysql/restore_executor.rs`, add after `fn spawn`:

```rust
/// Result of streaming a validated dump into a Dockerized `mysql` client.
///
/// The caller classifies a non-zero exit with its own target wording; the
/// streaming itself does not know whether the target is local or remote.
pub(super) struct StreamedImport {
    pub(super) status: std::process::ExitStatus,
    pub(super) diagnostic: BoundedBytes,
    bytes: u64,
    sha256: Sha256Digest,
}

impl StreamedImport {
    /// Confirms mysql consumed exactly the validated SQL before reporting it.
    pub(super) fn verified_metrics(
        &self,
        artifact: &ValidatedRestoreArtifact,
    ) -> Result<RestoreMetrics, RestoreExecutorError> {
        if self.bytes != artifact.metadata().uncompressed_bytes
            || self.sha256 != artifact.metadata().sql_sha256
        {
            return Err(RestoreExecutorError::ArtifactChangedDuringImport);
        }
        Ok(RestoreMetrics {
            imported_bytes: self.bytes,
        })
    }
}

pub(super) async fn stream_import(
    spec: &ProcessSpec,
    artifact: &ValidatedRestoreArtifact,
    cancellation: &CancellationToken,
    docker_context: &str,
    operation_container: &str,
) -> Result<StreamedImport, RestoreExecutorError> {
    let mut child = spawn(spec, true)?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or(RestoreExecutorError::MissingStdin)?;
    let stderr = child
        .stderr
        .take()
        .ok_or(RestoreExecutorError::MissingStderr)?;
    let stderr_task = tokio::spawn(read_bounded(stderr, MAX_STDERR_BYTES));

    let path = artifact.dump_path().to_owned();
    let (sender, mut receiver) = mpsc::channel::<Vec<u8>>(BUFFERED_CHUNKS);
    let decoder = tokio::task::spawn_blocking(move || decode_chunks(path, sender));
    let copy_result = async {
        loop {
            let chunk = tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    return Err(RestoreExecutorError::Interrupted);
                }
                chunk = receiver.recv() => chunk,
            };
            let Some(chunk) = chunk else {
                break;
            };
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    return Err(RestoreExecutorError::Interrupted);
                }
                result = stdin.write_all(&chunk) => {
                    result.map_err(RestoreExecutorError::WriteStdin)?;
                }
            }
        }
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                Err(RestoreExecutorError::Interrupted)
            }
            result = stdin.shutdown() => result.map_err(RestoreExecutorError::CloseStdin),
        }
    }
    .await;
    drop(receiver);
    drop(stdin);

    let mut interrupted_while_waiting = false;
    let status = if copy_result.is_err() {
        terminate_ephemeral_run(&mut child, docker_context, operation_container).await
    } else {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                interrupted_while_waiting = true;
                terminate_ephemeral_run(&mut child, docker_context, operation_container).await
            }
            status = child.wait() => status,
        }
    }
    .map_err(RestoreExecutorError::Wait);
    let diagnostic = stderr_task
        .await
        .map_err(RestoreExecutorError::StderrTask)??;
    let decoded = decoder.await.map_err(RestoreExecutorError::DecoderTask)?;

    if matches!(&copy_result, Err(RestoreExecutorError::Interrupted)) || interrupted_while_waiting
    {
        if let Err(error) = status {
            tracing::warn!(%error, "could not confirm interrupted restore child termination");
        }
        return Err(RestoreExecutorError::Interrupted);
    }
    copy_result?;
    let decoded = decoded?;
    let status = status?;
    Ok(StreamedImport {
        status,
        diagnostic,
        bytes: decoded.bytes,
        sha256: decoded.sha256,
    })
}
```

Replace the whole body of `DockerMysqlRestoreExecutor::import` with:

```rust
        if self.cancellation.is_cancelled() {
            return Err(RestoreExecutorError::Interrupted);
        }
        let option_file = target_option_file(target)?;
        let operation_container = ephemeral_container_name("restore");
        let spec = import_process_spec(target, artifact, option_file.path(), &operation_container);
        let streamed = stream_import(
            &spec,
            artifact,
            &self.cancellation,
            target.docker_context(),
            &operation_container,
        )
        .await?;
        if !streamed.status.success() {
            return Err(RestoreExecutorError::ImportFailed {
                exit_code: streamed.status.code(),
                kind: classify_failure(&streamed.diagnostic.bytes),
                stderr_truncated: streamed.diagnostic.truncated,
            });
        }
        streamed.verified_metrics(artifact)
```

Change `async fn run_without_stdin(` to `pub(super) async fn run_without_stdin(` and `fn classify_failure(` to `pub(super) fn classify_failure(`. Add `use crate::infrastructure::process::BoundedBytes` to the existing `process::{...}` import if not already imported (it is: `process::{BoundedBytes, ProcessSpec, read_bounded}`).

- [ ] **Step 5: Run the new and the existing restore tests**

Run: `cargo test --lib restore_executor && cargo test --lib restore_engine && cargo test --lib restore_service`
Expected: all PASS (behavior of the local restore is unchanged).

- [ ] **Step 6: Gate and commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
git add src/infrastructure/mysql/restore_executor.rs src/infrastructure/restore_artifact.rs
git commit -m "refactor: extract the dump import stream from the local restore executor"
```

---

### Task 2: Remote target gate

**Files:**
- Create: `src/application/remote_target_gate.rs`
- Modify: `src/application/mod.rs` (register module + re-exports)
- Modify: `src/application/restore_engine.rs:83` (`fn versions_are_restore_compatible` → `pub(crate) fn`)

**Interfaces:**
- Consumes: `ConfigRepository::load() -> Result<AppConfig, ConfigError>`; `AppConfig.profiles: BTreeMap<ProfileName, SourceProfileConfig>`; `AppConfig.client_runtime.docker_context: Option<String>`; `ClientCatalog::validate(&str, &str) -> Result<ApprovedMysqlClient, ClientCatalogError>`; `CredentialStore::get(&CredentialKey) -> Result<SecretString, CredentialError>`; `MysqlServerInfo { version, vendor, server_uuid, tls_cipher }`; `crate::application::restore_engine::versions_are_restore_compatible(MysqlVersion, MysqlVersion, MysqlVersion, MysqlVersion) -> bool`.
- Produces:
  - `pub struct RemoteProfileChoice { pub profile: ProfileName, pub host: String, pub port: u16 }` (Clone, Debug, Eq, PartialEq)
  - `pub struct RemoteTargetProbeRequest<'a> { pub docker_context: &'a str, pub profile: &'a SourceProfileConfig, pub password: &'a SecretString, pub client: ApprovedMysqlClient }`
  - `#[async_trait] pub trait RemoteTargetProbe: Send + Sync { async fn probe(&self, request: RemoteTargetProbeRequest<'_>) -> Result<MysqlServerInfo, DockerClientError>; }`
  - `pub struct RemoteTargetGate` with `new(ConfigRepository)`, `eligible_profiles(&self) -> Result<Vec<RemoteProfileChoice>, RemoteTargetGateError>`, `async verify(&self, &dyn CredentialStore, &dyn RemoteTargetProbe, &ProfileName) -> Result<GuardedRemoteTarget, RemoteTargetGateError>`
  - `pub struct GuardedRemoteTarget` with `authorize(self, DatabaseName, &DumpArtifactMetadata) -> Result<AuthorizedRemoteTarget, RemoteTargetGateError>`, `server_version()`
  - `pub struct AuthorizedRemoteTarget` accessors: `profile() -> &ProfileName`, `host() -> &str`, `port() -> u16`, `username() -> &str`, `password() -> &SecretString`, `tls_mode() -> MysqlTlsMode`, `tls_material() -> &MysqlTlsMaterialPaths`, `docker_context() -> &str`, `client() -> ApprovedMysqlClient`, `server_version() -> MysqlVersion`, `database() -> &DatabaseName`; `#[cfg(test)] pub(crate) fn for_test(database: DatabaseName) -> Self`
  - `pub enum RemoteTargetGateError { Config, ProfileNotFound, ProductionDestination, DockerContextMissing, ClientCatalog, Credential, Probe, TlsRequiredButNotNegotiated, UnsupportedVendor, UnsupportedServerSeries, SourceCollision, VersionMismatch }`

- [ ] **Step 1: Write the module with its failing tests**

Create `src/application/remote_target_gate.rs`:

```rust
use async_trait::async_trait;
use secrecy::SecretString;
use thiserror::Error;

use crate::{
    application::restore_engine::versions_are_restore_compatible,
    domain::{
        DatabaseName, DumpArtifactMetadata, MysqlServerUuid, MysqlTlsMaterialPaths, MysqlTlsMode,
        MysqlVersion, ProfileName,
    },
    infrastructure::{
        config::{ConfigError, ConfigRepository, SourceProfileConfig},
        credentials::{CredentialError, CredentialStore},
        mysql::{
            ApprovedMysqlClient, ClientCatalog, ClientCatalogError, DockerClientError,
            MysqlServerInfo,
        },
    },
};

/// A profile `push` may write to. Production profiles are never offered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteProfileChoice {
    pub profile: ProfileName,
    pub host: String,
    pub port: u16,
}

pub struct RemoteTargetProbeRequest<'a> {
    pub docker_context: &'a str,
    pub profile: &'a SourceProfileConfig,
    pub password: &'a SecretString,
    pub client: ApprovedMysqlClient,
}

#[async_trait]
pub trait RemoteTargetProbe: Send + Sync {
    async fn probe(
        &self,
        request: RemoteTargetProbeRequest<'_>,
    ) -> Result<MysqlServerInfo, DockerClientError>;
}

/// Turns a configured profile into a destination `push` may write to.
///
/// This is the only path that authorizes a write outside the local Docker
/// target, so every refusal happens here, before any SQL is sent.
pub struct RemoteTargetGate {
    repository: ConfigRepository,
}

impl RemoteTargetGate {
    pub fn new(repository: ConfigRepository) -> Self {
        Self { repository }
    }

    pub fn eligible_profiles(&self) -> Result<Vec<RemoteProfileChoice>, RemoteTargetGateError> {
        let config = self.repository.load()?;
        Ok(config
            .profiles
            .iter()
            .filter(|(_, profile)| !profile.production)
            .map(|(name, profile)| RemoteProfileChoice {
                profile: name.clone(),
                host: profile.host.clone(),
                port: profile.port,
            })
            .collect())
    }

    pub async fn verify(
        &self,
        credentials: &dyn CredentialStore,
        probe: &dyn RemoteTargetProbe,
        profile_name: &ProfileName,
    ) -> Result<GuardedRemoteTarget, RemoteTargetGateError> {
        let config = self.repository.load()?;
        let profile = config
            .profiles
            .get(profile_name)
            .ok_or(RemoteTargetGateError::ProfileNotFound)?;
        if profile.production {
            return Err(RemoteTargetGateError::ProductionDestination);
        }
        let docker_context = config
            .client_runtime
            .docker_context
            .as_deref()
            .ok_or(RemoteTargetGateError::DockerContextMissing)?;
        let client = ClientCatalog::validate(&profile.mysql_series, &profile.client.image)?;
        let password = credentials.get(&profile.credential_key).await?;
        let server = probe
            .probe(RemoteTargetProbeRequest {
                docker_context,
                profile,
                password: &password,
                client,
            })
            .await?;
        if profile.tls_mode.requires_encrypted_transport() && server.tls_cipher.is_none() {
            return Err(RemoteTargetGateError::TlsRequiredButNotNegotiated);
        }
        if !server.vendor.to_ascii_lowercase().contains("mysql") {
            return Err(RemoteTargetGateError::UnsupportedVendor);
        }
        let detected_series = format!("{}.{}", server.version.major, server.version.minor);
        if detected_series != client.series() {
            return Err(RemoteTargetGateError::UnsupportedServerSeries);
        }

        Ok(GuardedRemoteTarget {
            profile: profile_name.clone(),
            host: profile.host.clone(),
            port: profile.port,
            username: profile.username.clone(),
            password,
            tls_mode: profile.tls_mode,
            tls_material: profile.tls_material.clone(),
            docker_context: docker_context.to_owned(),
            client,
            server_version: server.version,
            server_uuid: server.server_uuid,
        })
    }
}

pub struct GuardedRemoteTarget {
    profile: ProfileName,
    host: String,
    port: u16,
    username: String,
    password: SecretString,
    tls_mode: MysqlTlsMode,
    tls_material: MysqlTlsMaterialPaths,
    docker_context: String,
    client: ApprovedMysqlClient,
    server_version: MysqlVersion,
    server_uuid: MysqlServerUuid,
}

impl std::fmt::Debug for GuardedRemoteTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardedRemoteTarget")
            .field("profile", &self.profile)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("server_version", &self.server_version)
            .field("server_uuid", &self.server_uuid)
            .finish_non_exhaustive()
    }
}

impl GuardedRemoteTarget {
    pub const fn server_version(&self) -> MysqlVersion {
        self.server_version
    }

    /// Binds the destination database once the dump is known.
    ///
    /// Overwriting the exact database the dump came from is refused; another
    /// database on the same server is a legitimate sandbox copy.
    pub fn authorize(
        self,
        database: DatabaseName,
        metadata: &DumpArtifactMetadata,
    ) -> Result<AuthorizedRemoteTarget, RemoteTargetGateError> {
        if self.server_uuid == metadata.source_server_uuid && database == metadata.database {
            return Err(RemoteTargetGateError::SourceCollision);
        }
        if !versions_are_restore_compatible(
            metadata.source_version,
            metadata.client_version,
            self.server_version,
            self.client.version(),
        ) {
            return Err(RemoteTargetGateError::VersionMismatch);
        }
        Ok(AuthorizedRemoteTarget {
            target: self,
            database,
        })
    }
}

#[derive(Debug)]
pub struct AuthorizedRemoteTarget {
    target: GuardedRemoteTarget,
    database: DatabaseName,
}

impl AuthorizedRemoteTarget {
    pub fn profile(&self) -> &ProfileName {
        &self.target.profile
    }

    pub fn host(&self) -> &str {
        &self.target.host
    }

    pub const fn port(&self) -> u16 {
        self.target.port
    }

    pub fn username(&self) -> &str {
        &self.target.username
    }

    pub fn password(&self) -> &SecretString {
        &self.target.password
    }

    pub const fn tls_mode(&self) -> MysqlTlsMode {
        self.target.tls_mode
    }

    pub fn tls_material(&self) -> &MysqlTlsMaterialPaths {
        &self.target.tls_material
    }

    pub fn docker_context(&self) -> &str {
        &self.target.docker_context
    }

    pub const fn client(&self) -> ApprovedMysqlClient {
        self.target.client
    }

    pub const fn server_version(&self) -> MysqlVersion {
        self.target.server_version
    }

    pub fn database(&self) -> &DatabaseName {
        &self.database
    }

    #[cfg(test)]
    pub(crate) fn for_test(database: DatabaseName) -> Self {
        Self {
            target: GuardedRemoteTarget {
                profile: ProfileName::try_from("sandbox").unwrap(),
                host: "sandbox.db.internal".to_owned(),
                port: 3306,
                username: "sandbox_writer".to_owned(),
                password: SecretString::from("remote-test-password"),
                tls_mode: MysqlTlsMode::Required,
                tls_material: MysqlTlsMaterialPaths::default(),
                docker_context: "desktop-linux".to_owned(),
                client: ClientCatalog::resolve("8.4").unwrap(),
                server_version: "8.4.4".parse().unwrap(),
                server_uuid: "33333333-3333-4333-8333-333333333333".parse().unwrap(),
            },
            database,
        }
    }
}

#[derive(Debug, Error)]
pub enum RemoteTargetGateError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error("the destination profile does not exist; see `reprodb profile list`")]
    ProfileNotFound,

    #[error("production profiles are never push destinations")]
    ProductionDestination,

    #[error("the MySQL client Docker context is missing; run `reprodb doctor`")]
    DockerContextMissing,

    #[error(transparent)]
    ClientCatalog(#[from] ClientCatalogError),

    #[error(transparent)]
    Credential(#[from] CredentialError),

    #[error(transparent)]
    Probe(#[from] DockerClientError),

    #[error("the destination did not negotiate the TLS transport required by its profile")]
    TlsRequiredButNotNegotiated,

    #[error("the destination server is not MySQL")]
    UnsupportedVendor,

    #[error("the destination server series differs from its profile; re-add the profile")]
    UnsupportedServerSeries,

    #[error(
        "the destination is the server and database this dump came from; choose another database name"
    )]
    SourceCollision,

    #[error("the destination MySQL version cannot import this dump (downgrades are refused)")]
    VersionMismatch,
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    };

    use tempfile::tempdir;

    use crate::{
        domain::{
            CredentialKey, CredentialScope, DatabaseEncoding, DumpArtifactCompletion,
            DumpArtifactContext, DumpId, Sha256Digest,
        },
        infrastructure::{
            config::{
                AppConfig, AppPaths, ClientRuntimeConfig, MysqlClientConfig, MysqlFamily,
            },
            credentials::MemoryCredentialStore,
        },
    };

    use super::*;

    const SOURCE_UUID: &str = "11111111-1111-4111-8111-111111111111";
    const OTHER_UUID: &str = "33333333-3333-4333-8333-333333333333";

    struct FakeProbe {
        server: MysqlServerInfo,
        calls: Arc<Mutex<usize>>,
    }

    impl FakeProbe {
        fn returning(version: &str, server_uuid: &str, tls: bool) -> Self {
            Self {
                server: MysqlServerInfo {
                    version: version.parse().unwrap(),
                    vendor: "MySQL Community Server - GPL".to_owned(),
                    server_uuid: server_uuid.parse().unwrap(),
                    tls_cipher: tls.then(|| "TLS_AES_256_GCM_SHA384".to_owned()),
                },
                calls: Arc::new(Mutex::new(0)),
            }
        }
    }

    #[async_trait]
    impl RemoteTargetProbe for FakeProbe {
        async fn probe(
            &self,
            _request: RemoteTargetProbeRequest<'_>,
        ) -> Result<MysqlServerInfo, DockerClientError> {
            *self.calls.lock().unwrap() += 1;
            Ok(self.server.clone())
        }
    }

    fn profile(production: bool, credential_key: CredentialKey) -> SourceProfileConfig {
        SourceProfileConfig {
            host: "sandbox.db.internal".to_owned(),
            port: 3306,
            username: "sandbox_writer".to_owned(),
            credential_key,
            mysql_family: MysqlFamily::Mysql,
            mysql_series: "8.4".to_owned(),
            production,
            tls_mode: if production {
                MysqlTlsMode::VerifyIdentity
            } else {
                MysqlTlsMode::Required
            },
            tls_material: if production {
                MysqlTlsMaterialPaths {
                    ca: Some("/etc/reprodb/ca.pem".into()),
                    cert: None,
                    key: None,
                }
            } else {
                MysqlTlsMaterialPaths::default()
            },
            client: MysqlClientConfig {
                image: ClientCatalog::resolve("8.4").unwrap().image().to_owned(),
            },
        }
    }

    async fn fixture(root: &std::path::Path) -> (ConfigRepository, MemoryCredentialStore) {
        let repository = ConfigRepository::new(AppPaths::new(
            root.join("config"),
            root.join("cache"),
            root.join("data"),
        ));
        let sandbox_key = CredentialKey::new(CredentialScope::Source);
        let production_key = CredentialKey::new(CredentialScope::Source);
        repository
            .save(&AppConfig {
                client_runtime: ClientRuntimeConfig {
                    docker_context: Some("desktop-linux".to_owned()),
                    ..ClientRuntimeConfig::default()
                },
                profiles: BTreeMap::from([
                    (
                        ProfileName::try_from("sandbox").unwrap(),
                        profile(false, sandbox_key),
                    ),
                    (
                        ProfileName::try_from("prod-source").unwrap(),
                        profile(true, production_key),
                    ),
                ]),
                ..AppConfig::default()
            })
            .unwrap();
        let credentials = MemoryCredentialStore::default();
        credentials
            .set(&sandbox_key, SecretString::from("sandbox-password"))
            .await
            .unwrap();
        credentials
            .set(&production_key, SecretString::from("production-password"))
            .await
            .unwrap();
        (repository, credentials)
    }

    fn metadata(source_version: &str) -> DumpArtifactMetadata {
        DumpArtifactMetadata::try_new(
            DumpId::new(),
            DumpArtifactContext {
                database: DatabaseName::try_from("salt_sagatec").unwrap(),
                profile: ProfileName::try_from("prod-source").unwrap(),
                source_fingerprint: Sha256Digest::from_bytes([1; 32]),
                source_server_uuid: SOURCE_UUID.parse().unwrap(),
                source_version: source_version.parse().unwrap(),
                client_version: source_version.parse().unwrap(),
                database_encoding: DatabaseEncoding::try_new(
                    "utf8mb4".to_owned(),
                    "utf8mb4_0900_ai_ci".to_owned(),
                )
                .unwrap(),
                policy_version: 1,
            },
            DumpArtifactCompletion {
                created_at_unix_seconds: 100,
                completed_at_unix_seconds: 101,
                uncompressed_bytes: 10,
                compressed_bytes: 10,
                sql_sha256: Sha256Digest::from_bytes([2; 32]),
                artifact_sha256: Sha256Digest::from_bytes([3; 32]),
            },
        )
        .unwrap()
    }

    fn name(value: &str) -> ProfileName {
        ProfileName::try_from(value).unwrap()
    }

    fn database(value: &str) -> DatabaseName {
        DatabaseName::try_from(value).unwrap()
    }

    #[tokio::test]
    async fn only_non_production_profiles_are_eligible() {
        let directory = tempdir().unwrap();
        let (repository, _credentials) = fixture(directory.path()).await;

        let choices = RemoteTargetGate::new(repository).eligible_profiles().unwrap();

        assert_eq!(
            choices,
            vec![RemoteProfileChoice {
                profile: name("sandbox"),
                host: "sandbox.db.internal".to_owned(),
                port: 3306,
            }]
        );
    }

    #[tokio::test]
    async fn a_production_profile_is_refused_before_any_connection() {
        let directory = tempdir().unwrap();
        let (repository, credentials) = fixture(directory.path()).await;
        let probe = FakeProbe::returning("8.4.4", OTHER_UUID, true);

        let error = RemoteTargetGate::new(repository)
            .verify(&credentials, &probe, &name("prod-source"))
            .await
            .unwrap_err();

        assert!(matches!(error, RemoteTargetGateError::ProductionDestination));
        assert_eq!(*probe.calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn an_unknown_profile_is_refused() {
        let directory = tempdir().unwrap();
        let (repository, credentials) = fixture(directory.path()).await;

        let error = RemoteTargetGate::new(repository)
            .verify(
                &credentials,
                &FakeProbe::returning("8.4.4", OTHER_UUID, true),
                &name("missing"),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, RemoteTargetGateError::ProfileNotFound));
    }

    #[tokio::test]
    async fn a_required_tls_profile_without_a_cipher_is_refused() {
        let directory = tempdir().unwrap();
        let (repository, credentials) = fixture(directory.path()).await;

        let error = RemoteTargetGate::new(repository)
            .verify(
                &credentials,
                &FakeProbe::returning("8.4.4", OTHER_UUID, false),
                &name("sandbox"),
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            RemoteTargetGateError::TlsRequiredButNotNegotiated
        ));
    }

    #[tokio::test]
    async fn a_server_outside_the_profile_series_is_refused() {
        let directory = tempdir().unwrap();
        let (repository, credentials) = fixture(directory.path()).await;

        let error = RemoteTargetGate::new(repository)
            .verify(
                &credentials,
                &FakeProbe::returning("8.0.45", OTHER_UUID, true),
                &name("sandbox"),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, RemoteTargetGateError::UnsupportedServerSeries));
    }

    #[tokio::test]
    async fn the_source_database_itself_is_refused_but_another_name_on_that_server_is_not() {
        let directory = tempdir().unwrap();
        let (repository, credentials) = fixture(directory.path()).await;
        let gate = RemoteTargetGate::new(repository);
        let probe = FakeProbe::returning("8.4.4", SOURCE_UUID, true);

        let same = gate
            .verify(&credentials, &probe, &name("sandbox"))
            .await
            .unwrap()
            .authorize(database("salt_sagatec"), &metadata("8.4.4"))
            .unwrap_err();
        let other = gate
            .verify(&credentials, &probe, &name("sandbox"))
            .await
            .unwrap()
            .authorize(database("salt_sagatec_qa"), &metadata("8.4.4"))
            .unwrap();

        assert!(matches!(same, RemoteTargetGateError::SourceCollision));
        assert_eq!(other.database().as_str(), "salt_sagatec_qa");
        assert_eq!(other.host(), "sandbox.db.internal");
    }

    #[tokio::test]
    async fn the_same_database_name_on_another_server_is_allowed() {
        let directory = tempdir().unwrap();
        let (repository, credentials) = fixture(directory.path()).await;

        let target = RemoteTargetGate::new(repository)
            .verify(
                &credentials,
                &FakeProbe::returning("8.4.4", OTHER_UUID, true),
                &name("sandbox"),
            )
            .await
            .unwrap()
            .authorize(database("salt_sagatec"), &metadata("8.4.4"))
            .unwrap();

        use secrecy::ExposeSecret as _;
        assert_eq!(target.profile().as_str(), "sandbox");
        assert_eq!(target.password().expose_secret(), "sandbox-password");
    }

    #[tokio::test]
    async fn a_downgrade_is_refused() {
        let directory = tempdir().unwrap();
        let (repository, credentials) = fixture(directory.path()).await;
        // Destination is 8.4; a dump taken from a hypothetical 9.x server is a downgrade.
        let guarded = RemoteTargetGate::new(repository)
            .verify(
                &credentials,
                &FakeProbe::returning("8.4.4", OTHER_UUID, true),
                &name("sandbox"),
            )
            .await
            .unwrap();

        let error = guarded
            .authorize(database("salt_sagatec"), &metadata("9.1.0"))
            .unwrap_err();

        assert!(matches!(error, RemoteTargetGateError::VersionMismatch));
    }
}
```

(If `MysqlVersion` cannot parse `"9.1.0"` or `DumpArtifactMetadata::try_new` rejects it, use `"8.4.4"` for the dump and make the probe return `"8.0.45"` with a profile whose `mysql_series` is `"8.0"` instead — the assertion stays `VersionMismatch`. Check `ClientCatalog::resolve("8.0")` exists before choosing; the catalog is in `src/infrastructure/mysql/client_catalog.rs`.)

- [ ] **Step 2: Register the module and widen the version helper**

In `src/application/mod.rs` add `mod remote_target_gate;` (alphabetical, after `pull_service`) and:

```rust
pub use remote_target_gate::{
    AuthorizedRemoteTarget, GuardedRemoteTarget, RemoteProfileChoice, RemoteTargetGate,
    RemoteTargetGateError, RemoteTargetProbe, RemoteTargetProbeRequest,
};
```

In `src/application/restore_engine.rs` change `fn versions_are_restore_compatible(` to `pub(crate) fn versions_are_restore_compatible(`.

- [ ] **Step 3: Run the gate tests**

Run: `cargo test --lib remote_target_gate`
Expected: 7 PASS.

- [ ] **Step 4: Gate and commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
git add src/application/remote_target_gate.rs src/application/mod.rs src/application/restore_engine.rs
git commit -m "feat: gate remote push destinations by profile, identity and version"
```

---

### Task 3: Docker probe for the destination server

**Files:**
- Create: `src/infrastructure/mysql/remote_target_probe.rs`
- Modify: `src/infrastructure/mysql/mod.rs`

**Interfaces:**
- Consumes: `RemoteTargetProbe`, `RemoteTargetProbeRequest` (Task 2); `DockerMysqlClientRuntime::{prepare_existing, create_option_file_with_tls_material, probe_connection}`.
- Produces: `pub struct DockerRemoteTargetProbe<R>` with `pub fn new(runner: R) -> Self`, implementing `RemoteTargetProbe` for `R: ProcessRunner`.

- [ ] **Step 1: Write the probe with a failing test**

Create `src/infrastructure/mysql/remote_target_probe.rs`:

```rust
use async_trait::async_trait;

use crate::{
    application::{RemoteTargetProbe, RemoteTargetProbeRequest},
    infrastructure::{
        mysql::{DockerClientError, DockerMysqlClientRuntime, MysqlServerInfo},
        process::ProcessRunner,
    },
};

/// Reads the destination identity through the profile's own connection
/// settings, exactly as a dump would reach that server.
pub struct DockerRemoteTargetProbe<R> {
    runtime: DockerMysqlClientRuntime<R>,
}

impl<R> DockerRemoteTargetProbe<R>
where
    R: ProcessRunner,
{
    pub fn new(runner: R) -> Self {
        Self {
            runtime: DockerMysqlClientRuntime::new(runner),
        }
    }
}

#[async_trait]
impl<R> RemoteTargetProbe for DockerRemoteTargetProbe<R>
where
    R: ProcessRunner,
{
    async fn probe(
        &self,
        request: RemoteTargetProbeRequest<'_>,
    ) -> Result<MysqlServerInfo, DockerClientError> {
        let prepared = self
            .runtime
            .prepare_existing(
                request.docker_context,
                request.client.series(),
                request.client.image(),
            )
            .await?;
        let option_file = self.runtime.create_option_file_with_tls_material(
            &request.profile.host,
            request.profile.port,
            &request.profile.username,
            request.password,
            request.profile.tls_mode,
            &request.profile.tls_material,
        )?;
        self.runtime.probe_connection(&prepared, &option_file).await
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use secrecy::SecretString;

    use crate::{
        domain::{CredentialKey, CredentialScope, MysqlTlsMode},
        infrastructure::{
            config::{MysqlClientConfig, MysqlFamily, SourceProfileConfig},
            mysql::ClientCatalog,
            process::{ProcessError, ProcessOutput, ProcessSpec},
        },
    };

    use super::*;

    struct FakeRunner {
        outputs: Mutex<VecDeque<ProcessOutput>>,
        commands: Arc<Mutex<Vec<ProcessSpec>>>,
    }

    #[async_trait]
    impl ProcessRunner for FakeRunner {
        async fn output(&self, spec: &ProcessSpec) -> Result<ProcessOutput, ProcessError> {
            self.commands.lock().unwrap().push(spec.clone());
            Ok(self.outputs.lock().unwrap().pop_front().unwrap())
        }
    }

    #[tokio::test]
    async fn probes_the_destination_through_the_profile_host_without_leaking_the_password() {
        let client = ClientCatalog::resolve("8.4").unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let probe = DockerRemoteTargetProbe::new(FakeRunner {
            outputs: Mutex::new(VecDeque::from([
                ProcessOutput::success(
                    serde_json::to_vec(&vec![client.repository_digest()]).unwrap(),
                ),
                ProcessOutput::success("mysql  Ver 8.4.4 for Linux on aarch64\n"),
                ProcessOutput::success(
                    "8.4.4\tMySQL Community Server - GPL\t33333333-3333-4333-8333-333333333333\nSsl_cipher\tTLS_AES_256_GCM_SHA384\n",
                ),
            ])),
            commands: Arc::clone(&commands),
        });
        let profile = SourceProfileConfig {
            host: "sandbox.db.internal".to_owned(),
            port: 3307,
            username: "sandbox_writer".to_owned(),
            credential_key: CredentialKey::new(CredentialScope::Source),
            mysql_family: MysqlFamily::Mysql,
            mysql_series: "8.4".to_owned(),
            production: false,
            tls_mode: MysqlTlsMode::Required,
            tls_material: Default::default(),
            client: MysqlClientConfig {
                image: client.image().to_owned(),
            },
        };

        let server = probe
            .probe(RemoteTargetProbeRequest {
                docker_context: "desktop-linux",
                profile: &profile,
                password: &SecretString::from("password-marker"),
                client,
            })
            .await
            .unwrap();

        assert_eq!(server.version.to_string(), "8.4.4");
        assert_eq!(
            server.server_uuid.to_string(),
            "33333333-3333-4333-8333-333333333333"
        );
        let rendered = format!("{:?}", commands.lock().unwrap());
        assert!(!rendered.contains("password-marker"));
        assert!(!rendered.contains("--network=container:"));
    }
}
```

If `MysqlServerUuid` has no `Display`, compare with `"33333333-3333-4333-8333-333333333333".parse().unwrap()` instead.

- [ ] **Step 2: Register and run**

In `src/infrastructure/mysql/mod.rs` add `mod remote_target_probe;` and `pub use remote_target_probe::DockerRemoteTargetProbe;`.

Run: `cargo test --lib remote_target_probe`
Expected: PASS. (Fails to compile until registered — that is the red step.)

- [ ] **Step 3: Gate and commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
git add src/infrastructure/mysql/remote_target_probe.rs src/infrastructure/mysql/mod.rs
git commit -m "feat: probe push destinations with the profile connection"
```

---

### Task 4: Remote import executor

**Files:**
- Create: `src/infrastructure/mysql/remote_import_executor.rs`
- Modify: `src/infrastructure/mysql/mod.rs`

**Interfaces:**
- Consumes: `AuthorizedRemoteTarget` (Task 2); `stream_import`, `StreamedImport::verified_metrics`, `run_without_stdin`, `classify_failure` (Task 1); `RestoreFailureKind`, `RestoreMetrics`, `RestoreExecutorError`.
- Produces:
  - `#[async_trait] pub trait RemoteImportExecutor: Send + Sync { async fn ensure_database(&self, target: &AuthorizedRemoteTarget, metadata: &DumpArtifactMetadata) -> Result<(), RemoteImportError>; async fn import(&self, target: &AuthorizedRemoteTarget, artifact: &ValidatedRestoreArtifact) -> Result<RestoreMetrics, RemoteImportError>; }`
  - `pub struct DockerMysqlRemoteImportExecutor` with `pub fn new(CancellationToken) -> Self`, `Default`
  - `pub enum RemoteFailureKind { Authentication, Permission, DestinationUnavailable, Sql, DockerUnavailable, Unknown }`
  - `pub enum RemoteImportError { Client(DockerClientError), Stream(RestoreExecutorError), EnsureFailed { exit_code, kind, stderr_truncated }, ImportFailed { exit_code, kind, stderr_truncated } }`

- [ ] **Step 1: Write the executor with failing tests**

Create `src/infrastructure/mysql/remote_import_executor.rs`:

```rust
use std::{ffi::OsString, path::Path};

use async_trait::async_trait;
use thiserror::Error;

use crate::{
    application::AuthorizedRemoteTarget,
    domain::DumpArtifactMetadata,
    infrastructure::{
        cancellation::CancellationToken,
        credentials::{MYSQL_OPTION_FILE_CONTAINER_PATH, MYSQL_SECRETS_CONTAINER_DIRECTORY},
        docker::ephemeral_container_name,
        mysql::{
            DockerClientError, DockerMysqlClientRuntime, RestoreExecutorError,
            RestoreFailureKind, RestoreMetrics,
            restore_executor::{classify_failure, run_without_stdin, stream_import},
        },
        process::{ProcessSpec, TokioProcessRunner},
        restore_artifact::ValidatedRestoreArtifact,
    },
};

#[async_trait]
pub trait RemoteImportExecutor: Send + Sync {
    async fn ensure_database(
        &self,
        target: &AuthorizedRemoteTarget,
        metadata: &DumpArtifactMetadata,
    ) -> Result<(), RemoteImportError>;

    async fn import(
        &self,
        target: &AuthorizedRemoteTarget,
        artifact: &ValidatedRestoreArtifact,
    ) -> Result<RestoreMetrics, RemoteImportError>;
}

#[derive(Clone, Debug, Default)]
pub struct DockerMysqlRemoteImportExecutor {
    cancellation: CancellationToken,
}

impl DockerMysqlRemoteImportExecutor {
    pub fn new(cancellation: CancellationToken) -> Self {
        Self { cancellation }
    }
}

#[async_trait]
impl RemoteImportExecutor for DockerMysqlRemoteImportExecutor {
    async fn ensure_database(
        &self,
        target: &AuthorizedRemoteTarget,
        metadata: &DumpArtifactMetadata,
    ) -> Result<(), RemoteImportError> {
        if self.cancellation.is_cancelled() {
            return Err(RestoreExecutorError::Interrupted.into());
        }
        let option_file = remote_option_file(target)?;
        let operation_container = ephemeral_container_name("push");
        let spec = ensure_process_spec(target, metadata, option_file.path(), &operation_container);
        let (status, diagnostic) = run_without_stdin(
            &spec,
            &self.cancellation,
            target.docker_context(),
            &operation_container,
        )
        .await?;
        if !status.success() {
            return Err(RemoteImportError::EnsureFailed {
                exit_code: status.code(),
                kind: classify_remote_failure(&diagnostic.bytes),
                stderr_truncated: diagnostic.truncated,
            });
        }
        Ok(())
    }

    async fn import(
        &self,
        target: &AuthorizedRemoteTarget,
        artifact: &ValidatedRestoreArtifact,
    ) -> Result<RestoreMetrics, RemoteImportError> {
        if self.cancellation.is_cancelled() {
            return Err(RestoreExecutorError::Interrupted.into());
        }
        let option_file = remote_option_file(target)?;
        let operation_container = ephemeral_container_name("push");
        let spec = import_process_spec(target, artifact, option_file.path(), &operation_container);
        let streamed = stream_import(
            &spec,
            artifact,
            &self.cancellation,
            target.docker_context(),
            &operation_container,
        )
        .await?;
        if !streamed.status.success() {
            return Err(RemoteImportError::ImportFailed {
                exit_code: streamed.status.code(),
                kind: classify_remote_failure(&streamed.diagnostic.bytes),
                stderr_truncated: streamed.diagnostic.truncated,
            });
        }
        Ok(streamed.verified_metrics(artifact)?)
    }
}

fn remote_option_file(
    target: &AuthorizedRemoteTarget,
) -> Result<crate::infrastructure::credentials::MysqlOptionFile, DockerClientError> {
    DockerMysqlClientRuntime::new(TokioProcessRunner).create_option_file_with_tls_material(
        target.host(),
        target.port(),
        target.username(),
        target.password(),
        target.tls_mode(),
        target.tls_material(),
    )
}

fn ensure_process_spec(
    target: &AuthorizedRemoteTarget,
    metadata: &DumpArtifactMetadata,
    option_file: &Path,
    operation_container: &str,
) -> ProcessSpec {
    let database = target.database().as_str();
    let sql = format!(
        "CREATE DATABASE IF NOT EXISTS `{database}` CHARACTER SET {} COLLATE {};",
        metadata.database_charset, metadata.database_collation
    );
    remote_mysql_process_spec(target, option_file, false, operation_container)
        .args(["--execute", &sql])
}

fn import_process_spec(
    target: &AuthorizedRemoteTarget,
    artifact: &ValidatedRestoreArtifact,
    option_file: &Path,
    operation_container: &str,
) -> ProcessSpec {
    remote_mysql_process_spec(target, option_file, true, operation_container).args([
        OsString::from("--binary-mode"),
        OsString::from(format!("--database={}", target.database())),
        OsString::from(format!(
            "--default-character-set={}",
            artifact.metadata().database_charset
        )),
    ])
}

/// Same client container as the dump uses: the profile's host is reached
/// through `host-gateway`, never through another container's network.
fn remote_mysql_process_spec(
    target: &AuthorizedRemoteTarget,
    option_file: &Path,
    interactive: bool,
    operation_container: &str,
) -> ProcessSpec {
    let mut mount = OsString::from("type=bind,src=");
    mount.push(option_file.parent().unwrap_or(option_file).as_os_str());
    mount.push(format!(",dst={MYSQL_SECRETS_CONTAINER_DIRECTORY},readonly"));

    let mut spec = ProcessSpec::new("docker").args([
        OsString::from("--context"),
        OsString::from(target.docker_context()),
        OsString::from("run"),
        OsString::from("--rm"),
        OsString::from("--name"),
        OsString::from(operation_container),
    ]);
    if interactive {
        spec = spec.arg("-i");
    }
    spec = spec.args([
        OsString::from("--pull=never"),
        OsString::from("--add-host=host.docker.internal:host-gateway"),
        OsString::from("--mount"),
        mount,
        OsString::from(target.client().image()),
        OsString::from("mysql"),
        OsString::from(format!(
            "--defaults-file={MYSQL_OPTION_FILE_CONTAINER_PATH}"
        )),
    ]);
    if target.client().supports_no_login_paths() {
        spec = spec.arg("--no-login-paths");
    }
    spec
}

/// MySQL reports a missing database grant (error 1044) as "Access denied …
/// to database"; that is a privilege problem, not a wrong password.
fn classify_remote_failure(stderr: &[u8]) -> RemoteFailureKind {
    let lowered = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if lowered.contains("access denied") && lowered.contains("to database") {
        return RemoteFailureKind::Permission;
    }
    classify_failure(stderr).into()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteFailureKind {
    Authentication,
    Permission,
    DestinationUnavailable,
    Sql,
    DockerUnavailable,
    Unknown,
}

impl From<RestoreFailureKind> for RemoteFailureKind {
    fn from(kind: RestoreFailureKind) -> Self {
        match kind {
            RestoreFailureKind::Authentication => Self::Authentication,
            RestoreFailureKind::Permission => Self::Permission,
            RestoreFailureKind::TargetUnavailable => Self::DestinationUnavailable,
            RestoreFailureKind::Sql => Self::Sql,
            RestoreFailureKind::DockerUnavailable => Self::DockerUnavailable,
            RestoreFailureKind::Unknown => Self::Unknown,
        }
    }
}

impl std::fmt::Display for RemoteFailureKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Authentication => {
                "the destination rejected the profile credential; remove and re-add the profile"
            }
            Self::Permission => {
                "the destination user lacks privileges to create or write the selected database"
            }
            Self::DestinationUnavailable => {
                "the destination MySQL server became unreachable; check the network and run `reprodb doctor`"
            }
            Self::Sql => "MySQL rejected a statement from the validated dump",
            Self::DockerUnavailable => {
                "Docker became unavailable; start Docker and run `reprodb doctor`"
            }
            Self::Unknown => "the MySQL client returned an unclassified push failure",
        })
    }
}

#[derive(Debug, Error)]
pub enum RemoteImportError {
    #[error(transparent)]
    Client(#[from] DockerClientError),

    #[error(
        "{0}; the remote database may be partially imported and rerunning the same push imports over it"
    )]
    Stream(#[from] RestoreExecutorError),

    #[error(
        "remote database creation failed: {kind} (exit code {exit_code:?}, diagnostics truncated: {stderr_truncated})"
    )]
    EnsureFailed {
        exit_code: Option<i32>,
        kind: RemoteFailureKind,
        stderr_truncated: bool,
    },

    #[error(
        "remote import failed: {kind} (exit code {exit_code:?}, diagnostics truncated: {stderr_truncated}); the remote database may be partially imported and rerunning the same push imports over it"
    )]
    ImportFailed {
        exit_code: Option<i32>,
        kind: RemoteFailureKind,
        stderr_truncated: bool,
    },
}

#[cfg(test)]
mod tests {
    use crate::domain::DatabaseName;

    use super::*;

    fn arguments(spec: &ProcessSpec) -> Vec<String> {
        spec.arguments()
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    fn target() -> AuthorizedRemoteTarget {
        AuthorizedRemoteTarget::for_test(DatabaseName::try_from("salt_sagatec_qa").unwrap())
    }

    #[test]
    fn the_remote_client_reaches_the_profile_host_and_never_joins_a_container_network() {
        let arguments = arguments(&remote_mysql_process_spec(
            &target(),
            Path::new("/tmp/reprodb/client.cnf"),
            true,
            "reprodb-push-0123456789abcdef0123456789abcdef",
        ));

        assert!(arguments.contains(&"-i".to_owned()));
        assert!(!arguments.contains(&"-t".to_owned()));
        assert!(arguments.contains(&"--add-host=host.docker.internal:host-gateway".to_owned()));
        assert!(!arguments.iter().any(|argument| argument.starts_with("--network=")));
        assert!(arguments.contains(&format!(
            "--defaults-file={MYSQL_OPTION_FILE_CONTAINER_PATH}"
        )));
        assert!(arguments.iter().any(|argument| argument.ends_with(",readonly")));
        assert!(!arguments.join(" ").contains("remote-test-password"));
    }

    #[tokio::test]
    async fn the_destination_is_created_if_missing_and_never_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let artifact = crate::infrastructure::restore_artifact::test_support::validated_artifact(
            directory.path(),
            b"SELECT 1;\n",
        )
        .await;
        let arguments = arguments(&ensure_process_spec(
            &target(),
            artifact.metadata(),
            Path::new("/tmp/reprodb/client.cnf"),
            "reprodb-push-0123456789abcdef0123456789abcdef",
        ));
        let sql = arguments.last().unwrap();

        assert_eq!(arguments[arguments.len() - 2], "--execute");
        assert_eq!(
            sql,
            "CREATE DATABASE IF NOT EXISTS `salt_sagatec_qa` CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci;"
        );
        assert!(!sql.to_ascii_uppercase().contains("DROP"));
    }

    #[tokio::test]
    async fn the_import_uses_binary_mode_on_the_destination_database() {
        let directory = tempfile::tempdir().unwrap();
        let artifact = crate::infrastructure::restore_artifact::test_support::validated_artifact(
            directory.path(),
            b"SELECT 1;\n",
        )
        .await;
        let arguments = arguments(&import_process_spec(
            &target(),
            &artifact,
            Path::new("/tmp/reprodb/client.cnf"),
            "reprodb-push-0123456789abcdef0123456789abcdef",
        ));

        assert!(arguments.contains(&"--binary-mode".to_owned()));
        assert!(arguments.contains(&"--database=salt_sagatec_qa".to_owned()));
        assert!(arguments.contains(&"--default-character-set=utf8mb4".to_owned()));
    }

    #[test]
    fn a_missing_database_grant_is_a_permission_problem_not_a_bad_password() {
        assert_eq!(
            classify_remote_failure(
                b"ERROR 1044 (42000): Access denied for user 'qa'@'%' to database 'salt_sagatec_qa'"
            ),
            RemoteFailureKind::Permission
        );
        assert_eq!(
            classify_remote_failure(
                b"ERROR 1045 (28000): Access denied for user 'qa'@'10.0.0.1' (using password: YES)"
            ),
            RemoteFailureKind::Authentication
        );
        assert_eq!(
            classify_remote_failure(b"ERROR 2003 (HY000): Can't connect to MySQL server"),
            RemoteFailureKind::DestinationUnavailable
        );
    }

    #[test]
    fn failures_explain_that_a_rerun_imports_over_a_partial_database() {
        let interrupted = RemoteImportError::from(RestoreExecutorError::Interrupted);
        let failed = RemoteImportError::ImportFailed {
            exit_code: Some(1),
            kind: RemoteFailureKind::Sql,
            stderr_truncated: false,
        };

        for error in [interrupted, failed] {
            let message = error.to_string();
            assert!(message.contains("partially imported"));
            assert!(message.contains("rerunning the same push"));
        }
    }
}
```

- [ ] **Step 2: Register and run (red until registered)**

In `src/infrastructure/mysql/mod.rs`:
- change `mod restore_executor;` to `pub(crate) mod restore_executor;` only if the `use super::restore_executor::...` path does not resolve; sibling access via `crate::infrastructure::mysql::restore_executor::{...}` works with a private module because the importing module is inside `mysql`. Prefer keeping it `mod restore_executor;` and importing via `super::restore_executor::{classify_failure, run_without_stdin, stream_import}` — replace the `mysql::{ … restore_executor::{…} }` path in the `use` block above with that `super::` form if the compiler reports a privacy error.
- add `mod remote_import_executor;` and

```rust
pub use remote_import_executor::{
    DockerMysqlRemoteImportExecutor, RemoteFailureKind, RemoteImportError, RemoteImportExecutor,
};
```

Run: `cargo test --lib remote_import_executor`
Expected: 5 PASS.

- [ ] **Step 3: Gate and commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
git add src/infrastructure/mysql/remote_import_executor.rs src/infrastructure/mysql/mod.rs
git commit -m "feat: import validated dumps into a remote MySQL destination"
```

---

### Task 5: Push service

**Files:**
- Create: `src/application/push_service.rs`
- Modify: `src/application/mod.rs`
- Modify: `src/application/restore_service.rs:158` (`fn dump_choices` → `pub(crate) fn dump_choices`)
- Modify: `src/infrastructure/operation_lock.rs` (`Remote` scope + `OperationLockKey::remote`)

**Interfaces:**
- Consumes: Task 2 (`RemoteTargetGate`, `RemoteTargetProbe`, `RemoteProfileChoice`, `RemoteTargetGateError`), Task 4 (`RemoteImportExecutor`, `RemoteImportError`); `RestoreService::dump_choices(&self, &DatabaseName) -> Result<Vec<RestoreDumpChoice>, RestoreServiceError>`; `DumpService::{new, with_progress, with_status, create}`; `PullDumpDependencies<'a> { preflight, executor }`; `LocalRestoreArtifactValidator::validate_by_id(RestoreArtifactLookup) -> Result<ValidatedRestoreArtifact, RestoreArtifactError>`.
- Produces:
  - `OperationLockKey::remote(profile: &ProfileName, database: &DatabaseName) -> OperationLockKey`
  - `pub enum PushDumpChoice { Existing(DumpId), Fresh }`
  - `pub struct PushPlan { pub source_profile: ProfileName, pub source_database: DatabaseName, pub dump_id: DumpId, pub source_version: MysqlVersion, pub destination_profile: ProfileName, pub destination_host: String, pub destination_port: u16, pub database: DatabaseName, pub destination_version: MysqlVersion }`
  - `pub trait PushSelector: Send + Sync { fn profile(&self, &[RemoteProfileChoice]) -> Result<ProfileName, PushSelectionError>; fn dump(&self, &DatabaseName, &[RestoreDumpChoice]) -> Result<PushDumpChoice, PushSelectionError>; fn database(&self, source_database: &DatabaseName) -> Result<DatabaseName, PushSelectionError>; fn confirm(&self, plan: &PushPlan) -> Result<bool, PushSelectionError>; }`
  - `pub enum PushSelectionError { Unavailable(String), UnknownProfile(ProfileName), UnknownDump(DumpId) }`
  - `pub enum PushProgress { CreatingDump, DumpReady(DumpId), Importing }`, `pub trait PushProgressObserver`, `NoPushProgress`
  - `pub enum PushOutcome { Ready(PushReady), Cancelled }`, `pub struct PushReady { pub plan: PushPlan, pub imported_bytes: u64 }`
  - `pub struct PushService` with `new`, `with_progress`, `with_compression_progress`, `with_dump_status`, `async push<E: RemoteImportExecutor>(&self, credentials: &dyn CredentialStore, probe: &dyn RemoteTargetProbe, dump: PullDumpDependencies<'_>, executor: E, selector: &dyn PushSelector, database: DatabaseName) -> Result<PushOutcome, PushServiceError>`
  - `pub enum PushServiceError { NoEligibleProfile, Selection, Target, DumpChoices, Dump, Artifact, Lock, Import, ImportedSizeMismatch }`

- [ ] **Step 1: Add the remote lock key**

In `src/infrastructure/operation_lock.rs` add the variant `Remote` to `OperationLockScope`, map it to `"remote"` in `directory()`, and add to `impl OperationLockKey`:

```rust
    pub fn remote(profile: &ProfileName, database: &DatabaseName) -> Self {
        Self::new(
            OperationLockScope::Remote,
            [profile.as_str().as_bytes(), database.as_str().as_bytes()],
        )
    }
```

Add to that file's tests module:

```rust
    #[test]
    fn remote_and_source_keys_for_the_same_names_never_collide() {
        let profile = ProfileName::try_from("sandbox").unwrap();
        let database = DatabaseName::try_from("salt_sagatec").unwrap();

        assert_ne!(
            OperationLockKey::remote(&profile, &database),
            OperationLockKey::source(&profile, &database)
        );
    }
```

Check `src/error.rs` and any `match` on `OperationLockScope` (`rtk proxy grep -rn "OperationLockScope::" src`) and add the `Remote` arm where needed (e.g. a Busy message such as "another push to this destination is running").

Change `fn dump_choices(` in `src/application/restore_service.rs` to `pub(crate) fn dump_choices(`.

- [ ] **Step 2: Write the service and its failing tests**

Create `src/application/push_service.rs`:

```rust
use std::sync::Arc;

use thiserror::Error;

use crate::{
    application::{
        DumpService, DumpServiceError, DumpStatusObserver, NoDumpStatus, PullDumpDependencies,
        RemoteProfileChoice, RemoteTargetGate, RemoteTargetGateError, RemoteTargetProbe,
        RestoreDumpChoice, RestoreService, RestoreServiceError,
    },
    domain::{DatabaseName, DumpId, MysqlVersion, ProfileName},
    infrastructure::{
        compression::{CompressionProgressObserver, NoCompressionProgress},
        config::ConfigRepository,
        credentials::CredentialStore,
        mysql::{RemoteImportError, RemoteImportExecutor},
        operation_lock::{OperationLockError, OperationLockKey, OperationLockManager},
        restore_artifact::{
            LocalRestoreArtifactValidator, RestoreArtifactError, RestoreArtifactLookup,
        },
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PushDumpChoice {
    Existing(DumpId),
    /// Create a new dump from the active profile, exactly like `pull`.
    Fresh,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushPlan {
    pub source_profile: ProfileName,
    pub source_database: DatabaseName,
    pub dump_id: DumpId,
    pub source_version: MysqlVersion,
    pub destination_profile: ProfileName,
    pub destination_host: String,
    pub destination_port: u16,
    pub database: DatabaseName,
    pub destination_version: MysqlVersion,
}

/// Every choice `push` needs from the user.
///
/// Implementations must refuse rather than guess when they cannot ask: each
/// answer decides what is written to a server other than the local target.
pub trait PushSelector: Send + Sync {
    fn profile(&self, choices: &[RemoteProfileChoice]) -> Result<ProfileName, PushSelectionError>;

    fn dump(
        &self,
        database: &DatabaseName,
        choices: &[RestoreDumpChoice],
    ) -> Result<PushDumpChoice, PushSelectionError>;

    fn database(&self, source_database: &DatabaseName) -> Result<DatabaseName, PushSelectionError>;

    fn confirm(&self, plan: &PushPlan) -> Result<bool, PushSelectionError>;
}

#[derive(Debug, Error)]
pub enum PushSelectionError {
    #[error("{0}")]
    Unavailable(String),

    #[error(
        "`{0}` is not an eligible push destination; production profiles are never accepted (see `reprodb profile list`)"
    )]
    UnknownProfile(ProfileName),

    #[error("dump `{0}` is not stored for this database; see `reprodb cache list`")]
    UnknownDump(DumpId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PushProgress {
    CreatingDump,
    DumpReady(DumpId),
    Importing,
}

pub trait PushProgressObserver: Send + Sync {
    fn update(&self, progress: &PushProgress);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoPushProgress;

impl PushProgressObserver for NoPushProgress {
    fn update(&self, _progress: &PushProgress) {}
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushReady {
    pub plan: PushPlan,
    pub imported_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PushOutcome {
    Ready(PushReady),
    Cancelled,
}

pub struct PushService {
    repository: ConfigRepository,
    progress: Arc<dyn PushProgressObserver>,
    compression_progress: Arc<dyn CompressionProgressObserver>,
    dump_status: Arc<dyn DumpStatusObserver>,
}

impl PushService {
    pub fn new(repository: ConfigRepository) -> Self {
        Self {
            repository,
            progress: Arc::new(NoPushProgress),
            compression_progress: Arc::new(NoCompressionProgress),
            dump_status: Arc::new(NoDumpStatus),
        }
    }

    pub fn with_progress(mut self, progress: Arc<dyn PushProgressObserver>) -> Self {
        self.progress = progress;
        self
    }

    pub fn with_compression_progress(
        mut self,
        progress: Arc<dyn CompressionProgressObserver>,
    ) -> Self {
        self.compression_progress = progress;
        self
    }

    pub fn with_dump_status(mut self, status: Arc<dyn DumpStatusObserver>) -> Self {
        self.dump_status = status;
        self
    }

    pub async fn push<E>(
        &self,
        credentials: &dyn CredentialStore,
        probe: &dyn RemoteTargetProbe,
        dump: PullDumpDependencies<'_>,
        executor: E,
        selector: &dyn PushSelector,
        database: DatabaseName,
    ) -> Result<PushOutcome, PushServiceError>
    where
        E: RemoteImportExecutor,
    {
        let gate = RemoteTargetGate::new(self.repository.clone());
        let profiles = gate.eligible_profiles()?;
        if profiles.is_empty() {
            return Err(PushServiceError::NoEligibleProfile);
        }
        let profile = selector.profile(&profiles)?;
        // The destination is attested before any dump work: a wrong password
        // or an unreachable sandbox should not cost a production export.
        let guarded = gate.verify(credentials, probe, &profile).await?;

        let choices = RestoreService::new(self.repository.clone()).dump_choices(&database)?;
        let dump_id = match selector.dump(&database, &choices)? {
            PushDumpChoice::Existing(dump_id) => dump_id,
            PushDumpChoice::Fresh => {
                self.progress.update(&PushProgress::CreatingDump);
                let created = DumpService::new(self.repository.clone())
                    .with_progress(Arc::clone(&self.compression_progress))
                    .with_status(Arc::clone(&self.dump_status))
                    .create(credentials, dump.preflight, dump.executor, database.clone())
                    .await?;
                self.progress.update(&PushProgress::DumpReady(created.dump_id));
                created.dump_id
            }
        };
        let cache_dir = self.repository.paths().cache_dir();
        let artifact = LocalRestoreArtifactValidator::new(cache_dir.clone())
            .validate_by_id(RestoreArtifactLookup {
                database: &database,
                dump_id,
            })
            .await?;
        let metadata = artifact.metadata();
        let target_database = selector.database(&metadata.database)?;
        let target = guarded.authorize(target_database, metadata)?;
        let plan = PushPlan {
            source_profile: metadata.profile.clone(),
            source_database: metadata.database.clone(),
            dump_id: metadata.dump_id,
            source_version: metadata.source_version,
            destination_profile: target.profile().clone(),
            destination_host: target.host().to_owned(),
            destination_port: target.port(),
            database: target.database().clone(),
            destination_version: target.server_version(),
        };
        if !selector.confirm(&plan)? {
            return Ok(PushOutcome::Cancelled);
        }

        let _lock = OperationLockManager::new(cache_dir)
            .try_acquire(OperationLockKey::remote(target.profile(), target.database()))?;
        self.progress.update(&PushProgress::Importing);
        executor.ensure_database(&target, metadata).await?;
        let metrics = executor.import(&target, &artifact).await?;
        if metrics.imported_bytes() != metadata.uncompressed_bytes {
            return Err(PushServiceError::ImportedSizeMismatch);
        }

        Ok(PushOutcome::Ready(PushReady {
            plan,
            imported_bytes: metrics.imported_bytes(),
        }))
    }
}

#[derive(Debug, Error)]
pub enum PushServiceError {
    #[error(
        "no non-production profile is configured as a push destination; run `reprodb profile add NAME`"
    )]
    NoEligibleProfile,

    #[error(transparent)]
    Selection(#[from] PushSelectionError),

    #[error(transparent)]
    Target(#[from] RemoteTargetGateError),

    #[error(transparent)]
    DumpChoices(#[from] RestoreServiceError),

    #[error(transparent)]
    Dump(#[from] DumpServiceError),

    #[error(transparent)]
    Artifact(#[from] RestoreArtifactError),

    #[error(transparent)]
    Lock(#[from] OperationLockError),

    #[error(transparent)]
    Import(#[from] RemoteImportError),

    #[error(
        "mysql consumed a different number of bytes than the validated dump; the remote database may be partially imported"
    )]
    ImportedSizeMismatch,
}
```

If `paths().cache_dir()` returns `&Path` rather than `PathBuf`, drop the `.clone()` and pass it directly (match `RestoreService::restore_to`, which calls `self.repository.paths().cache_dir()` inline twice).

Append the tests module to the same file:

```rust
#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use secrecy::SecretString;
    use tempfile::tempdir;

    use crate::{
        application::{
            AuthorizedRemoteTarget, DumpPreflightGateway, DumpSource, RemoteTargetProbeRequest,
        },
        domain::{CredentialKey, CredentialScope, DumpArtifactMetadata, MysqlTlsMode},
        infrastructure::{
            compression::CompressionMetrics,
            config::{
                AppConfig, AppPaths, ClientRuntimeConfig, MysqlClientConfig, MysqlFamily,
                SourceProfileConfig,
            },
            credentials::MemoryCredentialStore,
            mysql::{
                ApprovedMysqlDump, ClientCatalog, DockerClientError, DumpExecutionRequest,
                DumpExecutor, DumpExecutorError, DumpPreflightError, MysqlServerInfo,
                RestoreMetrics,
            },
            restore_artifact::{ValidatedRestoreArtifact, test_support::validated_artifact},
        },
    };

    use super::*;

    struct FakeProbe;

    #[async_trait]
    impl RemoteTargetProbe for FakeProbe {
        async fn probe(
            &self,
            _request: RemoteTargetProbeRequest<'_>,
        ) -> Result<MysqlServerInfo, DockerClientError> {
            Ok(MysqlServerInfo {
                version: "8.4.4".parse().unwrap(),
                vendor: "MySQL Community Server - GPL".to_owned(),
                server_uuid: "33333333-3333-4333-8333-333333333333".parse().unwrap(),
                tls_cipher: Some("TLS_AES_256_GCM_SHA384".to_owned()),
            })
        }
    }

    /// `push` must never reach the dump path when a cached dump is chosen.
    struct UnusedDump;

    #[async_trait]
    impl DumpPreflightGateway for UnusedDump {
        async fn assess(
            &self,
            _source: &DumpSource<'_>,
            _database: &DatabaseName,
        ) -> Result<ApprovedMysqlDump, DumpPreflightError> {
            panic!("a cached push must not contact the source");
        }
    }

    #[async_trait]
    impl DumpExecutor for UnusedDump {
        async fn execute(
            &self,
            _request: DumpExecutionRequest<'_>,
            _output: std::fs::File,
        ) -> Result<CompressionMetrics, DumpExecutorError> {
            panic!("a cached push must not dump");
        }
    }

    struct FakeExecutor {
        calls: Arc<Mutex<Vec<&'static str>>>,
        imported_bytes_delta: i64,
    }

    #[async_trait]
    impl RemoteImportExecutor for FakeExecutor {
        async fn ensure_database(
            &self,
            _target: &AuthorizedRemoteTarget,
            _metadata: &DumpArtifactMetadata,
        ) -> Result<(), RemoteImportError> {
            self.calls.lock().unwrap().push("ensure");
            Ok(())
        }

        async fn import(
            &self,
            _target: &AuthorizedRemoteTarget,
            artifact: &ValidatedRestoreArtifact,
        ) -> Result<RestoreMetrics, RemoteImportError> {
            self.calls.lock().unwrap().push("import");
            let bytes = artifact.metadata().uncompressed_bytes as i64 + self.imported_bytes_delta;
            Ok(RestoreMetrics::new_for_test(bytes as u64))
        }
    }

    struct FakeSelector {
        dump: PushDumpChoice,
        database: &'static str,
        confirm: bool,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl PushSelector for FakeSelector {
        fn profile(
            &self,
            choices: &[RemoteProfileChoice],
        ) -> Result<ProfileName, PushSelectionError> {
            self.calls.lock().unwrap().push("profile");
            Ok(choices[0].profile.clone())
        }

        fn dump(
            &self,
            _database: &DatabaseName,
            _choices: &[RestoreDumpChoice],
        ) -> Result<PushDumpChoice, PushSelectionError> {
            self.calls.lock().unwrap().push("dump");
            Ok(self.dump)
        }

        fn database(
            &self,
            _source_database: &DatabaseName,
        ) -> Result<DatabaseName, PushSelectionError> {
            self.calls.lock().unwrap().push("database");
            Ok(DatabaseName::try_from(self.database).unwrap())
        }

        fn confirm(&self, _plan: &PushPlan) -> Result<bool, PushSelectionError> {
            self.calls.lock().unwrap().push("confirm");
            Ok(self.confirm)
        }
    }

    async fn fixture(
        root: &std::path::Path,
        with_sandbox: bool,
    ) -> (ConfigRepository, MemoryCredentialStore, DumpId) {
        let paths = AppPaths::new(root.join("config"), root.join("cache"), root.join("data"));
        let repository = ConfigRepository::new(paths.clone());
        let key = CredentialKey::new(CredentialScope::Source);
        let mut profiles = BTreeMap::new();
        if with_sandbox {
            profiles.insert(
                ProfileName::try_from("sandbox").unwrap(),
                SourceProfileConfig {
                    host: "sandbox.db.internal".to_owned(),
                    port: 3306,
                    username: "sandbox_writer".to_owned(),
                    credential_key: key,
                    mysql_family: MysqlFamily::Mysql,
                    mysql_series: "8.4".to_owned(),
                    production: false,
                    tls_mode: MysqlTlsMode::Required,
                    tls_material: Default::default(),
                    client: MysqlClientConfig {
                        image: ClientCatalog::resolve("8.4").unwrap().image().to_owned(),
                    },
                },
            );
        }
        repository
            .save(&AppConfig {
                client_runtime: ClientRuntimeConfig {
                    docker_context: Some("desktop-linux".to_owned()),
                    ..ClientRuntimeConfig::default()
                },
                profiles,
                ..AppConfig::default()
            })
            .unwrap();
        let credentials = MemoryCredentialStore::default();
        credentials
            .set(&key, SecretString::from("sandbox-password"))
            .await
            .unwrap();
        let artifact = validated_artifact(&paths.cache_dir(), b"CREATE TABLE `t` (`id` INT);\n").await;
        let dump_id = artifact.metadata().dump_id;
        drop(artifact);
        (repository, credentials, dump_id)
    }

    fn acme() -> DatabaseName {
        DatabaseName::try_from("acme_production").unwrap()
    }

    #[tokio::test]
    async fn a_confirmed_push_attests_the_destination_then_creates_and_imports() {
        let directory = tempdir().unwrap();
        let (repository, credentials, dump_id) = fixture(directory.path(), true).await;
        let calls = Arc::new(Mutex::new(Vec::new()));

        let outcome = PushService::new(repository)
            .push(
                &credentials,
                &FakeProbe,
                PullDumpDependencies {
                    preflight: &UnusedDump,
                    executor: &UnusedDump,
                },
                FakeExecutor {
                    calls: Arc::clone(&calls),
                    imported_bytes_delta: 0,
                },
                &FakeSelector {
                    dump: PushDumpChoice::Existing(dump_id),
                    database: "acme_qa",
                    confirm: true,
                    calls: Arc::clone(&calls),
                },
                acme(),
            )
            .await
            .unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            ["profile", "dump", "database", "confirm", "ensure", "import"]
        );
        let PushOutcome::Ready(ready) = outcome else {
            panic!("expected a completed push");
        };
        assert_eq!(ready.plan.destination_profile.as_str(), "sandbox");
        assert_eq!(ready.plan.database.as_str(), "acme_qa");
        assert_eq!(ready.plan.source_database, acme());
        assert_eq!(ready.plan.dump_id, dump_id);
    }

    #[tokio::test]
    async fn a_declined_confirmation_never_reaches_the_executor() {
        let directory = tempdir().unwrap();
        let (repository, credentials, dump_id) = fixture(directory.path(), true).await;
        let calls = Arc::new(Mutex::new(Vec::new()));

        let outcome = PushService::new(repository)
            .push(
                &credentials,
                &FakeProbe,
                PullDumpDependencies {
                    preflight: &UnusedDump,
                    executor: &UnusedDump,
                },
                FakeExecutor {
                    calls: Arc::clone(&calls),
                    imported_bytes_delta: 0,
                },
                &FakeSelector {
                    dump: PushDumpChoice::Existing(dump_id),
                    database: "acme_qa",
                    confirm: false,
                    calls: Arc::clone(&calls),
                },
                acme(),
            )
            .await
            .unwrap();

        assert_eq!(outcome, PushOutcome::Cancelled);
        assert!(!calls.lock().unwrap().contains(&"ensure"));
        assert!(!calls.lock().unwrap().contains(&"import"));
    }

    #[tokio::test]
    async fn without_a_non_production_profile_nothing_is_asked() {
        let directory = tempdir().unwrap();
        let (repository, credentials, dump_id) = fixture(directory.path(), false).await;
        let calls = Arc::new(Mutex::new(Vec::new()));

        let error = PushService::new(repository)
            .push(
                &credentials,
                &FakeProbe,
                PullDumpDependencies {
                    preflight: &UnusedDump,
                    executor: &UnusedDump,
                },
                FakeExecutor {
                    calls: Arc::clone(&calls),
                    imported_bytes_delta: 0,
                },
                &FakeSelector {
                    dump: PushDumpChoice::Existing(dump_id),
                    database: "acme_qa",
                    confirm: true,
                    calls: Arc::clone(&calls),
                },
                acme(),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, PushServiceError::NoEligibleProfile));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_short_import_is_reported_as_a_failure() {
        let directory = tempdir().unwrap();
        let (repository, credentials, dump_id) = fixture(directory.path(), true).await;
        let calls = Arc::new(Mutex::new(Vec::new()));

        let error = PushService::new(repository)
            .push(
                &credentials,
                &FakeProbe,
                PullDumpDependencies {
                    preflight: &UnusedDump,
                    executor: &UnusedDump,
                },
                FakeExecutor {
                    calls: Arc::clone(&calls),
                    imported_bytes_delta: -1,
                },
                &FakeSelector {
                    dump: PushDumpChoice::Existing(dump_id),
                    database: "acme_qa",
                    confirm: true,
                    calls,
                },
                acme(),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, PushServiceError::ImportedSizeMismatch));
    }
}
```

The fixture's dump is server `11111111-…`, the fake destination is `33333333-…`, so no collision. The fixture config has no `local_target`; if `AppConfig` validation requires one, copy the `local_target` block from `restore_service.rs` tests (`configured_fixture`).

- [ ] **Step 3: Register and run (red until registered)**

In `src/application/mod.rs` add `mod push_service;` and:

```rust
pub use push_service::{
    NoPushProgress, PushDumpChoice, PushOutcome, PushPlan, PushProgress, PushProgressObserver,
    PushReady, PushSelectionError, PushSelector, PushService, PushServiceError,
};
```

Run: `cargo test --lib push_service && cargo test --lib operation_lock`
Expected: all PASS.

- [ ] **Step 4: Gate and commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
git add src/application/push_service.rs src/application/mod.rs src/application/restore_service.rs src/infrastructure/operation_lock.rs
git commit -m "feat: orchestrate pushing a managed dump to another profile"
```

---

### Task 6: CLI command, prompts and exit codes

**Files:**
- Create: `src/cli/push.rs`
- Modify: `src/cli/mod.rs` (`pub mod push;`, `Commands::Push`, `PushArgs`, `name()`, parse tests)
- Modify: `src/cli/prompt.rs` (four prompt functions)
- Modify: `src/lib.rs` (dispatch)
- Modify: `src/error.rs` (`AppError::Push`, `push_error_category`, tests)

**Interfaces:**
- Consumes: Task 5 public API; Task 3 `DockerRemoteTargetProbe`; Task 4 `DockerMysqlRemoteImportExecutor`, `RemoteImportError`, `RemoteFailureKind`; `CliDumpProgress` (`src/cli/dump.rs`) for fresh-dump progress; `crate::cli::cache::format_duration`, `crate::cli::dump::format_bytes`.
- Produces: `reprodb push <DATABASE> [--profile P] [--dump-id ID | --fresh] [--database NAME] [--yes]`.

- [ ] **Step 1: Write the failing parse test**

Add to `src/cli/mod.rs` tests:

```rust
    #[test]
    fn parses_push_with_every_flag_and_rejects_dump_id_with_fresh() {
        let cli = Cli::try_parse_from([
            "reprodb",
            "push",
            "salt_sagatec",
            "--profile",
            "sandbox",
            "--dump-id",
            "550e8400-e29b-41d4-a716-446655440000",
            "--database",
            "salt_sagatec_qa",
            "--yes",
        ])
        .unwrap();
        let Commands::Push(push) = cli.command else {
            panic!("expected push command")
        };
        assert_eq!(push.database, "salt_sagatec");
        assert_eq!(push.profile.as_deref(), Some("sandbox"));
        assert_eq!(push.target_database.as_deref(), Some("salt_sagatec_qa"));
        assert!(push.yes);
        assert!(!push.fresh);

        assert!(
            Cli::try_parse_from([
                "reprodb",
                "push",
                "salt_sagatec",
                "--fresh",
                "--dump-id",
                "550e8400-e29b-41d4-a716-446655440000",
            ])
            .is_err()
        );
    }
```

Run: `cargo test --lib parses_push_with_every_flag` → Expected: compile FAIL (`Commands::Push` missing).

- [ ] **Step 2: Add `PushArgs` and the command**

In `src/cli/mod.rs`: add `pub mod push;` (alphabetical), the variant

```rust
    /// Import a managed dump into a database on another, non-production profile.
    Push(PushArgs),
```

after `Pull(PullArgs)`, `Self::Push(_) => "push",` in `name()`, and:

```rust
#[derive(Debug, Args)]
pub struct PushArgs {
    /// Source database name stored in the managed dump.
    #[arg(value_name = "DATABASE")]
    pub database: String,

    /// Destination profile. Production profiles are always refused.
    #[arg(long, value_name = "PROFILE")]
    pub profile: Option<String>,

    /// ID of a stored dump to push. Omit to choose interactively.
    #[arg(long, value_name = "ID", conflicts_with = "fresh")]
    pub dump_id: Option<String>,

    /// Create a new dump from the active profile and push it.
    #[arg(long)]
    pub fresh: bool,

    /// Destination database name; defaults to DATABASE.
    #[arg(long = "database", value_name = "DATABASE")]
    pub target_database: Option<String>,

    /// Skip the typed confirmation.
    #[arg(long, short = 'y')]
    pub yes: bool,
}
```

Run the parse test → PASS.

- [ ] **Step 3: Add the prompts**

Append to `src/cli/prompt.rs` (before `#[cfg(test)]`):

```rust
pub fn select_push_profile(labels: &[String]) -> Result<usize, PromptError> {
    Select::with_theme(&SimpleTheme)
        .with_prompt("Destination profile")
        .items(labels)
        .default(0)
        .interact()
        .map_err(unavailable)
}

pub fn select_push_dump(labels: &[String]) -> Result<usize, PromptError> {
    Select::with_theme(&SimpleTheme)
        .with_prompt("Dump to push")
        .items(labels)
        .default(0)
        .interact()
        .map_err(unavailable)
}

pub fn select_push_database(default: &DatabaseName) -> Result<DatabaseName, PromptError> {
    let value = Input::<String>::with_theme(&SimpleTheme)
        .with_prompt("Remote database")
        .default(default.as_str().to_owned())
        .validate_with(|value: &String| -> Result<(), &str> {
            DatabaseName::try_from(value.as_str())
                .map(|_| ())
                .map_err(|_| "enter a safe non-administrative database name")
        })
        .interact_text()
        .map_err(unavailable)?;
    DatabaseName::try_from(value).map_err(|_| PromptError::InvalidRestoreDatabase)
}

/// Returns `true` only when the user retypes the destination database name.
pub fn confirm_push_database(database: &DatabaseName) -> Result<bool, PromptError> {
    let typed = Input::<String>::with_theme(&SimpleTheme)
        .with_prompt("Type the database name to confirm")
        .allow_empty(true)
        .interact_text()
        .map_err(unavailable)?;
    Ok(typed.trim() == database.as_str())
}
```

- [ ] **Step 4: Write `src/cli/push.rs` with failing tests**

```rust
use std::io::Write as _;

use crate::{
    application::{
        PushDumpChoice, PushPlan, PushProgress, PushProgressObserver, PushReady,
        PushSelectionError, PushSelector, RemoteProfileChoice, RestoreDumpChoice,
    },
    cli::{cache::format_duration, dump::CliDumpProgress, dump::format_bytes, output::OutputStyle, prompt},
    domain::{DatabaseName, DumpId, ProfileName},
    infrastructure::compression::{CompressionProgress, CompressionProgressObserver},
};

pub struct CliPushSelector {
    style: OutputStyle,
    profile: Option<ProfileName>,
    dump_id: Option<DumpId>,
    fresh: bool,
    database: Option<DatabaseName>,
    yes: bool,
    interactive: bool,
    now_unix_seconds: u64,
}

impl CliPushSelector {
    pub fn new(
        style: OutputStyle,
        profile: Option<ProfileName>,
        dump_id: Option<DumpId>,
        fresh: bool,
        database: Option<DatabaseName>,
        yes: bool,
    ) -> Self {
        Self {
            style,
            profile,
            dump_id,
            fresh,
            database,
            yes,
            interactive: prompt::is_interactive(),
            now_unix_seconds: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs()),
        }
    }
}

impl PushSelector for CliPushSelector {
    fn profile(&self, choices: &[RemoteProfileChoice]) -> Result<ProfileName, PushSelectionError> {
        if let Some(requested) = &self.profile {
            return choices
                .iter()
                .find(|choice| choice.profile == *requested)
                .map(|choice| choice.profile.clone())
                .ok_or_else(|| PushSelectionError::UnknownProfile(requested.clone()));
        }
        if !self.interactive {
            return Err(PushSelectionError::Unavailable(format!(
                "no terminal to choose the destination on; rerun with --profile (one of: {})",
                choices
                    .iter()
                    .map(|choice| choice.profile.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        let labels = choices
            .iter()
            .map(|choice| format!("{}  {}:{}", choice.profile, choice.host, choice.port))
            .collect::<Vec<_>>();
        let index = prompt::select_push_profile(&labels)
            .map_err(|error| PushSelectionError::Unavailable(error.to_string()))?;
        Ok(choices[index].profile.clone())
    }

    fn dump(
        &self,
        _database: &DatabaseName,
        choices: &[RestoreDumpChoice],
    ) -> Result<PushDumpChoice, PushSelectionError> {
        if self.fresh {
            return Ok(PushDumpChoice::Fresh);
        }
        if let Some(dump_id) = self.dump_id {
            return choices
                .iter()
                .any(|choice| choice.dump_id == dump_id)
                .then_some(PushDumpChoice::Existing(dump_id))
                .ok_or(PushSelectionError::UnknownDump(dump_id));
        }
        if !self.interactive {
            return Err(PushSelectionError::Unavailable(
                "no terminal to choose a dump on; rerun with --fresh or --dump-id ID (see `reprodb cache list`)"
                    .to_owned(),
            ));
        }
        let mut labels = vec!["Generate a new dump now (active profile)".to_owned()];
        labels.extend(
            choices
                .iter()
                .map(|choice| render_dump_choice(self.now_unix_seconds, choice)),
        );
        let index = prompt::select_push_dump(&labels)
            .map_err(|error| PushSelectionError::Unavailable(error.to_string()))?;
        Ok(match index {
            0 => PushDumpChoice::Fresh,
            index => PushDumpChoice::Existing(choices[index - 1].dump_id),
        })
    }

    fn database(&self, source_database: &DatabaseName) -> Result<DatabaseName, PushSelectionError> {
        if let Some(database) = &self.database {
            return Ok(database.clone());
        }
        if !self.interactive {
            return Ok(source_database.clone());
        }
        prompt::select_push_database(source_database)
            .map_err(|error| PushSelectionError::Unavailable(error.to_string()))
    }

    fn confirm(&self, plan: &PushPlan) -> Result<bool, PushSelectionError> {
        print!("{}", render_plan(&self.style, plan));
        let _ = std::io::stdout().flush();
        if self.yes {
            return Ok(true);
        }
        if !self.interactive {
            return Err(PushSelectionError::Unavailable(
                "push writes to another server and there is no terminal to confirm on; rerun with --yes"
                    .to_owned(),
            ));
        }
        prompt::confirm_push_database(&plan.database)
            .map_err(|error| PushSelectionError::Unavailable(error.to_string()))
    }
}

fn render_dump_choice(now_unix_seconds: u64, choice: &RestoreDumpChoice) -> String {
    let age = if choice.completed_at_unix_seconds > now_unix_seconds {
        "from the future".to_owned()
    } else {
        format!(
            "{} ago",
            format_duration(now_unix_seconds - choice.completed_at_unix_seconds)
        )
    };
    format!(
        "{}  {:>14}  {:>10}  {}  MySQL {}",
        choice.dump_id,
        age,
        format_bytes(choice.compressed_bytes),
        choice.profile,
        choice.source_version,
    )
}

pub fn render_start(style: &OutputStyle) -> String {
    format!(
        "{} push\n{} Validating the destination profile...\n",
        style.brand("reprodb"),
        style.attention("○"),
    )
}

pub fn render_plan(style: &OutputStyle, plan: &PushPlan) -> String {
    format!(
        "\n{}\n  Source:       {} / {}\n  Dump ID:      {}\n  Destination:  {} ({}:{}) / {}\n  MySQL:        {} -> {}\n\n{} Tables present in the dump will be replaced in the remote database.\n",
        style.section("Push plan"),
        style.value(plan.source_profile.as_str()),
        style.value(plan.source_database.as_str()),
        style.value(&plan.dump_id.to_string()),
        style.value(plan.destination_profile.as_str()),
        plan.destination_host,
        plan.destination_port,
        style.value(plan.database.as_str()),
        plan.source_version,
        plan.destination_version,
        style.attention("!"),
    )
}

pub fn render_complete(style: &OutputStyle, ready: &PushReady) -> String {
    format!(
        "\n{} Push complete\n\n  Destination:  {} / {}\n  Imported:     {}\n  Dump ID:      {}\n",
        style.success("✓"),
        style.value(ready.plan.destination_profile.as_str()),
        style.value(ready.plan.database.as_str()),
        format_bytes(ready.imported_bytes),
        style.value(&ready.plan.dump_id.to_string()),
    )
}

pub fn render_cancelled(style: &OutputStyle) -> String {
    format!(
        "{} Confirmation did not match; nothing was written.\n",
        style.attention("!")
    )
}

pub struct CliPushProgress {
    style: OutputStyle,
    dump: CliDumpProgress,
}

impl CliPushProgress {
    pub fn new(style: OutputStyle) -> Self {
        Self {
            style,
            dump: CliDumpProgress::new(style),
        }
    }

    pub fn finish(&self) {
        self.dump.finish();
    }
}

impl PushProgressObserver for CliPushProgress {
    fn update(&self, progress: &PushProgress) {
        match progress {
            PushProgress::CreatingDump => println!(
                "{} Exporting a new dump from the active profile...",
                self.style.attention("○")
            ),
            PushProgress::DumpReady(dump_id) => {
                self.dump.finish();
                println!(
                    "{} Managed dump ready · {}",
                    self.style.success("✓"),
                    self.style.muted(&dump_id.to_string()),
                );
            }
            PushProgress::Importing => println!(
                "{} Creating the database if missing and streaming the validated dump...",
                self.style.attention("○")
            ),
        }
        let _ = std::io::stdout().flush();
    }
}

impl CompressionProgressObserver for CliPushProgress {
    fn set_estimated_input_bytes(&self, estimated_input_bytes: u64) {
        self.dump.set_estimated_input_bytes(estimated_input_bytes);
    }

    fn update(&self, progress: CompressionProgress) {
        self.dump.update(progress);
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::MysqlVersion;

    use super::*;

    fn selector() -> CliPushSelector {
        CliPushSelector {
            style: OutputStyle::plain(),
            profile: None,
            dump_id: None,
            fresh: false,
            database: None,
            yes: false,
            interactive: false,
            now_unix_seconds: 1_000,
        }
    }

    fn sandbox() -> Vec<RemoteProfileChoice> {
        vec![RemoteProfileChoice {
            profile: ProfileName::try_from("sandbox").unwrap(),
            host: "sandbox.db.internal".to_owned(),
            port: 3306,
        }]
    }

    fn plan() -> PushPlan {
        PushPlan {
            source_profile: ProfileName::try_from("prod-source").unwrap(),
            source_database: DatabaseName::try_from("salt_sagatec").unwrap(),
            dump_id: DumpId::new(),
            source_version: "8.0.45".parse::<MysqlVersion>().unwrap(),
            destination_profile: ProfileName::try_from("sandbox").unwrap(),
            destination_host: "sandbox.db.internal".to_owned(),
            destination_port: 3306,
            database: DatabaseName::try_from("salt_sagatec_qa").unwrap(),
            destination_version: "8.4.4".parse::<MysqlVersion>().unwrap(),
        }
    }

    #[test]
    fn without_a_terminal_the_profile_and_confirmation_must_come_from_flags() {
        let profile = selector().profile(&sandbox()).unwrap_err();
        let confirm = selector().confirm(&plan()).unwrap_err();
        let dump = selector()
            .dump(&DatabaseName::try_from("salt_sagatec").unwrap(), &[])
            .unwrap_err();

        assert!(profile.to_string().contains("--profile"));
        assert!(profile.to_string().contains("sandbox"));
        assert!(confirm.to_string().contains("--yes"));
        assert!(dump.to_string().contains("--fresh"));
    }

    #[test]
    fn a_production_or_unknown_profile_flag_explains_production_is_never_accepted() {
        let error = CliPushSelector {
            profile: Some(ProfileName::try_from("prod-source").unwrap()),
            ..selector()
        }
        .profile(&sandbox())
        .unwrap_err();

        assert!(error.to_string().contains("production profiles are never accepted"));
    }

    #[test]
    fn a_dump_id_of_another_database_is_refused_before_connecting() {
        let dump_id = DumpId::new();
        let error = CliPushSelector {
            dump_id: Some(dump_id),
            ..selector()
        }
        .dump(&DatabaseName::try_from("salt_sagatec").unwrap(), &[])
        .unwrap_err();

        assert!(matches!(error, PushSelectionError::UnknownDump(id) if id == dump_id));
    }

    #[test]
    fn flags_answer_without_prompting() {
        let selector = CliPushSelector {
            profile: Some(ProfileName::try_from("sandbox").unwrap()),
            fresh: true,
            database: Some(DatabaseName::try_from("salt_sagatec_qa").unwrap()),
            yes: true,
            ..selector()
        };
        let source = DatabaseName::try_from("salt_sagatec").unwrap();

        assert_eq!(selector.profile(&sandbox()).unwrap().as_str(), "sandbox");
        assert_eq!(selector.dump(&source, &[]).unwrap(), PushDumpChoice::Fresh);
        assert_eq!(selector.database(&source).unwrap().as_str(), "salt_sagatec_qa");
        assert!(selector.confirm(&plan()).unwrap());
    }

    #[test]
    fn the_plan_names_source_destination_and_the_overwrite_scope() {
        let output = render_plan(&OutputStyle::plain(), &plan());

        assert!(output.contains("Source:       prod-source / salt_sagatec"));
        assert!(output.contains("Destination:  sandbox (sandbox.db.internal:3306) / salt_sagatec_qa"));
        assert!(output.contains("MySQL:        8.0.45 -> 8.4.4"));
        assert!(output.contains("Tables present in the dump will be replaced"));
        assert!(!output.to_ascii_lowercase().contains("password"));
    }
}
```

`format_duration` here is `crate::cli::cache::format_duration(seconds: u64)` (the same one `src/cli/restore.rs` imports). If `CliDumpProgress::new` takes `OutputStyle` by value and `OutputStyle` is not `Copy`, clone it.

Run: `cargo test --lib cli::push` → Expected: 5 PASS.

- [ ] **Step 5: Map exit codes with failing tests**

In `src/error.rs`:
- add `PushServiceError, PushSelectionError, RemoteTargetGateError` to the `application::{…}` import and `RemoteFailureKind, RemoteImportError` to the `infrastructure::mysql::{…}` import;
- add the variant `#[error(transparent)] Push(#[from] PushServiceError),` after `Pull`;
- add `Self::Push(error) => push_error_category(error),` after `Self::Pull(error) => …`;
- add:

```rust
const fn push_error_category(error: &PushServiceError) -> ErrorCategory {
    match error {
        PushServiceError::NoEligibleProfile => ErrorCategory::Configuration,
        PushServiceError::Selection(PushSelectionError::Unavailable(_))
        | PushServiceError::Selection(PushSelectionError::UnknownProfile(_))
        | PushServiceError::Selection(PushSelectionError::UnknownDump(_)) => ErrorCategory::Usage,
        PushServiceError::Target(error) => remote_target_error_category(error),
        PushServiceError::DumpChoices(error) => restore_error_category(error),
        PushServiceError::Dump(error) => dump_error_category(error),
        PushServiceError::Artifact(_) | PushServiceError::Lock(_) => ErrorCategory::Cache,
        PushServiceError::Import(RemoteImportError::Client(error)) => {
            docker_client_error_category(error)
        }
        PushServiceError::Import(RemoteImportError::Stream(error)) => {
            restore_executor_error_category(error)
        }
        PushServiceError::Import(
            RemoteImportError::EnsureFailed { kind, .. } | RemoteImportError::ImportFailed { kind, .. },
        ) => remote_failure_category(*kind),
        PushServiceError::ImportedSizeMismatch => ErrorCategory::Restore,
    }
}

const fn remote_target_error_category(error: &RemoteTargetGateError) -> ErrorCategory {
    match error {
        RemoteTargetGateError::Config(_)
        | RemoteTargetGateError::ProfileNotFound
        | RemoteTargetGateError::ProductionDestination
        | RemoteTargetGateError::DockerContextMissing
        | RemoteTargetGateError::SourceCollision => ErrorCategory::Configuration,
        RemoteTargetGateError::Credential(_) => ErrorCategory::Credential,
        RemoteTargetGateError::ClientCatalog(_)
        | RemoteTargetGateError::UnsupportedVendor
        | RemoteTargetGateError::UnsupportedServerSeries => ErrorCategory::Dependency,
        RemoteTargetGateError::Probe(error) => docker_client_error_category(error),
        RemoteTargetGateError::TlsRequiredButNotNegotiated => ErrorCategory::SourceConnection,
        RemoteTargetGateError::VersionMismatch => ErrorCategory::Restore,
    }
}

const fn remote_failure_category(kind: RemoteFailureKind) -> ErrorCategory {
    match kind {
        RemoteFailureKind::Authentication => ErrorCategory::Credential,
        RemoteFailureKind::DestinationUnavailable => ErrorCategory::SourceConnection,
        RemoteFailureKind::DockerUnavailable => ErrorCategory::Docker,
        RemoteFailureKind::Permission | RemoteFailureKind::Sql | RemoteFailureKind::Unknown => {
            ErrorCategory::Restore
        }
    }
}
```

Add to `src/error.rs` tests:

```rust
    #[test]
    fn push_refusals_and_interruptions_map_to_stable_exit_codes() {
        use crate::infrastructure::mysql::RestoreExecutorError;

        let production = AppError::Push(PushServiceError::Target(
            RemoteTargetGateError::ProductionDestination,
        ));
        let collision =
            AppError::Push(PushServiceError::Target(RemoteTargetGateError::SourceCollision));
        let version =
            AppError::Push(PushServiceError::Target(RemoteTargetGateError::VersionMismatch));
        let interrupted = AppError::Push(PushServiceError::Import(RemoteImportError::Stream(
            RestoreExecutorError::Interrupted,
        )));
        let permission = AppError::Push(PushServiceError::Import(RemoteImportError::ImportFailed {
            exit_code: Some(1),
            kind: RemoteFailureKind::Permission,
            stderr_truncated: false,
        }));
        let no_terminal = AppError::Push(PushServiceError::Selection(
            PushSelectionError::Unavailable("rerun with --yes".to_owned()),
        ));

        assert_eq!(production.exit_code(), 10);
        assert_eq!(collision.exit_code(), 10);
        assert_eq!(version.exit_code(), 70);
        assert_eq!(interrupted.exit_code(), 130);
        assert_eq!(permission.exit_code(), 70);
        assert_eq!(no_terminal.exit_code(), 2);
    }
```

Run: `cargo test --lib push_refusals_and_interruptions` → Expected: PASS (after the mapping compiles).

- [ ] **Step 6: Dispatch in `src/lib.rs`**

Add after the `Commands::Pull(arguments) => { … }` arm:

```rust
        Commands::Push(arguments) => {
            let database = DatabaseName::try_from(arguments.database)?;
            let profile = arguments.profile.map(ProfileName::try_from).transpose()?;
            let dump_id = arguments
                .dump_id
                .map(|raw| raw.parse::<domain::DumpId>())
                .transpose()?;
            let target_database = arguments
                .target_database
                .map(DatabaseName::try_from)
                .transpose()?;
            let selector = cli::push::CliPushSelector::new(
                style,
                profile,
                dump_id,
                arguments.fresh,
                target_database,
                arguments.yes,
            );
            print!("{}", cli::push::render_start(&style));
            std::io::stdout().flush().map_err(AppError::Output)?;

            let repository = ConfigRepository::discover()?;
            let workflow = infrastructure::mysql::DockerDumpWorkflow::new(
                infrastructure::process::TokioProcessRunner,
            );
            let progress = std::sync::Arc::new(cli::push::CliPushProgress::new(style));
            let service = application::PushService::new(repository)
                .with_progress(progress.clone())
                .with_compression_progress(progress.clone());
            let dump_executor =
                infrastructure::mysql::DockerMysqlDumpExecutor::new(cancellation.clone());
            let result = service
                .push(
                    &credential_store,
                    &infrastructure::mysql::DockerRemoteTargetProbe::new(
                        infrastructure::process::TokioProcessRunner,
                    ),
                    application::PullDumpDependencies {
                        preflight: &workflow,
                        executor: &dump_executor,
                    },
                    infrastructure::mysql::DockerMysqlRemoteImportExecutor::new(
                        cancellation.clone(),
                    ),
                    &selector,
                    database,
                )
                .await;
            progress.finish();
            match result? {
                application::PushOutcome::Ready(ready) => {
                    print!("{}", cli::push::render_complete(&style, &ready));
                }
                application::PushOutcome::Cancelled => {
                    print!("{}", cli::push::render_cancelled(&style));
                }
            }
            Ok(())
        }
```

If `OutputStyle` is not `Copy`, pass `style.clone()` to the selector and progress (check how `Commands::Pull` passes `style` to `CliPullProgress::new(style)` — it passes by value and later uses `&style`, so it is `Copy`).

- [ ] **Step 7: Smoke the help output**

Run: `cargo run -- push --help`
Expected: shows `--profile`, `--dump-id`, `--fresh`, `--database`, `--yes`.

Run: `cargo run -- push salt_sagatec --profile x </dev/null`
Expected: non-zero exit with a clear message (no config → configuration error, or "not an eligible push destination"); no panic.

- [ ] **Step 8: Gate and commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
git add src/cli/push.rs src/cli/mod.rs src/cli/prompt.rs src/lib.rs src/error.rs
git commit -m "feat: add the push command"
```

---

### Task 7: Real push integration test and documentation

**Files:**
- Modify: `tests/pull_integration.rs` (new `#[ignore]` test reusing `start_mysql_container`, `wait_for_mysql`, `run_mysql_query`)
- Create: `docs/push-command.md`
- Modify: `README.md`, `docs/exit-codes.md`

**Interfaces:**
- Consumes: public API from Tasks 3–5 (`reprodb::application::{PushService, PushSelector, PushDumpChoice, PushOutcome, PushPlan, PushSelectionError, RemoteProfileChoice, RestoreDumpChoice, PullDumpDependencies}`, `reprodb::infrastructure::mysql::{DockerRemoteTargetProbe, DockerMysqlRemoteImportExecutor}`).

- [ ] **Step 1: Write the integration test**

Add to `tests/pull_integration.rs` imports: `PushDumpChoice, PushOutcome, PushPlan, PushSelectionError, PushSelector, PushService, RemoteProfileChoice, RestoreDumpChoice` under `application`, and `DockerMysqlRemoteImportExecutor, DockerRemoteTargetProbe` under `infrastructure::mysql`. Then add:

```rust
struct ScriptedPush {
    destination: ProfileName,
    dump: std::sync::Mutex<PushDumpChoice>,
}

impl PushSelector for ScriptedPush {
    fn profile(&self, choices: &[RemoteProfileChoice]) -> Result<ProfileName, PushSelectionError> {
        // `push-source` is non-production too, so both profiles are offered.
        assert!(choices.iter().any(|choice| choice.profile == self.destination));
        Ok(self.destination.clone())
    }

    fn dump(
        &self,
        _database: &DatabaseName,
        _choices: &[RestoreDumpChoice],
    ) -> Result<PushDumpChoice, PushSelectionError> {
        Ok(*self.dump.lock().unwrap())
    }

    fn database(&self, source_database: &DatabaseName) -> Result<DatabaseName, PushSelectionError> {
        Ok(source_database.clone())
    }

    fn confirm(&self, _plan: &PushPlan) -> Result<bool, PushSelectionError> {
        Ok(true)
    }
}

#[tokio::test]
#[ignore = "creates isolated MySQL source/destination containers and pushes between them"]
async fn pushes_a_fresh_then_a_cached_dump_into_another_profile_without_dropping_extra_tables() {
    let (context, _) = DockerTargetDiscovery::new(TokioProcessRunner)
        .discover()
        .await
        .unwrap();
    let suffix = Uuid::new_v4().simple().to_string();
    let source_name = format!("reprodb-push-source-{}", &suffix[..12]);
    let destination_name = format!("reprodb-push-dest-{}", &suffix[..12]);
    let source_password = SecretString::from("reprodb-push-source-only");
    let destination_password = SecretString::from("reprodb-push-destination-only");
    let client = ClientCatalog::resolve("8.4").unwrap();
    let _source_guard =
        start_mysql_container(&context, &source_name, client.image(), &source_password, true, false);
    let _destination_guard = start_mysql_container(
        &context,
        &destination_name,
        client.image(),
        &destination_password,
        true,
        false,
    );
    let (_, candidates) = DockerTargetDiscovery::new(TokioProcessRunner)
        .discover()
        .await
        .unwrap();
    let find = |name: &str| {
        candidates
            .iter()
            .find(|candidate| candidate.name.as_str() == name)
            .cloned()
            .expect("temporary MySQL container was not discovered")
    };
    let source = find(&source_name);
    let destination = find(&destination_name);
    let port = |candidate: &reprodb::infrastructure::docker::DockerContainerCandidate| {
        candidate
            .published_ports
            .first()
            .expect("MySQL port was not published")
            .host_port
    };
    wait_for_mysql(&context, source.id.as_str(), client.image(), &source_password).await;
    wait_for_mysql(
        &context,
        destination.id.as_str(),
        client.image(),
        &destination_password,
    )
    .await;

    let database = DatabaseName::try_from(format!("reprodb_push_{}", &suffix[..16])).unwrap();
    run_mysql_query(
        &context,
        source.id.as_str(),
        client.image(),
        &source_password,
        MysqlTlsMode::Required,
        None,
        &format!(
            "CREATE DATABASE `{database}` CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci; \
             CREATE TABLE `{database}`.`items` (`id` BIGINT PRIMARY KEY, `label` VARCHAR(64) NOT NULL); \
             INSERT INTO `{database}`.`items` VALUES (1, 'one'), (2, 'two');"
        ),
    )
    .expect("source fixture");

    let source_profile = ProfileName::try_from("push-source").unwrap();
    let destination_profile = ProfileName::try_from("push-sandbox").unwrap();
    let source_key = CredentialKey::new(CredentialScope::Source);
    let destination_key = CredentialKey::new(CredentialScope::Source);
    let profile = |port: u16, key: CredentialKey| SourceProfileConfig {
        host: "127.0.0.1".to_owned(),
        port,
        username: "root".to_owned(),
        credential_key: key,
        mysql_family: MysqlFamily::Mysql,
        mysql_series: "8.4".to_owned(),
        production: false,
        tls_mode: MysqlTlsMode::Required,
        tls_material: Default::default(),
        client: MysqlClientConfig {
            image: client.image().to_owned(),
        },
    };
    let temp = TempDir::new().unwrap();
    let repository = ConfigRepository::new(AppPaths::new(
        temp.path().join("config"),
        temp.path().join("cache"),
        temp.path().join("data"),
    ));
    repository
        .save(&AppConfig {
            active_profile: Some(source_profile.clone()),
            client_runtime: ClientRuntimeConfig {
                docker_context: Some(context.clone()),
                ..ClientRuntimeConfig::default()
            },
            profiles: std::collections::BTreeMap::from([
                (source_profile.clone(), profile(port(&source), source_key)),
                (
                    destination_profile.clone(),
                    profile(port(&destination), destination_key),
                ),
            ]),
            ..AppConfig::default()
        })
        .unwrap();
    let credentials = MemoryCredentialStore::default();
    credentials.set(&source_key, source_password.clone()).await.unwrap();
    credentials
        .set(&destination_key, destination_password.clone())
        .await
        .unwrap();

    let workflow = DockerDumpWorkflow::new(TokioProcessRunner);
    let dump_executor = DockerMysqlDumpExecutor::default();
    let probe = DockerRemoteTargetProbe::new(TokioProcessRunner);
    let selector = ScriptedPush {
        destination: destination_profile.clone(),
        dump: std::sync::Mutex::new(PushDumpChoice::Fresh),
    };
    let service = PushService::new(repository);

    // Both profiles are eligible destinations, so the selector picks the sandbox explicitly.
    let first = service
        .push(
            &credentials,
            &probe,
            PullDumpDependencies {
                preflight: &workflow,
                executor: &dump_executor,
            },
            DockerMysqlRemoteImportExecutor::default(),
            &selector,
            database.clone(),
        )
        .await
        .expect("fresh push");
    let PushOutcome::Ready(first) = first else {
        panic!("fresh push was cancelled");
    };

    run_mysql_query(
        &context,
        destination.id.as_str(),
        client.image(),
        &destination_password,
        MysqlTlsMode::Required,
        Some(&database),
        "UPDATE items SET label = 'changed' WHERE id = 1; CREATE TABLE extra (`id` INT);",
    )
    .expect("destination drift");

    *selector.dump.lock().unwrap() = PushDumpChoice::Existing(first.plan.dump_id);
    let second = service
        .push(
            &credentials,
            &probe,
            PullDumpDependencies {
                preflight: &workflow,
                executor: &dump_executor,
            },
            DockerMysqlRemoteImportExecutor::default(),
            &selector,
            database.clone(),
        )
        .await
        .expect("cached push");
    assert!(matches!(second, PushOutcome::Ready(_)));

    let rows = run_mysql_query(
        &context,
        destination.id.as_str(),
        client.image(),
        &destination_password,
        MysqlTlsMode::Required,
        Some(&database),
        "SELECT id, label FROM items ORDER BY id; SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = DATABASE() AND table_name = 'extra'",
    )
    .expect("destination verification");
    assert_eq!(String::from_utf8(rows).unwrap(), "1\tone\n2\ttwo\n1\n");
}
```

- [ ] **Step 2: Run it against Docker**

Run: `cargo test --test pull_integration pushes_a_fresh_then_a_cached_dump -- --ignored --nocapture --test-threads=1`
Expected: PASS. `items` restored to `one`/`two` by the second push; `extra` survives (count `1`).

- [ ] **Step 3: Write `docs/push-command.md`**

```markdown
# `reprodb push` — contrato operacional

`reprodb push DATABASE` importa um dump gerenciado em um database de **outro profile** — por exemplo, levar um dump de produção para um sandbox — sem passar pelo MySQL local.

```bash
reprodb push salt_sagatec                                   # pergunta profile, dump e nome
reprodb push salt_sagatec --profile sandbox --fresh --yes   # dump novo do profile ativo
reprodb push salt_sagatec --profile sandbox \
  --dump-id 550e8400-e29b-41d4-a716-446655440000 \
  --database salt_sagatec_qa --yes
```

## Fluxo

```text
profile destino (só não-produção)
    -> credencial + conexão com TLS do profile, lê versão e @@server_uuid
dump: cache existente ou "gerar dump novo agora" (profile ativo como origem)
    -> validar metadata, tamanho, Zstd e dois SHA-256
nome do database remoto (default: o da origem)
    -> recusar origem == destino (mesmo server_uuid e mesmo nome)
    -> recusar downgrade de versão
mostrar plano e pedir o nome do database digitado (--yes pula)
    -> CREATE DATABASE IF NOT EXISTS (charset/collation do dump)
    -> Zstd -> stdin do mysql (--binary-mode)
    -> conferir bytes importados
```

## Garantias

- Profile com `production = true` nunca é destino; não existe flag para forçar.
- Não há `DROP DATABASE`. O dump recria cada tabela que contém (`DROP TABLE IF EXISTS` + `CREATE TABLE`); tabelas que só existem no destino continuam lá.
- Mesmo servidor com outro nome de database é permitido; o próprio database de origem, não.
- Sem terminal, profile, dump e confirmação precisam vir por `--profile`, `--dump-id`/`--fresh` e `--yes`.
- O usuário do profile destino precisa de `CREATE`, `DROP`, `INSERT`, `ALTER`, `INDEX`, `REFERENCES` e `TRIGGER` no database (ou criar o database, se ele ainda não existir).

## Falhas

Falha ou `Ctrl+C` no meio do import deixa o database remoto parcialmente importado. Rodar o mesmo comando importa por cima de novo. Exit codes em [`exit-codes.md`](exit-codes.md).
```

- [ ] **Step 4: Update README and exit codes**

In `README.md`:
- in the commands table, after the `reprodb pull` row, add: ``| `reprodb push <database>` | Importa um dump em um database de outro profile (nunca produção) |``
- in the security list, replace `- a origem é lida apenas com `mysqldump`; nada é escrito nela;` with `- a origem é lida apenas com `mysqldump`; só o `push` escreve em host remoto, e nunca em profile de produção;`
- in the docs table "Fluxo principal" row, append `` · [`push-command.md`](docs/push-command.md)``.

In `docs/exit-codes.md`, add a line under the relevant categories stating that `push` uses `10` for production destination / source collision, `30` for an unreachable or TLS-failing destination, and `70` for version mismatch, missing privileges or import failures.

- [ ] **Step 5: Gate and commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
git add tests/pull_integration.rs docs/push-command.md README.md docs/exit-codes.md
git commit -m "docs: document push and prove it against real MySQL containers"
```
