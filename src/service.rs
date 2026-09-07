use crate::config::{Paths, atomic_write};
use anyhow::{Result, ensure};
use std::{fs, path::PathBuf, process::Command};

fn quote(value: &str) -> Result<String> {
    ensure!(!value.contains(['\n', '\r', '\0']), "invalid systemd path");
    Ok(format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    ))
}
pub fn install(paths: &Paths) -> Result<()> {
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    let dir = root.join("systemd/user");
    fs::create_dir_all(&dir)?;
    let exe = std::env::current_exe()?;
    let body = format!(
        "[Unit]\nDescription=GitHub Actions runner pool manager\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart={} --home {} manager\nRestart=on-failure\nRestartSec=10\nUMask=0077\nTimeoutStopSec=300\n\n[Install]\nWantedBy=default.target\n",
        quote(&exe.to_string_lossy())?,
        quote(&paths.home.to_string_lossy())?
    );
    atomic_write(&dir.join("runnerctl.service"), body.as_bytes())?;
    for args in [
        vec!["--user", "daemon-reload"],
        vec!["--user", "enable", "--now", "runnerctl.service"],
    ] {
        ensure!(
            Command::new("systemctl").args(args).status()?.success(),
            "systemctl failed; inspect the user service"
        );
    }
    println!(
        "Installed runnerctl.service. For operation after logout: sudo loginctl enable-linger $USER"
    );
    Ok(())
}
pub fn uninstall() -> Result<()> {
    ensure!(
        Command::new("systemctl")
            .args(["--user", "disable", "--now", "runnerctl.service"])
            .status()?
            .success(),
        "systemctl failed"
    );
    // Leave the unit and state intact for inspection; running jobs are preserved.
    println!(
        "Service disabled. Existing runner containers remain; use stop before uninstall to drain pools."
    );
    Ok(())
}
