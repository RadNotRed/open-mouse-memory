use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use crate::error::{AppError, Result};

const RULE_PATH: &str = "/etc/udev/rules.d/70-open-mouse-memory.rules";
const RULE_CONTENT: &str = include_str!("../packaging/udev/70-open-mouse-memory.rules");
const NO_PROMPT_ENV: &str = "OPEN_MOUSE_MEMORY_NO_ACCESS_PROMPT";

pub fn prompt_allowed(disabled: bool, json: bool) -> bool {
    !disabled
        && !json
        && !environment_disables_prompt()
        && io::stdin().is_terminal()
        && io::stderr().is_terminal()
}

pub fn prompt_and_request(path: &str) -> Result<bool> {
    eprintln!("A supported Logitech device was detected at {path}, but this user cannot access it.");
    eprint!("Request administrator approval to install the Open Mouse Memory device-access rule? [y/N] ");
    io::stderr()
        .flush()
        .map_err(|error| AppError::Other(format!("failed to display access prompt: {error}")))?;

    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|error| AppError::Other(format!("failed to read access prompt response: {error}")))?;
    if !consent_granted(&answer) {
        return Ok(false);
    }

    let executable = std::env::current_exe().map_err(|error| {
        AppError::Other(format!(
            "cannot locate the running open-mouse-memory executable: {error}"
        ))
    })?;
    let status = Command::new("/usr/bin/pkexec")
        .arg(executable)
        .arg("__install-access-rule")
        .status()
        .map_err(|error| AppError::Other(format!("failed to start PolicyKit authorization: {error}")))?;
    if !status.success() {
        return Err(AppError::Other(format!(
            "device-access authorization was canceled or failed{}",
            status
                .code()
                .map(|code| format!(" (exit {code})"))
                .unwrap_or_default()
        )));
    }

    for _ in 0..20 {
        if OpenOptions::new().read(true).write(true).open(path).is_ok() {
            return Ok(true);
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(AppError::Other(format!(
        "the access rule was installed, but {path} is still inaccessible; reconnect the mouse or receiver and retry"
    )))
}

pub fn install_rule_as_root() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(AppError::Unsafe(
            "the internal access-rule installer must be launched through PolicyKit".to_owned(),
        ));
    }

    let target = Path::new(RULE_PATH);
    let temporary = temporary_rule_path();
    let install_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&temporary)
            .map_err(|error| AppError::Other(format!("cannot create {}: {error}", temporary.display())))?;
        file.write_all(RULE_CONTENT.as_bytes())
            .map_err(|error| AppError::Other(format!("cannot write {}: {error}", temporary.display())))?;
        file.sync_all()
            .map_err(|error| AppError::Other(format!("cannot sync {}: {error}", temporary.display())))?;
        fs::rename(&temporary, target)
            .map_err(|error| AppError::Other(format!("cannot install {RULE_PATH}: {error}")))?;

        run_udevadm(["control", "--reload-rules"])?;
        run_udevadm(["trigger", "--action=add", "--subsystem-match=hidraw"])?;
        Ok(())
    })();
    if install_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    install_result
}

fn temporary_rule_path() -> PathBuf {
    PathBuf::from(format!(
        "/etc/udev/rules.d/.70-open-mouse-memory.rules.{}.tmp",
        std::process::id()
    ))
}

fn run_udevadm<const N: usize>(arguments: [&str; N]) -> Result<()> {
    let status = Command::new("/usr/bin/udevadm")
        .args(arguments)
        .status()
        .map_err(|error| AppError::Other(format!("failed to run udevadm: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::Other(format!(
            "udevadm failed{}",
            status
                .code()
                .map(|code| format!(" with exit {code}"))
                .unwrap_or_default()
        )))
    }
}

fn environment_disables_prompt() -> bool {
    std::env::var(NO_PROMPT_ENV)
        .ok()
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

fn consent_granted(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_explicit_consent() {
        assert!(consent_granted("y\n"));
        assert!(consent_granted("YES"));
        assert!(!consent_granted(""));
        assert!(!consent_granted("no"));
    }

    #[test]
    fn unprivileged_process_cannot_invoke_internal_installer() {
        if unsafe { libc::geteuid() } != 0 {
            assert!(matches!(install_rule_as_root(), Err(AppError::Unsafe(_))));
        }
    }
}
