use async_trait::async_trait;

use crate::{
    application::{DumpPreflightGateway, DumpSource},
    domain::DatabaseName,
    infrastructure::{
        mysql::{
            ApprovedMysqlDump, DockerMysqlDumpPreflight, DumpPreflightError, DumpPreflightSource,
        },
        process::ProcessRunner,
    },
};

#[derive(Clone, Debug)]
pub struct DockerDumpWorkflow<R> {
    runner: R,
}

impl<R> DockerDumpWorkflow<R> {
    pub fn new(runner: R) -> Self {
        Self { runner }
    }
}

#[async_trait]
impl<R> DumpPreflightGateway for DockerDumpWorkflow<R>
where
    R: ProcessRunner + Clone,
{
    async fn assess(
        &self,
        source: &DumpSource<'_>,
        database: &DatabaseName,
    ) -> Result<ApprovedMysqlDump, DumpPreflightError> {
        DockerMysqlDumpPreflight::new(
            self.runner.clone(),
            DumpPreflightSource {
                docker_context: source.docker_context.to_owned(),
                host: source.profile.host.clone(),
                port: source.profile.port,
                username: source.profile.username.clone(),
                password: source.password.clone(),
                tls_mode: source.profile.tls_mode,
                tls_material: source.profile.tls_material.clone(),
                client: source.client,
            },
        )
        .assess(database)
        .await
    }
}
