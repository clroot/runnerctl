use crate::{config::Paths, state::Run};
use anyhow::{Context, Result, anyhow, ensure};
use serde_json::Value;
use std::{io::Write, path::Path, time::Duration};
use tokio::process::Command;

// The user and group that own runner workspaces on the host. Docker Desktop on Windows
// maps every container file to the Windows user, so root keeps the runner's defaults.
#[cfg(unix)]
fn host_ids() -> (u32, u32) {
    unsafe { (libc::geteuid(), libc::getegid()) }
}
#[cfg(windows)]
fn host_ids() -> (u32, u32) {
    (0, 0)
}

/// The path at which the Docker daemon sees a host path. Docker Desktop on Windows mounts
/// drive C: at /run/desktop/mnt/host/c in its VM; elsewhere host paths are used as is.
#[cfg(unix)]
pub fn daemon_path(path: &Path) -> Result<String> {
    Ok(path.display().to_string())
}
#[cfg(windows)]
pub fn daemon_path(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .context("Docker paths must be valid Unicode")?;
    let text = text.strip_prefix(r"\\?\").unwrap_or(text);
    let (drive, rest) = text
        .split_once(":\\")
        .filter(|(drive, _)| drive.len() == 1 && drive.chars().all(|c| c.is_ascii_alphabetic()))
        .with_context(|| {
            format!(
                "{} is not on a local drive; Docker socket pools need --home on a drive such as C:",
                path.display()
            )
        })?;
    Ok(format!(
        "/run/desktop/mnt/host/{}/{}",
        drive.to_ascii_lowercase(),
        rest.replace('\\', "/")
    ))
}

pub struct Docker;
impl Docker {
    async fn command(args: &[String]) -> Result<Vec<u8>> {
        let output = tokio::time::timeout(
            Duration::from_secs(30),
            Command::new("docker")
                .args(args)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("Docker command timed out")??;
        // Never include arbitrary daemon output: it can contain registration secrets.
        ensure!(
            output.status.success(),
            "Docker {} failed (check daemon, image and permissions)",
            args.first().map(String::as_str).unwrap_or("command")
        );
        Ok(output.stdout)
    }
    pub async fn info() -> Result<String> {
        let out = Self::command(&[
            "info".into(),
            "--format".into(),
            "{{.OperatingSystem}}".into(),
        ])
        .await?;
        Ok(String::from_utf8_lossy(&out).trim().to_owned())
    }
    pub async fn shares(directory: &Path, image: &str) -> Result<()> {
        // Bind mounts resolve where the daemon runs: Docker Desktop shares only some host
        // folders with its VM, and a remote daemon sees none of them.
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let mut probe = tempfile::Builder::new()
            .prefix(".probe-")
            .tempfile_in(directory)?;
        probe.write_all(nonce.as_bytes())?;
        probe.flush()?;
        let out = Self::command(&[
            "run".into(),
            "--rm".into(),
            "--pull=never".into(),
            "--label".into(),
            "io.runnerctl.probe=bind".into(),
            "--network=none".into(),
            "--user=0".into(),
            "--security-opt=no-new-privileges:true".into(),
            "--volume".into(),
            format!("{}:/probe:ro,Z", probe.path().display()),
            "--entrypoint=/bin/sh".into(),
            image.into(),
            "-c".into(),
            "cat /probe".into(),
        ])
        .await;
        ensure!(
            out.is_ok_and(|out| out == nonce.as_bytes()),
            "Docker cannot read {} through a bind mount; run the daemon on this machine and share this path with it",
            directory.display()
        );
        Ok(())
    }
    pub async fn image(image: &str) -> Result<()> {
        Self::command(&["image".into(), "inspect".into(), image.into()]).await?;
        Ok(())
    }
    pub async fn image_version(image: &str) -> Result<Option<String>> {
        let bytes = Self::command(&["image".into(), "inspect".into(), image.into()]).await?;
        let data: Value = serde_json::from_slice(&bytes)?;
        Ok(data[0]["Config"]["Labels"]["io.runnerctl.runner.version"]
            .as_str()
            .map(str::to_owned))
    }
    pub async fn pull(image: &str) -> Result<()> {
        let status = tokio::time::timeout(
            Duration::from_secs(600),
            Command::new("docker")
                .args(["pull", image])
                .stdout(std::process::Stdio::null())
                .kill_on_drop(true)
                .status(),
        )
        .await
        .context("Docker pull timed out")??;
        ensure!(
            status.success(),
            "Docker pull failed; check registry access and image tag"
        );
        Ok(())
    }
    pub async fn list(manager: &str) -> Result<Vec<Value>> {
        let ids = Self::command(&[
            "ps".into(),
            "-aq".into(),
            "--filter".into(),
            format!("label=io.runnerctl.manager={manager}"),
        ])
        .await?;
        let ids = String::from_utf8(ids)?;
        if ids.trim().is_empty() {
            return Ok(vec![]);
        }
        let mut args = vec!["inspect".into()];
        args.extend(ids.split_whitespace().map(str::to_owned));
        Ok(serde_json::from_slice(&Self::command(&args).await?)?)
    }
    pub fn owned(container: &Value, manager: &str, run: &Run) -> bool {
        let labels = &container["Config"]["Labels"];
        labels["io.runnerctl.manager"].as_str() == Some(manager)
            && labels["io.runnerctl.run"].as_str() == Some(&run.id)
            && labels["io.runnerctl.pool"].as_str() == Some(&run.pool_id)
    }
    fn create_args(manager: &str, run: &Run, paths: &Paths) -> Result<Vec<String>> {
        let bootstrap = paths.home.join("runs").join(&run.id);
        let mut args: Vec<String> = vec![
            "create".into(),
            "--name".into(),
            run.id.clone(),
            "--restart=no".into(),
            "--init".into(),
            "--log-opt".into(),
            "max-size=10m".into(),
            "--log-opt".into(),
            "max-file=3".into(),
            "--security-opt".into(),
            "no-new-privileges:true".into(),
        ];
        if let Some(cpus) = run.pool.cpus {
            args.extend(["--cpus".into(), cpus.to_string()]);
        }
        if let Some(memory) = &run.pool.memory {
            args.extend(["--memory".into(), memory.clone()]);
        }
        for (key, value) in [
            ("manager", manager),
            ("pool", run.pool_id.as_str()),
            ("run", run.id.as_str()),
        ] {
            args.extend(["--label".into(), format!("io.runnerctl.{key}={value}")]);
        }
        let labels = run
            .pool
            .labels
            .iter()
            .cloned()
            .chain(std::iter::once(format!("pool:{}", run.pool.name)))
            .collect::<Vec<_>>()
            .join(",");
        for (key, value) in [
            ("GITHUB_ORG", run.pool.organization.as_str()),
            ("RUNNER_NAME", run.id.as_str()),
            ("RUNNER_LABELS", labels.as_str()),
            ("RUNNER_GROUP", run.pool.runner_group.as_str()),
        ] {
            args.extend(["--env".into(), format!("{key}={value}")]);
        }
        args.extend(["--env".into(), format!("GITHUB_URL={}", run.pool.url())]);
        if run.pool.repository.is_some() {
            args.extend(["--env".into(), "RUNNER_SCOPE=repo".into()]);
        }
        // The directory holds only a short-lived registration token and completion marker.
        // :Z supports enforcing SELinux hosts; the root entrypoint consumes the token.
        args.extend([
            "--volume".into(),
            format!("{}:/run/runnerctl:Z", bootstrap.display()),
        ]);
        if run.pool.docker_socket {
            // Services in non-container jobs are reached via localhost, just as
            // on a bare-metal runner. The shared daemon is already a host trust boundary.
            args.extend(["--network".into(), "host".into()]);
            // The host daemon must see the same runtime/workspace paths as the runner.
            let runtime = bootstrap.join("runtime");
            let shared = daemon_path(&runtime)?;
            // The runner starts shell steps from scripts under its root, unquoted.
            ensure!(
                !shared.contains(char::is_whitespace),
                "Docker socket pools need a --home path without spaces: {}",
                paths.home.display()
            );
            args.extend([
                "--volume".into(),
                format!("{}:{shared}:Z", runtime.display()),
                "--env".into(),
                format!("RUNNER_ROOT={shared}"),
                "--env".into(),
                format!("RUNNER_UID={}", host_ids().0),
            ]);
            args.extend([
                "--volume".into(),
                "/var/run/docker.sock:/var/run/docker.sock".into(),
            ]);
        }
        args.push(run.pool.image.clone());
        Ok(args)
    }
    pub async fn create(manager: &str, run: &Run, paths: &Paths) -> Result<()> {
        if run.pool.docker_socket {
            crate::config::secure_dir(&paths.home.join("runs").join(&run.id).join("runtime"))?;
        }
        let args = Self::create_args(manager, run, paths)?;
        Self::command(&args).await?;
        Ok(())
    }
    pub async fn start(id: &str) -> Result<()> {
        Self::command(&["start".into(), id.into()]).await?;
        Ok(())
    }
    pub async fn remove(id: &str, force: bool) -> Result<()> {
        let mut args = vec!["rm".into()];
        if force {
            args.push("--force".into());
        }
        args.push(id.into());
        Self::command(&args).await?;
        Ok(())
    }
    pub async fn repair_runtime_permissions(run: &Run, paths: &Paths) -> Result<()> {
        // A job container can create root-owned files in the shared workspace.
        // Mount only this already-stopped runner's directory, without network or socket access.
        let directory = paths.home.join("runs").join(&run.id);
        Self::command(&[
            "run".into(),
            "--rm".into(),
            "--name".into(),
            format!("{}-cleanup", run.id),
            "--label".into(),
            format!("io.runnerctl.cleanup={}", run.id),
            "--network=none".into(),
            "--user=0".into(),
            "--cpus=1".into(),
            "--memory=128m".into(),
            "--cap-drop=ALL".into(),
            "--cap-add=CHOWN".into(),
            "--cap-add=DAC_OVERRIDE".into(),
            "--cap-add=FOWNER".into(),
            "--security-opt=no-new-privileges:true".into(),
            "--volume".into(),
            format!("{}:/cleanup:Z", directory.display()),
            "--entrypoint=/bin/sh".into(),
            run.pool.image.clone(),
            "-c".into(),
            "chown -hR \"$1:$2\" /cleanup && chmod -R u+rwX /cleanup".into(),
            "--".into(),
            host_ids().0.to_string(),
            host_ids().1.to_string(),
        ])
        .await?;
        Ok(())
    }
    pub async fn boot_time(image: &str) -> Result<u64> {
        // btime is not namespaced, so a container reports the kernel running the daemon:
        // the host on Linux, the VM on Docker Desktop, OrbStack and WSL2.
        let out = Self::command(&[
            "run".into(),
            "--rm".into(),
            "--pull=never".into(),
            "--label".into(),
            "io.runnerctl.probe=boot-time".into(),
            "--network=none".into(),
            "--user=65534:65534".into(),
            "--cap-drop=ALL".into(),
            "--security-opt=no-new-privileges:true".into(),
            "--entrypoint=/bin/sh".into(),
            image.into(),
            "-c".into(),
            "while read -r key value _; do if [ \"$key\" = btime ]; then echo \"$value\"; fi; done < /proc/stat".into(),
        ])
        .await?;
        String::from_utf8(out)?
            .trim()
            .parse()
            .context("cannot read the Docker host boot time")
    }
    pub async fn logs(id: &str) -> Result<String> {
        let out = tokio::time::timeout(
            Duration::from_secs(30),
            Command::new("docker")
                .args(["logs", "--tail", "1000", id])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("Docker logs timed out")??;
        ensure!(out.status.success(), "cannot read Docker logs");
        let mut bytes = out.stdout;
        bytes.extend(out.stderr);
        bytes.truncate(1024 * 1024);
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
    pub async fn archive(id: &str, paths: &Paths) -> Result<()> {
        let file = paths.home.join("logs").join(format!("{id}.tar"));
        // Docker cp creates a bounded archive on stdout; stream to disk and cap size.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut child = Command::new("docker")
            .args([
                "cp",
                &format!("{id}:/home/runner/actions-runner/_diag/."),
                "-",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let result: Result<()> = tokio::time::timeout(Duration::from_secs(30), async {
            let mut stdout = child
                .stdout
                .take()
                .ok_or_else(|| anyhow!("missing Docker stdout"))?
                .take(32 * 1024 * 1024 + 1);
            let mut target = tokio::fs::File::create(&file).await?;
            let size = tokio::io::copy(&mut stdout, &mut target).await?;
            target.flush().await?;
            ensure!(
                size <= 32 * 1024 * 1024,
                "diagnostic archive exceeds 32 MiB; export manually before cleanup"
            );
            ensure!(child.wait().await?.success(), "diagnostic archive failed");
            Ok(())
        })
        .await
        .context("diagnostic archive timed out")?;
        result
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn windows_paths_map_to_docker_desktop_vm_paths() {
        assert_eq!(
            daemon_path(Path::new(r"C:\Users\me\AppData\Local\runnerctl\runs")).unwrap(),
            "/run/desktop/mnt/host/c/Users/me/AppData/Local/runnerctl/runs"
        );
        assert_eq!(
            daemon_path(Path::new(r"\\?\D:\runnerctl")).unwrap(),
            "/run/desktop/mnt/host/d/runnerctl"
        );
        assert!(daemon_path(Path::new(r"\\server\share\runnerctl")).is_err());
    }
}
