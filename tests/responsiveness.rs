use assert_cmd::Command;
use serde_json::Value;
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::Path,
    process::{Child, Stdio},
    time::{Duration, Instant},
};

struct Manager(Child);
impl Drop for Manager {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn cli(home: &Path) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("runnerctl"));
    command
        .arg("--home")
        .arg(home)
        .timeout(Duration::from_secs(2));
    command
}
fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} was not created",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn start_manager(home: &Path, bin: &Path, marker: &Path) -> Manager {
    let child = std::process::Command::new(assert_cmd::cargo::cargo_bin!("runnerctl"))
        .args(["--home", home.to_str().unwrap(), "manager"])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("RUNNERCTL_TEST_MARKER", marker)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&home.join("manager.sock"));
    Manager(child)
}
fn fake_docker(bin: &Path, body: &str) {
    fs::create_dir_all(bin).unwrap();
    let path = bin.join("docker");
    fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn slow_reconciliation_accepts_stop_and_does_not_provision_from_stale_intent() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("state");
    let bin = dir.path().join("bin");
    let marker = dir.path().join("slow-operation");
    fake_docker(
        &bin,
        "if [ \"$1\" = ps ]; then touch \"$RUNNERCTL_TEST_MARKER\"; sleep 3; exit 0; fi\nexit 1",
    );
    cli(&home)
        .args(["init", "--org", "example"])
        .assert()
        .success();
    cli(&home)
        .args([
            "pool", "add", "test", "--org", "example", "--auth", "example", "--labels", "test",
        ])
        .assert()
        .success();
    cli(&home).args(["start", "test"]).assert().success();
    let _manager = start_manager(&home, &bin, &marker);
    wait_for(&marker);
    let started = Instant::now();
    cli(&home)
        .args(["stop", "test", "--wait", "--timeout", "1s"])
        .assert()
        .success();
    assert!(started.elapsed() < Duration::from_secs(2));
    std::thread::sleep(Duration::from_millis(3200));
    let output = cli(&home).args(["--json", "status"]).output().unwrap();
    assert!(output.status.success());
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["pools"][0]["intent"], "stopped");
    assert_eq!(status["pools"][0]["active"], 0);
    assert_eq!(status["pools"][0]["error"], Value::Null);
}

#[test]
fn slow_doctor_and_incomplete_client_do_not_block_status() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("state");
    let bin = dir.path().join("bin");
    let marker = dir.path().join("slow-doctor");
    fake_docker(
        &bin,
        "touch \"$RUNNERCTL_TEST_MARKER\"\nsleep 3\nprintf 'test-daemon\\n'",
    );
    cli(&home)
        .args(["init", "--org", "example"])
        .assert()
        .success();
    let _manager = start_manager(&home, &bin, &marker);
    let _incomplete_client = UnixStream::connect(home.join("manager.sock")).unwrap();
    let mut doctor = std::process::Command::new(assert_cmd::cargo::cargo_bin!("runnerctl"))
        .args(["--home", home.to_str().unwrap(), "doctor"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&marker);
    let started = Instant::now();
    cli(&home).arg("status").assert().success();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(doctor.wait().unwrap().success());
}
