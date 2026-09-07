use std::{ffi::OsString, io, process::Stdio};

use async_trait::async_trait;
use thiserror::Error;
use tokio::process::{Child, Command};

pub async fn terminate_and_wait(child: &mut Child) -> io::Result<std::process::ExitStatus> {
    if let Err(kill_error) = child.start_kill() {
        return match child.try_wait() {
            Ok(Some(status)) => Ok(status),
            _ => Err(kill_error),
        };
    }
    child.wait().await
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessSpec {
    program: OsString,
    arguments: Vec<OsString>,
}

impl ProcessSpec {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
        }
    }

    pub fn arg(mut self, argument: impl Into<OsString>) -> Self {
        self.arguments.push(argument.into());
        self
    }

    pub fn args(mut self, arguments: impl IntoIterator<Item = impl Into<OsString>>) -> Self {
        self.arguments.extend(arguments.into_iter().map(Into::into));
        self
    }

    pub fn program(&self) -> &OsString {
        &self.program
    }

    pub fn arguments(&self) -> &[OsString] {
        &self.arguments
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessOutput {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl ProcessOutput {
    pub fn success(stdout: impl Into<Vec<u8>>) -> Self {
        Self {
            success: true,
            exit_code: Some(0),
            stdout: stdout.into(),
            stderr: Vec::new(),
        }
    }

    pub fn failure(exit_code: i32, stderr: impl Into<Vec<u8>>) -> Self {
        Self {
            success: false,
            exit_code: Some(exit_code),
            stdout: Vec::new(),
            stderr: stderr.into(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("could not start or wait for an external process")]
    Io {
        #[source]
        source: io::Error,
    },
}

#[async_trait]
pub trait ProcessRunner: Send + Sync {
    async fn output(&self, spec: &ProcessSpec) -> Result<ProcessOutput, ProcessError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TokioProcessRunner;

#[async_trait]
impl ProcessRunner for TokioProcessRunner {
    async fn output(&self, spec: &ProcessSpec) -> Result<ProcessOutput, ProcessError> {
        let output = Command::new(spec.program())
            .args(spec.arguments())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|source| ProcessError::Io { source })?;

        Ok(ProcessOutput {
            success: output.status.success(),
            exit_code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_spec_keeps_arguments_structured() {
        let spec = ProcessSpec::new("docker")
            .arg("--context")
            .arg("desktop-linux; echo unsafe")
            .args(["image", "inspect"]);

        assert_eq!(spec.program(), "docker");
        assert_eq!(spec.arguments().len(), 4);
        assert_eq!(spec.arguments()[1], "desktop-linux; echo unsafe");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn termination_kills_and_reaps_the_owned_child() {
        let mut child = Command::new("sh")
            .args(["-c", "exec sleep 30"])
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let status = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            terminate_and_wait(&mut child),
        )
        .await
        .expect("termination must not wait for the original process duration")
        .unwrap();

        assert!(!status.success());
        assert!(child.try_wait().unwrap().is_some());
    }
}
