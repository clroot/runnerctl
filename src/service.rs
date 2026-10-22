use crate::config::{Paths, atomic_write};
use anyhow::{Result, ensure};
#[cfg(unix)]
use std::path::PathBuf;
use std::{fs, process::Command};

#[cfg(target_os = "macos")]
pub use launchd::{install, uninstall};
#[cfg(not(any(target_os = "macos", windows)))]
pub use systemd::{install, uninstall};
#[cfg(windows)]
pub use task::{install, run, uninstall};

#[cfg(not(any(target_os = "macos", windows)))]
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

#[cfg(windows)]
mod task {
    use super::*;
    use std::{process::Stdio, time::Duration};

    const NAME: &str = "runnerctl-manager";

    // Task Scheduler expands %VARIABLES% in arguments, and paths cannot contain quotes.
    fn quote(value: &str) -> Result<String> {
        ensure!(
            !value.contains(['"', '%', '\0', '\n', '\r']),
            "invalid Task Scheduler path: {value}"
        );
        // Backslashes before the closing quote would escape it.
        let trailing = value.len() - value.trim_end_matches('\\').len();
        Ok(format!("\"{value}{}\"", "\\".repeat(trailing)))
    }
    fn escape(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
    fn schtasks(args: &[&str]) -> bool {
        Command::new("schtasks")
            .args(args)
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
    // Ending a task that is not running is fine.
    fn end() {
        let _ = Command::new("schtasks")
            .args(["/end", "/tn", NAME])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    pub fn install(paths: &Paths) -> Result<()> {
        let exe = std::env::current_exe()?;
        let user = crate::windows::user_sid()?;
        // A console program started at logon opens a window; headless conhost hides it.
        let arguments = format!(
            "--headless {} --home {} service run",
            quote(&exe.to_string_lossy())?,
            quote(&paths.home.to_string_lossy())?
        );
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n  <RegistrationInfo>\n    <Description>GitHub Actions runner pool manager</Description>\n  </RegistrationInfo>\n  <Triggers>\n    <LogonTrigger>\n      <Enabled>true</Enabled>\n      <UserId>{user}</UserId>\n    </LogonTrigger>\n  </Triggers>\n  <Principals>\n    <Principal id=\"Author\">\n      <UserId>{user}</UserId>\n      <LogonType>InteractiveToken</LogonType>\n      <RunLevel>LeastPrivilege</RunLevel>\n    </Principal>\n  </Principals>\n  <Settings>\n    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\n    <RestartOnFailure>\n      <Interval>PT1M</Interval>\n      <Count>999</Count>\n    </RestartOnFailure>\n    <Priority>5</Priority>\n  </Settings>\n  <Actions Context=\"Author\">\n    <Exec>\n      <Command>%SystemRoot%\\System32\\conhost.exe</Command>\n      <Arguments>{}</Arguments>\n    </Exec>\n  </Actions>\n</Task>\n",
            escape(&arguments)
        );
        // schtasks reads task XML as UTF-16 with a byte order mark.
        let bytes: Vec<u8> = std::iter::once(0xfeff)
            .chain(body.encode_utf16())
            .flat_map(u16::to_le_bytes)
            .collect();
        let definition = paths.home.join("service.xml");
        atomic_write(&definition, &bytes)?;
        // Replace a running manager so the new definition takes effect, and give it a
        // moment to exit; a manager that still holds the lock is retried after 10 seconds.
        end();
        for _ in 0..50 {
            if crate::manager::lock(paths).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        ensure!(
            schtasks(&[
                "/create",
                "/tn",
                NAME,
                "/xml",
                &definition.to_string_lossy(),
                "/f"
            ]) && schtasks(&["/run", "/tn", NAME]),
            "schtasks failed; inspect the {NAME} task in Task Scheduler"
        );
        println!(
            "Installed the {NAME} scheduled task. It runs while you are signed in, like Docker Desktop. Logs: {}",
            paths.home.join("manager.log").display()
        );
        Ok(())
    }
    pub fn uninstall() -> Result<()> {
        end();
        // Keep the task for inspection but do not start it at the next sign-in.
        ensure!(
            schtasks(&["/change", "/tn", NAME, "/disable"]),
            "schtasks failed; inspect the {NAME} task in Task Scheduler"
        );
        println!(
            "Service disabled. Existing runner containers remain; use stop before uninstall to drain pools."
        );
        Ok(())
    }
    /// The task's entry point: the manager with output in manager.log, restarted on failure.
    pub async fn run(paths: Paths) -> Result<()> {
        paths.prepare()?;
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(paths.home.join("manager.log"))?;
        crate::windows::redirect_output(&log)?;
        let manager = async {
            loop {
                match crate::manager::serve(paths.clone()).await {
                    Ok(()) => return,
                    Err(error) => eprintln!("error: {error:#}"),
                }
                // Like systemd Restart=on-failure with RestartSec=10.
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        };
        // Ending the task stops only conhost, so leave with it. Runner state is
        // persisted, and the next manager reconciles containers as after a crash.
        // A detached thread, unlike spawn_blocking, does not delay exit while it waits.
        let (exited, parent) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || exited.send(crate::windows::wait_for_parent()));
        tokio::select! {
            _ = manager => {}
            result = parent => {
                if let Ok(Err(error)) = result {
                    eprintln!("error: {error:#}");
                }
                let _ = fs::remove_file(paths.socket());
            }
        }
        Ok(())
    }
}
