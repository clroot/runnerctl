use crate::{config::Paths, state::Run};
use anyhow::{Context, Result, anyhow, ensure};
use serde_json::Value;
use std::time::Duration;
use tokio::process::Command;

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
    pub async fn info() -> Result<()> {
        Self::command(&[
            "info".into(),
            "--format".into(),
            "{{.ServerVersion}}".into(),
        ])
        .await?;
        Ok(())
    }
    pub async fn image(image: &str) -> Result<()> {
        Self::command(&["image".into(), "inspect".into(), image.into()]).await?;
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
    pub async fn create(manager: &str, run: &Run, paths: &Paths) -> Result<()> {
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
        // The directory holds only a short-lived registration token and completion marker.
        // :Z supports enforcing SELinux hosts; the root entrypoint consumes the token.
        args.extend([
            "--volume".into(),
            format!("{}:/run/runnerctl:Z", bootstrap.display()),
        ]);
        if run.pool.docker_socket {
            args.extend([
                "--volume".into(),
                "/var/run/docker.sock:/var/run/docker.sock".into(),
            ]);
        }
        args.push(run.pool.image.clone());
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
