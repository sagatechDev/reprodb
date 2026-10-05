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
            push_destination: true,
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
