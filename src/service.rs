use crate::config::{Paths, atomic_write};
use anyhow::{Result, ensure};
use std::{fs, path::PathBuf, process::Command};

#[cfg(target_os = "macos")]
pub use launchd::{install, uninstall};
#[cfg(not(target_os = "macos"))]
pub use systemd::{install, uninstall};

#[cfg(not(target_os = "macos"))]
mod systemd {
    use super::*;

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
    fn wsl() -> bool {
        std::env::var_os("WSL_DISTRO_NAME").is_some()
            || std::path::Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists()
    }
    fn systemctl(args: &[&str]) -> Result<()> {
        let hint = if wsl() {
            "; on WSL, set [boot] systemd=true in /etc/wsl.conf and run wsl.exe --shutdown"
        } else {
            ""
        };
        let status = Command::new("systemctl").args(args).status();
        ensure!(
            status.is_ok_and(|s| s.success()),
            "systemctl failed; inspect the user service{hint}"
        );
        Ok(())
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
        systemctl(&["--user", "daemon-reload"])?;
        systemctl(&["--user", "enable", "--now", "runnerctl.service"])?;
        println!(
            "Installed runnerctl.service. For operation after logout: sudo loginctl enable-linger $USER"
        );
        Ok(())
    }
    pub fn uninstall() -> Result<()> {
        systemctl(&["--user", "disable", "--now", "runnerctl.service"])?;
        // Leave the unit and state intact for inspection; running jobs are preserved.
        println!(
            "Service disabled. Existing runner containers remain; use stop before uninstall to drain pools."
        );
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod launchd {
    use super::*;

    const LABEL: &str = "io.runnerctl.manager";

    fn escape(value: &str) -> Result<String> {
        ensure!(
            !value.chars().any(char::is_control),
            "invalid launchd value"
        );
        Ok(value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;"))
    }
    fn domain() -> String {
        format!("gui/{}", unsafe { libc::getuid() })
    }
    fn launchctl(args: &[&str]) -> bool {
        Command::new("launchctl")
            .args(args)
            .status()
            .is_ok_and(|s| s.success())
    }
    // Stopping the manager leaves containers running. An agent that is not loaded is fine.
    fn bootout(service: &str) {
        let _ = Command::new("launchctl")
            .args(["bootout", service])
            .stderr(std::process::Stdio::null())
            .status();
    }
    pub fn install(paths: &Paths) -> Result<()> {
        let user_home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let dir = user_home.join("Library/LaunchAgents");
        let exe = std::env::current_exe()?;
        // macOS privacy protection denies these folders to agents without Full Disk Access.
        for path in [&exe, &paths.home] {
            ensure!(
                ![
                    "Desktop",
                    "Documents",
                    "Downloads",
                    "Library/Mobile Documents"
                ]
                .iter()
                .any(|folder| path.starts_with(user_home.join(folder))),
                "LaunchAgents cannot access {}; install the binary and --home outside Desktop, Documents, Downloads and iCloud Drive",
                path.display()
            );
        }
        fs::create_dir_all(&dir)?;
        // launchd starts agents with a minimal PATH; keep the one that finds this docker CLI.
        let mut environment = String::new();
        for name in ["PATH", "DOCKER_HOST", "DOCKER_CONTEXT", "DOCKER_CONFIG"] {
            if let Some(value) = std::env::var_os(name) {
                environment.push_str(&format!(
                    "\t\t<key>{name}</key>\n\t\t<string>{}</string>\n",
                    escape(&value.to_string_lossy())?
                ));
            }
        }
        let log = escape(&paths.home.join("manager.log").to_string_lossy())?;
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n\t<key>Label</key>\n\t<string>{LABEL}</string>\n\t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{}</string>\n\t\t<string>--home</string>\n\t\t<string>{}</string>\n\t\t<string>manager</string>\n\t</array>\n\t<key>EnvironmentVariables</key>\n\t<dict>\n{environment}\t</dict>\n\t<key>RunAtLoad</key>\n\t<true/>\n\t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>\n\t<key>ThrottleInterval</key>\n\t<integer>10</integer>\n\t<key>Umask</key>\n\t<integer>63</integer>\n\t<key>ExitTimeOut</key>\n\t<integer>300</integer>\n\t<key>StandardOutPath</key>\n\t<string>{log}</string>\n\t<key>StandardErrorPath</key>\n\t<string>{log}</string>\n</dict>\n</plist>\n",
            escape(&exe.to_string_lossy())?,
            escape(&paths.home.to_string_lossy())?,
        );
        let plist = dir.join(format!("{LABEL}.plist"));
        atomic_write(&plist, body.as_bytes())?;
        let domain = domain();
        let service = format!("{domain}/{LABEL}");
        // Reload a previously installed agent with the new definition.
        bootout(&service);
        ensure!(
            launchctl(&["enable", &service])
                && launchctl(&["bootstrap", &domain, &plist.to_string_lossy()]),
            "launchctl failed; inspect launchctl print {service}"
        );
        println!(
            "Installed {LABEL} as a LaunchAgent. It runs while you are logged in, like Docker Desktop and OrbStack. Logs: {}",
            paths.home.join("manager.log").display()
        );
        Ok(())
    }
    pub fn uninstall() -> Result<()> {
        let service = format!("{}/{LABEL}", domain());
        bootout(&service);
        // Keep the plist for inspection but do not load it at the next login.
        ensure!(launchctl(&["disable", &service]), "launchctl failed");
        println!(
            "Service disabled. Existing runner containers remain; use stop before uninstall to drain pools."
        );
        Ok(())
    }
}
