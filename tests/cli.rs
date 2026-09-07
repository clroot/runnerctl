use assert_cmd::Command;
use serde_json::Value;

fn cmd(home: &std::path::Path) -> Command {
    let mut c = Command::new(assert_cmd::cargo::cargo_bin!("runnerctl"));
    c.arg("--home").arg(home);
    c
}
#[test]
fn cli_multi_pool_offline_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let h = dir.path();
    cmd(h).args(["init", "--org", "example"]).assert().success();
    for name in ["validate", "build"] {
        cmd(h)
            .args([
                "pool",
                "add",
                name,
                "--org",
                "example",
                "--auth",
                "example",
                "--labels",
                name,
                "--replicas",
                "2",
            ])
            .assert()
            .success();
    }
    cmd(h).args(["start", "--all"]).assert().success();
    cmd(h).args(["stop", "build"]).assert().success();
    cmd(h).args(["scale", "build", "3"]).assert().success();
    let out = cmd(h).args(["--json", "status"]).output().unwrap();
    assert!(out.status.success());
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["pools"][0]["intent"], "running");
    assert_eq!(value["pools"][1]["intent"], "stopped");
    assert_eq!(value["pools"][1]["replicas"], 3);
    cmd(h).args(["stop"]).assert().failure();
    cmd(h).args(["start", "build", "--all"]).assert().failure();
    cmd(h).args(["scale", "build", "100"]).assert().failure();
    cmd(h).args(["config", "check"]).assert().success();
}
#[test]
fn credentials_are_not_in_config_or_output() {
    let dir = tempfile::tempdir().unwrap();
    let h = dir.path();
    cmd(h).args(["init", "--org", "example"]).assert().success();
    let out = cmd(h)
        .args(["auth", "login", "--profile", "example", "--token-stdin"])
        .write_stdin("not-a-real-pat\n")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("not-a-real-pat"));
    assert!(
        !std::fs::read_to_string(h.join("config.toml"))
            .unwrap()
            .contains("not-a-real-pat")
    );
}

#[test]
fn daemon_socket_and_exclusive_writer() {
    let dir = tempfile::tempdir().unwrap();
    let h = dir.path();
    cmd(h).args(["init", "--org", "example"]).assert().success();
    let process = std::process::Command::new(assert_cmd::cargo::cargo_bin!("runnerctl"))
        .arg("--home")
        .arg(h)
        .arg("manager")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    struct Guard(std::process::Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _guard = Guard(process);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !h.join("manager.sock").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    cmd(h).arg("manager").assert().failure();
    let out = cmd(h).args(["--json", "status"]).output().unwrap();
    assert!(out.status.success());
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_ne!(value["manager_running"], false);
    cmd(h)
        .args([
            "pool", "add", "build", "--org", "example", "--auth", "example", "--labels", "build",
        ])
        .assert()
        .success();
    let out = cmd(h).args(["--json", "status"]).output().unwrap();
    assert!(out.status.success());
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["pools"][0]["intent"], "stopped");
}
