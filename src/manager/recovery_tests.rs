use super::*;
use serde_json::{Value, json};
use std::process::Command;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::Command as TokioCommand,
    task::JoinHandle,
};

struct DockerCleanup {
    manager_id: String,
}

impl Drop for DockerCleanup {
    fn drop(&mut self) {
        let Ok(output) = Command::new("docker")
            .args([
                "ps",
                "-aq",
                "--filter",
                &format!("label=io.runnerctl.manager={}", self.manager_id),
            ])
            .output()
        else {
            return;
        };
        for id in String::from_utf8_lossy(&output.stdout).split_whitespace() {
            let _ = Command::new("docker").args(["rm", "-f", id]).output();
        }
    }
}

struct ServerCleanup(Option<JoinHandle<()>>);

impl Drop for ServerCleanup {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

async fn github_connection(mut stream: TcpStream, live_id: String) {
    let mut reader = BufReader::new(&mut stream);
    let mut first = String::new();
    if reader.read_line(&mut first).await.is_err() {
        return;
    }
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.is_err() || line.is_empty() || line == "\r\n" {
            break;
        }
    }
    let (status, body) = if first.contains("registration-token") {
        ("201 Created", json!({"token":"short-lived-test-token"}))
    } else if first.starts_with("DELETE") {
        ("204 No Content", Value::Null)
    } else {
        (
            "200 OK",
            json!({
                "total_count": 1,
                "runners": [{
                    "id": 9001,
                    "name": live_id,
                    "status": "online",
                    "busy": true
                }]
            }),
        )
    };
    let body = if body.is_null() {
        String::new()
    } else {
        body.to_string()
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = reader.get_mut().write_all(response.as_bytes()).await;
}

async fn github_server(live_id: String) -> (String, ServerCleanup) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let live_id = live_id.clone();
            tokio::spawn(github_connection(stream, live_id));
        }
    });
    (base, ServerCleanup(Some(task)))
}

async fn docker_command(args: Vec<String>) -> anyhow::Result<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        TokioCommand::new("docker")
            .args(&args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("test Docker command timed out")??;
    anyhow::ensure!(
        output.status.success(),
        "docker command failed: {}",
        args.join(" ")
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

async fn controlled_container(
    paths: &Paths,
    manager_id: &str,
    run: &Run,
    script: &str,
) -> anyhow::Result<()> {
    let bootstrap = paths.home.join("runs").join(&run.id);
    config::secure_dir(&bootstrap)?;
    docker_command(vec![
        "create".into(),
        "--name".into(),
        run.id.clone(),
        "--restart=no".into(),
        "--init".into(),
        "--network".into(),
        "none".into(),
        "--label".into(),
        format!("io.runnerctl.manager={manager_id}"),
        "--label".into(),
        format!("io.runnerctl.pool={}", run.pool_id),
        "--label".into(),
        format!("io.runnerctl.run={}", run.id),
        "--volume".into(),
        format!("{}:/run/runnerctl:Z", bootstrap.display()),
        "--entrypoint".into(),
        "/bin/sh".into(),
        run.pool.image.clone(),
        "-c".into(),
        format!(
            "mkdir -p /home/runner/actions-runner/_diag && \
             echo test > /home/runner/actions-runner/_diag/test.log; {script}"
        ),
    ])
    .await?;
    Ok(())
}

async fn start_container(id: &str) -> anyhow::Result<()> {
    docker_command(vec!["start".into(), id.into()]).await?;
    Ok(())
}

async fn exit_container(id: &str) -> anyhow::Result<()> {
    start_container(id).await?;
    docker_command(vec!["wait".into(), id.into()]).await?;
    Ok(())
}

fn fixture() -> anyhow::Result<(tempfile::TempDir, Engine)> {
    let dir = tempfile::tempdir()?;
    let paths = Paths::new(dir.path().to_owned())?;
    paths.prepare()?;
    let credential = paths.home.join("credentials/test");
    config::atomic_write(&credential, b"test-pat")?;

    let mut config = Config::default();
    config.manager.max_runners = 8;
    config.manager.max_parallel_creates = 2;
    config.auth.insert(
        "team".into(),
        config::Auth {
            credential: format!("file:{}", credential.display()),
        },
    );
    config.pools.push(config::Pool {
        name: "recovery".into(),
        organization: "test-org".into(),
        auth: "team".into(),
        labels: vec!["recovery".into()],
        runner_group: "Default".into(),
        replicas: 3,
        image: "local/runnerctl-test:1".into(),
        docker_socket: false,
    });
    config::atomic_write(&paths.config(), toml::to_string(&config)?.as_bytes())?;
    Ok((dir, Engine::open(paths)?))
}

fn test_run(e: &Engine, id: &str, phase: &str) -> Run {
    let pool = e.state.config.pools[0].clone();
    let pool_state = &e.state.pools[&pool.name];
    Run {
        id: id.into(),
        pool_id: pool_state.id.clone(),
        pool,
        generation: pool_state.generation,
        created_at: now(),
        phase: phase.into(),
        remote_id: None,
        retiring: false,
        force: false,
        completed_job: false,
        log_saved: false,
    }
}

fn run_labels(containers: &[Value]) -> Vec<String> {
    containers
        .iter()
        .filter_map(|container| {
            container["Config"]["Labels"]["io.runnerctl.run"]
                .as_str()
                .map(str::to_owned)
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires Docker and the local/runnerctl-test:1 image; see tests/README.md"]
async fn docker_tick_recovery_backoff_restart_and_doctor() {
    let (_dir, mut e) = fixture().unwrap();
    let manager_id = e.state.manager_id.clone();
    let prefix = &manager_id[..8];
    let live_id = format!("rc-{prefix}-live");
    let (base, _server) = github_server(live_id.clone()).await;
    e.github = Github::testing(base.clone());
    let _cleanup = DockerCleanup {
        manager_id: manager_id.clone(),
    };
    e.handle(Request::Start {
        pool: Some("recovery".into()),
        all: false,
    })
    .await
    .unwrap();

    let exited_a_id = format!("rc-{prefix}-exit-a");
    let exited_b_id = format!("rc-{prefix}-exit-b");
    let live = test_run(&e, &live_id, "starting");
    let exited_a = test_run(&e, &exited_a_id, "exited");
    let exited_b = test_run(&e, &exited_b_id, "exited");
    e.state.runs.insert(live_id.clone(), live.clone());
    e.state.runs.insert(exited_a_id.clone(), exited_a.clone());
    e.state.runs.insert(exited_b_id.clone(), exited_b.clone());
    e.save().unwrap();

    controlled_container(&e.paths, &manager_id, &live, "sleep 300")
        .await
        .unwrap();
    start_container(&live_id).await.unwrap();
    controlled_container(&e.paths, &manager_id, &exited_a, "exit 143")
        .await
        .unwrap();
    exit_container(&exited_a_id).await.unwrap();
    controlled_container(&e.paths, &manager_id, &exited_b, "exit 143")
        .await
        .unwrap();
    exit_container(&exited_b_id).await.unwrap();
    assert!(
        !e.paths
            .home
            .join("runs")
            .join(&exited_a_id)
            .join("completed")
            .exists()
    );
    assert!(
        !e.paths
            .home
            .join("runs")
            .join(&exited_b_id)
            .join("completed")
            .exists()
    );

    let first_tick_at = now();
    e.tick().await.unwrap();
    let pool = e.state.pools["recovery"].clone();
    assert_eq!(e.state.runs.len(), 1);
    assert_eq!(e.state.runs[&live_id].phase, "busy");
    assert_eq!(pool.failures, 1);
    assert_eq!(pool.retry_scope, RetryScope::Provisioning);
    assert!(pool.retry_at > now());
    assert!(pool.retry_at >= first_tick_at);
    assert!(pool.retry_at <= now() + MAX_RETRY_SECONDS);
    let containers = Docker::list(&manager_id).await.unwrap();
    assert_eq!(run_labels(&containers), vec![live_id.clone()]);
    for id in [&exited_a_id, &exited_b_id] {
        assert!(e.paths.home.join("logs").join(format!("{id}.log")).exists());
        assert!(e.paths.home.join("logs").join(format!("{id}.tar")).exists());
    }
    // Keep the backoff scenario deterministic even on a slow Docker host.
    let retry_at = now() + MAX_RETRY_SECONDS;
    e.state.pools.get_mut("recovery").unwrap().retry_at = retry_at;
    e.save().unwrap();

    let doctor = e.handle(Request::Doctor).await.unwrap();
    assert_eq!(doctor["healthy"], json!(false));
    let checks = doctor["checks"].as_array().unwrap();
    assert!(
        checks
            .iter()
            .find(|check| check["check"] == "docker")
            .is_some_and(|check| check["ok"] == json!(true))
    );
    assert!(
        checks
            .iter()
            .find(|check| check["check"] == "github:recovery")
            .is_some_and(|check| check["ok"] == json!(true))
    );
    assert!(
        checks
            .iter()
            .find(|check| check["check"] == "image:recovery")
            .is_some_and(|check| check["ok"] == json!(true))
    );
    assert!(
        checks
            .iter()
            .find(|check| check["check"] == "pool:recovery")
            .is_some_and(|check| check["ok"] == json!(false))
    );

    let exited_c_id = format!("rc-{prefix}-exit-c");
    let exited_c = test_run(&e, &exited_c_id, "exited");
    e.state.runs.insert(exited_c_id.clone(), exited_c.clone());
    e.save().unwrap();
    controlled_container(&e.paths, &manager_id, &exited_c, "exit 143")
        .await
        .unwrap();
    exit_container(&exited_c_id).await.unwrap();
    e.tick().await.unwrap();
    assert_eq!(e.state.pools["recovery"].failures, 1);
    assert_eq!(e.state.pools["recovery"].retry_at, retry_at);
    assert_eq!(e.state.runs.len(), 1);
    assert_eq!(e.state.runs[&live_id].phase, "busy");
    let containers = Docker::list(&manager_id).await.unwrap();
    assert_eq!(run_labels(&containers), vec![live_id.clone()]);

    let paths = e.paths.clone();
    drop(e);
    let mut e = Engine::open(paths).unwrap();
    assert_eq!(e.state.manager_id, manager_id);
    e.github = Github::testing(base);
    assert_eq!(e.state.pools["recovery"].failures, 1);
    assert_eq!(
        e.state.pools["recovery"].retry_scope,
        RetryScope::Provisioning
    );
    e.state.pools.get_mut("recovery").unwrap().retry_at = now().saturating_sub(1);
    e.save().unwrap();

    e.tick().await.unwrap();
    let replacement_id = e
        .state
        .runs
        .keys()
        .find(|id| *id != &live_id)
        .cloned()
        .expect("elapsed backoff provisions a replacement");
    assert_eq!(e.state.runs[&replacement_id].phase, "creating");
    assert!(e.state.pools["recovery"].running);

    e.tick().await.unwrap();
    assert_eq!(e.state.runs[&replacement_id].phase, "starting");
    let containers = Docker::list(&manager_id).await.unwrap();
    let labels = run_labels(&containers);
    assert!(labels.contains(&live_id));
    assert!(labels.contains(&replacement_id));
}
