use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::{ExitCode, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use async_compression::{
    Level,
    tokio::{bufread::ZstdDecoder, write::ZstdEncoder},
};
use thiserror::Error;
use tokio::{
    fs::{self, File, OpenOptions},
    io::{self, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStderr, Command},
    task::JoinHandle,
};

const CLIENT_CONFIG_PATH: &str = "/run/secrets/reprodb.cnf";
const STDERR_LIMIT: usize = 64 * 1024;

#[derive(Debug, Error)]
enum SpikeError {
    #[error("usage: reprodb-streaming-spike <dump|restore> CLIENT_IMAGE OPTION_FILE DATABASE FILE")]
    Usage,

    #[error("invalid database name: {0}")]
    InvalidDatabase(String),

    #[error("output already exists: {0}")]
    OutputExists(PathBuf),

    #[error("docker {operation} failed with {status}: {stderr}")]
    Docker {
        operation: &'static str,
        status: std::process::ExitStatus,
        stderr: String,
    },

    #[error("{operation} stream failed: {error}; docker stderr: {stderr}")]
    Stream {
        operation: &'static str,
        error: io::Error,
        stderr: String,
    },

    #[error("operation interrupted")]
    Interrupted,

    #[error("stderr reader task failed: {0}")]
    StderrTask(#[from] tokio::task::JoinError),

    #[error(transparent)]
    Io(#[from] io::Error),
}

type Result<T> = std::result::Result<T, SpikeError>;

#[derive(Debug)]
enum Operation {
    Dump,
    Restore,
}

impl Operation {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "dump" => Ok(Self::Dump),
            "restore" => Ok(Self::Restore),
            _ => Err(SpikeError::Usage),
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Dump => "dump",
            Self::Restore => "restore",
        }
    }
}

struct Args {
    operation: Operation,
    client_image: String,
    option_file: PathBuf,
    database: String,
    file: PathBuf,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut args = env::args_os().skip(1);
        let operation = args.next().ok_or(SpikeError::Usage)?;
        let client_image = args.next().ok_or(SpikeError::Usage)?;
        let option_file = args.next().ok_or(SpikeError::Usage)?;
        let database = args.next().ok_or(SpikeError::Usage)?;
        let file = args.next().ok_or(SpikeError::Usage)?;

        if args.next().is_some() {
            return Err(SpikeError::Usage);
        }

        let operation = Operation::parse(&operation.to_string_lossy())?;
        let client_image = client_image.to_string_lossy().into_owned();
        if client_image.is_empty() {
            return Err(SpikeError::Usage);
        }

        let database = database.to_string_lossy().into_owned();
        validate_database_name(&database)?;

        Ok(Self {
            operation,
            client_image,
            option_file: PathBuf::from(option_file),
            database,
            file: PathBuf::from(file),
        })
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(SpikeError::Interrupted) => {
            eprintln!("operation interrupted");
            ExitCode::from(130)
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let args = Args::parse()?;
    let option_file = fs::canonicalize(&args.option_file).await?;

    match args.operation {
        Operation::Dump => dump(&args.client_image, &option_file, &args.database, &args.file).await,
        Operation::Restore => {
            restore(&args.client_image, &option_file, &args.database, &args.file).await
        }
    }
}

async fn dump(client_image: &str, option_file: &Path, database: &str, output: &Path) -> Result<()> {
    if fs::try_exists(output).await? {
        return Err(SpikeError::OutputExists(output.to_path_buf()));
    }

    let partial = partial_path(output);
    if fs::try_exists(&partial).await? {
        return Err(SpikeError::OutputExists(partial));
    }

    let result = dump_to_partial(client_image, option_file, database, &partial).await;
    if result.is_err() {
        remove_if_present(&partial).await;
        return result;
    }

    fs::rename(&partial, output).await?;
    Ok(())
}

async fn dump_to_partial(
    client_image: &str,
    option_file: &Path,
    database: &str,
    partial: &Path,
) -> Result<()> {
    let container_name = unique_container_name(Operation::Dump);
    let mut child = spawn_client(
        &container_name,
        client_image,
        option_file,
        "mysqldump",
        &[
            format!("--defaults-file={CLIENT_CONFIG_PATH}"),
            "--single-transaction".into(),
            "--quick".into(),
            "--no-tablespaces".into(),
            "--hex-blob".into(),
            "--set-gtid-purged=OFF".into(),
            "--triggers".into(),
            "--skip-lock-tables".into(),
            database.into(),
        ],
        true,
        false,
    )?;

    let mut stdout = child.stdout.take().expect("stdout is piped");
    let stderr = spawn_stderr_reader(child.stderr.take().expect("stderr is piped"));

    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(partial)
        .await?;
    let mut encoder = ZstdEncoder::with_quality(output, Level::Precise(1));

    let copy_result = tokio::select! {
        copy = io::copy(&mut stdout, &mut encoder) => copy,
        signal = tokio::signal::ctrl_c() => {
            signal?;
            terminate_container(&container_name, &mut child).await;
            let _ = stderr.await??;
            return Err(SpikeError::Interrupted);
        }
    };

    let bytes = match copy_result {
        Ok(bytes) => bytes,
        Err(error) => {
            terminate_container(&container_name, &mut child).await;
            let stderr = stderr.await??;
            return Err(SpikeError::Stream {
                operation: "dump",
                error,
                stderr,
            });
        }
    };

    if let Err(error) = encoder.shutdown().await {
        terminate_container(&container_name, &mut child).await;
        let stderr = stderr.await??;
        return Err(SpikeError::Stream {
            operation: "compression",
            error,
            stderr,
        });
    }

    let status = child.wait().await?;
    let stderr = stderr.await??;

    if !status.success() {
        return Err(SpikeError::Docker {
            operation: "dump",
            status,
            stderr,
        });
    }

    eprintln!("dump stream completed: {bytes} uncompressed bytes");
    Ok(())
}

async fn restore(
    client_image: &str,
    option_file: &Path,
    database: &str,
    input: &Path,
) -> Result<()> {
    let container_name = unique_container_name(Operation::Restore);
    let mut child = spawn_client(
        &container_name,
        client_image,
        option_file,
        "mysql",
        &[
            format!("--defaults-file={CLIENT_CONFIG_PATH}"),
            "--binary-mode".into(),
            database.into(),
        ],
        false,
        true,
    )?;

    let stderr = spawn_stderr_reader(child.stderr.take().expect("stderr is piped"));
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let input = BufReader::new(File::open(input).await?);
    let mut decoder = ZstdDecoder::new(input);

    let copy_result = tokio::select! {
        copy = io::copy(&mut decoder, &mut stdin) => copy,
        signal = tokio::signal::ctrl_c() => {
            signal?;
            terminate_container(&container_name, &mut child).await;
            let _ = stderr.await??;
            return Err(SpikeError::Interrupted);
        }
    };

    let bytes = match copy_result {
        Ok(bytes) => bytes,
        Err(error) => {
            terminate_container(&container_name, &mut child).await;
            let stderr = stderr.await??;
            return Err(SpikeError::Stream {
                operation: "restore",
                error,
                stderr,
            });
        }
    };

    if let Err(error) = stdin.shutdown().await {
        terminate_container(&container_name, &mut child).await;
        let stderr = stderr.await??;
        return Err(SpikeError::Stream {
            operation: "restore stdin shutdown",
            error,
            stderr,
        });
    }
    drop(stdin);

    let status = child.wait().await?;
    let stderr = stderr.await??;
    if !status.success() {
        return Err(SpikeError::Docker {
            operation: "restore",
            status,
            stderr,
        });
    }

    eprintln!("restore stream completed: {bytes} uncompressed bytes");
    Ok(())
}

fn spawn_client(
    container_name: &str,
    client_image: &str,
    option_file: &Path,
    program: &str,
    arguments: &[String],
    pipe_stdout: bool,
    pipe_stdin: bool,
) -> Result<Child> {
    let mount = format!(
        "type=bind,src={},dst={CLIENT_CONFIG_PATH},readonly",
        option_file.display()
    );

    let mut command = Command::new("docker");
    command.arg("run").arg("--rm");

    if pipe_stdin {
        command.arg("-i");
    }

    command
        .arg("--name")
        .arg(container_name)
        .arg("--add-host=host.docker.internal:host-gateway")
        .arg("--mount")
        .arg(mount)
        .arg(client_image)
        .arg(program)
        .args(arguments)
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    if pipe_stdout {
        command.stdout(Stdio::piped());
    } else {
        command.stdout(Stdio::null());
    }

    if pipe_stdin {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }

    Ok(command.spawn()?)
}

fn spawn_stderr_reader(stderr: ChildStderr) -> JoinHandle<io::Result<String>> {
    tokio::spawn(read_stderr_capped(stderr))
}

async fn read_stderr_capped(mut stderr: ChildStderr) -> io::Result<String> {
    let mut result = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut truncated = false;

    loop {
        let read = stderr.read(&mut buffer).await?;
        if read == 0 {
            break;
        }

        let remaining = STDERR_LIMIT.saturating_sub(result.len());
        if remaining > 0 {
            result.extend_from_slice(&buffer[..read.min(remaining)]);
        }
        if read > remaining {
            truncated = true;
        }
    }

    let mut result = String::from_utf8_lossy(&result).into_owned();
    if truncated {
        result.push_str("\n[stderr truncated]");
    }
    Ok(result)
}

async fn terminate_container(container_name: &str, child: &mut Child) {
    let _ = Command::new("docker")
        .arg("kill")
        .arg(container_name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn remove_if_present(path: &Path) {
    match fs::remove_file(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => eprintln!("failed to remove partial {}: {error}", path.display()),
    }
}

fn partial_path(output: &Path) -> PathBuf {
    let mut value: OsString = output.as_os_str().to_owned();
    value.push(".part");
    PathBuf::from(value)
}

fn unique_container_name(operation: Operation) -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "reprodb-spike-{}-{}-{nonce}",
        std::process::id(),
        operation.label()
    )
}

fn validate_database_name(value: &str) -> Result<()> {
    const BLOCKED: [&str; 4] = ["mysql", "information_schema", "performance_schema", "sys"];

    let valid = !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));

    if !valid
        || BLOCKED
            .iter()
            .any(|blocked| value.eq_ignore_ascii_case(blocked))
    {
        return Err(SpikeError::InvalidDatabase(value.to_owned()));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_safe_database_names() {
        assert!(validate_database_name("salt_sagatec").is_ok());
        assert!(validate_database_name("tenant-1").is_ok());
    }

    #[test]
    fn rejects_unsafe_and_administrative_database_names() {
        for database in [
            "",
            "../../mysql",
            "tenant;DROP DATABASE mysql",
            "mysql",
            "INFORMATION_SCHEMA",
            &"a".repeat(65),
        ] {
            assert!(validate_database_name(database).is_err(), "{database}");
        }
    }

    #[test]
    fn appends_part_without_replacing_extensions() {
        assert_eq!(
            partial_path(Path::new("dump.sql.zst")),
            PathBuf::from("dump.sql.zst.part")
        );
    }
}
