use anyhow::{Context, Result, anyhow, bail};
use std::ffi::{OsStr, OsString};
use std::process::Command;
use std::time::Duration;

pub trait CommandRunner {
    fn output(&self, program: &str, args: &[&str]) -> Result<String>;
    fn status(&self, program: &str, args: &[&str]) -> Result<()>;

    fn output_with_timeout(
        &self,
        program: &str,
        args: &[&str],
        _timeout: Duration,
    ) -> Result<String> {
        self.output(program, args)
    }

    fn status_with_timeout(&self, program: &str, args: &[&str], _timeout: Duration) -> Result<()> {
        self.status(program, args)
    }
}

#[derive(Debug, Copy, Clone)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn output(&self, program: &str, args: &[&str]) -> Result<String> {
        let output = Command::new(program)
            .args(args.iter().map(OsStr::new))
            .output()
            .with_context(|| format!("failed to execute {} {}", program, args.join(" ")))?;

        if !output.status.success() {
            bail!(
                "{} {} failed: {}",
                program,
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        String::from_utf8(output.stdout).map_err(|e| anyhow!(e))
    }

    fn status(&self, program: &str, args: &[&str]) -> Result<()> {
        let status = Command::new(program)
            .args(args.iter().map(OsStr::new))
            .status()
            .with_context(|| format!("failed to execute {} {}", program, args.join(" ")))?;

        if status.success() {
            Ok(())
        } else {
            bail!("{} {} failed with {}", program, args.join(" "), status)
        }
    }

    fn output_with_timeout(
        &self,
        program: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<String> {
        let output = timeout_command(program, args, timeout)
            .with_context(|| format!("failed to execute {} {}", program, args.join(" ")))?;

        if timed_out(output.status.code()) {
            bail!(
                "{} {} timed out after {:?}",
                program,
                args.join(" "),
                timeout
            );
        }

        if !output.status.success() {
            bail!(
                "{} {} failed: {}",
                program,
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        String::from_utf8(output.stdout).map_err(|e| anyhow!(e))
    }

    fn status_with_timeout(&self, program: &str, args: &[&str], timeout: Duration) -> Result<()> {
        let output = timeout_command(program, args, timeout)
            .with_context(|| format!("failed to execute {} {}", program, args.join(" ")))?;

        if timed_out(output.status.code()) {
            bail!(
                "{} {} timed out after {:?}",
                program,
                args.join(" "),
                timeout
            );
        }

        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "{} {} failed with {}: {}",
                program,
                args.join(" "),
                output.status,
                stderr.trim()
            )
        }
    }
}

fn timeout_command(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> std::io::Result<std::process::Output> {
    let mut timeout_args = vec![
        OsString::from("--kill-after=1s"),
        OsString::from(timeout_arg(timeout)),
        OsString::from(program),
    ];
    timeout_args.extend(args.iter().map(OsString::from));

    Command::new("timeout").args(timeout_args).output()
}

fn timeout_arg(timeout: Duration) -> String {
    format!("{:.3}s", timeout.as_secs_f64().max(0.001))
}

fn timed_out(code: Option<i32>) -> bool {
    matches!(code, Some(124 | 137))
}

/// Whether a program can be found in `PATH`, used by the `doctor` report.
pub fn command_exists(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };

    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(program);
        candidate.is_file() && is_executable(&candidate)
    })
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}
