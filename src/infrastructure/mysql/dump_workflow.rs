use async_trait::async_trait;

use crate::{
    application::{DumpPreflightGateway, DumpSource, DumpTenantResolver},
    domain::{
        DatabaseName, PatternTenantResolver, ResolvedTenant, TenantLookup, TenantResolutionError,
        TenantResolver,
    },
    infrastructure::{
        config::TenantResolverConfig,
        mysql::{
            ApprovedMysqlDump, DockerMysqlDumpPreflight, DockerSaltCentralTenantResolver,
            DumpPreflightError, DumpPreflightSource, SaltCentralSource,
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
impl<R> DumpTenantResolver for DockerDumpWorkflow<R>
where
    R: ProcessRunner + Clone,
{
    async fn resolve(
        &self,
        source: &DumpSource<'_>,
        lookup: &TenantLookup,
    ) -> Result<ResolvedTenant, TenantResolutionError> {
        match &source.profile.tenant_resolver {
            TenantResolverConfig::Pattern { pattern } => {
                PatternTenantResolver::new(pattern)?.resolve(lookup).await
            }
            TenantResolverConfig::SaltCentral {
                central_database,
                allow_domain_lookup,
            } => {
                DockerSaltCentralTenantResolver::new(
                    self.runner.clone(),
                    SaltCentralSource {
                        docker_context: source.docker_context.to_owned(),
                        host: source.profile.host.clone(),
                        port: source.profile.port,
                        username: source.profile.username.clone(),
                        password: source.password.clone(),
                        tls_mode: source.profile.tls_mode,
                        central_database: central_database.clone(),
                        client: source.client,
                    },
                    *allow_domain_lookup,
                )
                .resolve(lookup)
                .await
            }
        }
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
                client: source.client,
            },
        )
        .assess(database)
        .await
    }
}
