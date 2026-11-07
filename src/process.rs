//! Every child process is built and started here, so each one is logged at
//! debug.

use std::io::{ErrorKind, Write};
use std::process::{Child, Command, ExitStatus, Output, Stdio};

use crate::error::{Context, Error, Result};

pub(crate) fn command(program: &str) -> Command {
    Command::new(program)
}

/// Runs `command`, writing `stdin` if given, and returns its output
/// whatever the exit status.
pub(crate) fn output(
    command: &mut Command,
    stdin: Option<&[u8]>,
) -> Result<Output> {
    // argv never carries secrets (they go on stdin), so it's safe to show.
    log::debug!("run: {}", describe(command));
    let input = if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    let mut child = command
        .stdin(input)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| spawn_error(command, &error))?;
    let program = command.get_program().to_string_lossy();
    if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(bytes).context(format!("{program}: stdin"))?;
    }
    child.wait_with_output().context(program)
}

pub(crate) fn status(command: &mut Command) -> Result<ExitStatus> {
    log::debug!("run: {}", describe(command));
    command
        .status()
        .map_err(|error| spawn_error(command, &error))
}

pub(crate) fn spawn(command: &mut Command) -> Result<Child> {
    log::debug!("run: {}", describe(command));
    command
        .spawn()
        .map_err(|error| spawn_error(command, &error))
}

pub(crate) fn describe(command: &Command) -> String {
    let mut line = command.get_program().to_string_lossy().into_owned();
    for arg in command.get_args() {
        line.push(' ');
        line.push_str(&arg.to_string_lossy());
    }
    line
}

pub(crate) fn spawn_error(command: &Command, error: &std::io::Error) -> Error {
    let program = command.get_program().to_string_lossy();
    if matches!(error.kind(), ErrorKind::NotFound) {
        Error::new(format!("{program} not found on PATH"))
    } else {
        Error::new(format!("could not run {program}: {error}"))
    }
}

pub(crate) fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::spawn_error;
    use std::io::{Error, ErrorKind};
    use std::process::Command;

    #[test]
    fn a_missing_program_is_reported_as_not_on_path() {
        let error = Error::from(ErrorKind::NotFound);
        let message = spawn_error(&Command::new("nope"), &error);
        assert_eq!(message.message(), "nope not found on PATH");
    }

    #[test]
    fn any_other_spawn_failure_keeps_its_cause() {
        let error = Error::from(ErrorKind::PermissionDenied);
        let message = spawn_error(&Command::new("nope"), &error);
        assert!(
            message.message().starts_with("could not run nope: "),
            "{}",
            message.message()
        );
    }
}
