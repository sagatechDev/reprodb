use std::{env, io::Read, path::PathBuf, process::ExitCode};

use reprodb_streaming_spike::write_client_option_file;
use secrecy::SecretString;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args_os().skip(1);
    let path = PathBuf::from(arguments.next().ok_or("missing option file path")?);
    let host = arguments.next().ok_or("missing host")?;
    let port = arguments
        .next()
        .ok_or("missing port")?
        .to_string_lossy()
        .parse::<u16>()?;
    let username = arguments.next().ok_or("missing username")?;
    if arguments.next().is_some() {
        return Err("unexpected argument".into());
    }

    let mut password = String::new();
    std::io::stdin().read_to_string(&mut password)?;
    let password = SecretString::from(password);

    write_client_option_file(
        &path,
        &host.to_string_lossy(),
        port,
        &username.to_string_lossy(),
        &password,
    )?;
    Ok(())
}
