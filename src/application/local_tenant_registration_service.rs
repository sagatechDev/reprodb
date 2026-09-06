use async_trait::async_trait;
use thiserror::Error;

use crate::{
    application::{AuthorizedLocalTarget, RestoreCompleted},
    domain::LocalTenantRegistration,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalTenantRegistered;

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum LocalTenantWriteError {
    #[error("the local salt_central schema is incompatible with reprodb")]
    IncompatibleSchema,
    #[error("the selected local domain already belongs to another tenant")]
    DomainConflict,
    #[error("the existing local tenant has a database connection override")]
    ExistingConnectionOverride,
    #[error("the local salt_central rejected the target credential")]
    AuthenticationFailed,
    #[error("the local MySQL target became unavailable")]
    TargetUnavailable,
    #[error("the local tenant registration transaction failed")]
    TransactionFailed,
}

#[async_trait]
pub trait LocalTenantWriter: Send + Sync {
    async fn register(
        &self,
        target: &AuthorizedLocalTarget,
        registration: &LocalTenantRegistration,
    ) -> Result<(), LocalTenantWriteError>;
}

pub struct LocalTenantRegistrationService<W> {
    writer: W,
}

impl<W> LocalTenantRegistrationService<W>
where
    W: LocalTenantWriter,
{
    pub const fn new(writer: W) -> Self {
        Self { writer }
    }

    pub async fn register_after_restore(
        &self,
        target: &AuthorizedLocalTarget,
        restored: &RestoreCompleted,
        registration: &LocalTenantRegistration,
    ) -> Result<LocalTenantRegistered, LocalTenantRegistrationServiceError> {
        if target.database() != restored.database()
            || target.database() != registration.target_database()
            || restored.tenant_id() != registration.tenant_id()
        {
            return Err(LocalTenantRegistrationServiceError::RestoreIdentityMismatch);
        }
        self.writer.register(target, registration).await?;
        Ok(LocalTenantRegistered)
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum LocalTenantRegistrationServiceError {
    #[error("the restored tenant/database differs from the authorized local registration")]
    RestoreIdentityMismatch,
    #[error(transparent)]
    Write(#[from] LocalTenantWriteError),
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::domain::{DatabaseName, DomainAlias, LocalTenantFeatures, TenantId};

    use super::*;

    struct FakeWriter(AtomicUsize);

    #[async_trait]
    impl LocalTenantWriter for FakeWriter {
        async fn register(
            &self,
            _target: &AuthorizedLocalTarget,
            _registration: &LocalTenantRegistration,
        ) -> Result<(), LocalTenantWriteError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn registration(tenant: &str, database: &str) -> LocalTenantRegistration {
        LocalTenantRegistration::try_new(
            TenantId::try_from(tenant).unwrap(),
            DomainAlias::try_from("sagatec").unwrap(),
            DatabaseName::try_from(database).unwrap(),
            LocalTenantFeatures::default(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn requires_a_restore_capability_for_the_same_database() {
        let writer = FakeWriter(AtomicUsize::new(0));
        let service = LocalTenantRegistrationService::new(writer);
        let target =
            AuthorizedLocalTarget::for_test(DatabaseName::try_from("salt_sagatec").unwrap());
        let restored = RestoreCompleted::for_test(DatabaseName::try_from("salt_sagatec").unwrap());

        service
            .register_after_restore(
                &target,
                &restored,
                &registration("salt_sagatec", "salt_sagatec"),
            )
            .await
            .unwrap();
        assert_eq!(service.writer.0.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn mismatch_is_rejected_before_the_writer() {
        let writer = FakeWriter(AtomicUsize::new(0));
        let service = LocalTenantRegistrationService::new(writer);
        let target =
            AuthorizedLocalTarget::for_test(DatabaseName::try_from("salt_sagatec").unwrap());
        let restored = RestoreCompleted::for_test(DatabaseName::try_from("salt_sagatec").unwrap());

        for registration in [
            registration("salt_sagatec", "salt_polymer"),
            registration("salt_polymer", "salt_sagatec"),
        ] {
            assert_eq!(
                service
                    .register_after_restore(&target, &restored, &registration)
                    .await
                    .unwrap_err(),
                LocalTenantRegistrationServiceError::RestoreIdentityMismatch
            );
        }
        assert_eq!(service.writer.0.load(Ordering::SeqCst), 0);
    }
}
