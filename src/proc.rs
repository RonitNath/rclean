use crate::format::human_duration;
use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::Duration;
use wait_timeout::ChildExt;

// Runs a child with captured output but a hard deadline, so a process that
// blocks forever (e.g. cargo waiting on another process's build-directory
// lock) surfaces as a recoverable error instead of hanging the whole run.
pub fn run_with_timeout(
    mut command: Command,
    timeout: Duration,
    label: &str,
) -> Result<Output, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not run {label}: {error}"))?;

    // Drain both pipes on separate threads; otherwise a chatty child fills a
    // pipe buffer and deadlocks against our wait.
    let pipes = [
        child.stdout.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
        child.stderr.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
    ];
    let readers = pipes.map(|pipe| {
        thread::spawn(move || {
            let mut buffer = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buffer);
            }
            buffer
        })
    });

    let waited = child.wait_timeout(timeout);
    if !matches!(waited, Ok(Some(_))) {
        // Kill before joining the readers so the pipes close and they finish.
        let _ = child.kill();
        let _ = child.wait();
    }
    let [stdout, stderr] = readers.map(|reader| reader.join().unwrap_or_default());

    match waited {
        Ok(Some(status)) => Ok(Output {
            status,
            stdout,
            stderr,
        }),
        Ok(None) => Err(format!(
            "{label} timed out after {}; killed. Last diagnostic: {}",
            human_duration(timeout),
            command_diagnostic(&stderr)
        )),
        Err(error) => Err(format!("could not wait for {label}: {error}")),
    }
}

pub fn command_diagnostic(stderr: &[u8]) -> String {
    let message = String::from_utf8_lossy(stderr).trim().to_string();
    if message.is_empty() {
        "no diagnostic output".to_string()
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn sleep_command(seconds: u32) -> Command {
        #[cfg(windows)]
        {
            let mut command = Command::new("powershell");
            command.args(["-NoProfile", "-Command", &format!("Start-Sleep -Seconds {seconds}")]);
            command
        }
        #[cfg(not(windows))]
        {
            let mut command = Command::new("sleep");
            command.arg(seconds.to_string());
            command
        }
    }

    #[test]
    fn timeout_kills_hung_child() {
        let started = Instant::now();
        let result = run_with_timeout(sleep_command(60), Duration::from_secs(2), "sleep test");
        let error = result.expect_err("hung child should time out");
        assert!(error.contains("timed out"), "unexpected error: {error}");
        assert!(started.elapsed() < Duration::from_secs(30));
    }

    #[test]
    fn fast_child_returns_output() {
        let mut command = Command::new("cargo");
        command.arg("--version");
        let output = run_with_timeout(command, Duration::from_secs(60), "cargo --version")
            .expect("cargo --version should run");
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("cargo"));
    }
}
