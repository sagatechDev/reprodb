use std::{io, process::Stdio, time::Duration};

use tokio::process::{Child, Command};

use crate::infrastructure::process::terminate_and_wait;

const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);

pub fn ephemeral_container_name(operation: &str) -> String {
    format!("reprodb-{operation}-{}", uuid::Uuid::new_v4().simple())
}

/// Stops the attached Docker CLI child, then removes its uniquely named
/// ephemeral container if the daemon kept it alive after the client died.
pub async fn terminate_ephemeral_run(
    child: &mut Child,
    docker_context: &str,
    container_name: &str,
) -> io::Result<std::process::ExitStatus> {
    let status = terminate_and_wait(child).await?;
    let cleanup = Command::new("docker")
        .args(["--context", docker_context, "rm", "--force", container_name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status();

    match tokio::time::timeout(CLEANUP_TIMEOUT, cleanup).await {
        Ok(Ok(cleanup_status)) if cleanup_status.success() => {}
        Ok(Ok(cleanup_status)) => tracing::warn!(
            exit_code = cleanup_status.code(),
            "could not confirm ephemeral Docker container removal"
        ),
        Ok(Err(error)) => tracing::warn!(
            %error,
            "could not start ephemeral Docker container cleanup"
        ),
        Err(_) => tracing::warn!("ephemeral Docker container cleanup timed out"),
    }

    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ephemeral_names_are_unique_and_contain_no_database_identity() {
        let first = ephemeral_container_name("dump");
        let second = ephemeral_container_name("dump");

        assert_ne!(first, second);
        assert!(first.starts_with("reprodb-dump-"));
        assert!(!first.contains("acme_production"));
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        );
    }
}
