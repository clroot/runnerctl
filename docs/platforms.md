# macOS and Windows

Runners are Linux containers. On macOS and Windows, Docker runs them in a Linux VM, so jobs see a Linux runner (arm64 on Apple silicon). Workflows that need a macOS or Windows runner, such as Xcode builds, cannot use these pools.

## macOS

Install OrbStack or Docker Desktop and keep it running. Install the Xcode Command Line Tools (`xcode-select --install`) for the linker and Rust/Cargo 1.85 or later, then:

```bash
cargo install runnerctl --locked
runnerctl image pull
```

The [first runner](../README.md#your-first-runner) steps are the same as on Linux. `service install` creates the LaunchAgent `~/Library/LaunchAgents/io.runnerctl.manager.plist`:

- It runs while you are logged in. Docker Desktop and OrbStack also need a logged-in user, so an unattended Mac needs automatic login and its Docker app set to open at login.
- It writes its log to `<home>/manager.log`.
- It records `PATH`, `DOCKER_HOST`, `DOCKER_CONTEXT` and `DOCKER_CONFIG` from the shell that runs `service install`. Run it again after switching Docker apps or contexts.
- macOS privacy protection blocks agents from Desktop, Documents, Downloads and iCloud Drive, so `service install` refuses a binary or `--home` in those folders. The defaults, `~/.cargo/bin` and `~/.config/runnerctl`, are fine.
- `service uninstall` stops and disables the agent and keeps the plist.

Docker Desktop shares only some folders with its VM: `/Users`, `/Volumes`, `/private`, `/tmp` and `/var/folders` by default. Keep `--home` in one of them. `runnerctl doctor` fails its `docker:bind` check when the daemon cannot read the home directory through a bind mount.

### Docker socket pools on macOS

`--docker-socket` pools use the VM's network and Docker daemon. Docker's file sharing maps the runner workspace to the same path inside the VM, so job and service containers see the workspace as on Linux. Files created in containers belong to your macOS user, so cleanup does not need the ownership repair used on Linux.

OrbStack passed the local checks in [compatibility](compatibility.md), including a service reached through a random published port. Docker Desktop has not been validated. Its host networking setting (Settings > Resources > Network, 4.34 and later) is the first thing to check if jobs cannot reach service containers. If a job cannot reach a service on its reported port, map a fixed host port such as `5432:5432`.

Docker Desktop and OrbStack restart their VM when the app restarts or updates. runnerctl reads the VM's boot time, so runners stopped that way are replaced without failure backoff.

## Windows

Windows has two setups, both with Docker Desktop in Linux containers mode: runnerctl inside a WSL2 Linux distribution, or [natively](#windows-in-command-prompt-or-powershell) in Command Prompt or PowerShell. Use one of them for a given `--home`. WSL2 behaves like Linux; the native setup needs no Linux distribution or systemd.

### WSL2

Run runnerctl inside a WSL2 Linux distribution, with Docker Desktop's WSL integration as its Docker host.

1. Install a WSL2 distribution such as Ubuntu and Docker Desktop 4.30 or later. Enable the distribution in Settings > Resources > WSL integration. Docker Desktop 4.28 and 4.29 broke workspace bind mounts made from inside containers, which `--docker-socket` pools rely on.
2. Enable systemd in the distribution. Add this to `/etc/wsl.conf`, run `wsl.exe --shutdown` from Windows, and reopen the distribution:

   ```ini
   [boot]
   systemd=true
   ```

3. Inside the distribution, install `build-essential` and Rust/Cargo 1.85 or later, then run `cargo install runnerctl --locked` and `runnerctl image pull`.
4. Keep `--home` on the distribution's Linux filesystem, such as the default `~/.config/runnerctl`. Windows drives under `/mnt` do not keep the Unix permissions that runnerctl requires for credentials.
5. Follow the [first runner](../README.md#your-first-runner) steps. `service install` and `sudo loginctl enable-linger "$USER"` work as on Linux.

WSL stops an idle distribution after 15 seconds by default, even while a systemd service runs. To keep the manager running, add this to `%UserProfile%\.wslconfig` and run `wsl.exe --shutdown`:

```ini
[general]
instanceIdleTimeout=-1
```

WSL and Docker Desktop both need a signed-in Windows user. For an unattended PC, enable automatic sign-in, set Docker Desktop to start when you sign in, and start the distribution at sign-in, for example with a Task Scheduler task that runs `wsl.exe -d Ubuntu --exec true`.

The same Docker socket notes as on macOS apply: Docker Desktop maps workspace paths from the distribution for containers that jobs create. With Docker Desktop's default settings, a shell job cannot reach a service container through its published port on `localhost`, because the port is published only on the Windows side. Enable host networking (Settings > Resources > Network); with it, the full [compatibility suite](compatibility.md) passed. Otherwise run such jobs as container jobs and reach services by name. See [compatibility](compatibility.md) for the validated versions.

### Windows in Command Prompt or PowerShell

1. Install Docker Desktop 4.30 or later with the WSL 2 backend and keep it in Linux containers mode.
2. Install Rust/Cargo 1.85 or later with [rustup](https://rustup.rs) and the Visual Studio C++ build tools it offers, then run `cargo install runnerctl --locked` and `runnerctl image pull`.
3. Follow the [first runner](../README.md#your-first-runner) steps in Command Prompt or PowerShell.

When Smart App Control is on (Windows Security > App & browser control), Windows can refuse to start `runnerctl.exe` with "An Application Control policy has blocked this file", because a program you build yourself has no signature or reputation. Smart App Control has no per-program exception; use the WSL2 setup instead, or turn Smart App Control off.

The default home is `%LOCALAPPDATA%\runnerctl`. When runnerctl first uses a home directory, it replaces the inherited permissions with access for you, SYSTEM and Administrators only, the counterpart of mode 0700. Credential and private key files referenced with `file:` need the same restriction. This command sets it:

```powershell
icacls C:\path\to\key.pem /inheritance:r /grant:r "${env:USERNAME}:F"
```

The CLI reaches the manager through a named pipe that only you and SYSTEM can open. Its random name is stored in `<home>\manager.pipe`.

`service install` registers the scheduled task `runnerctl-manager` and starts it:

- It runs while you are signed in, like Docker Desktop, and starts at each sign-in. For an unattended PC, enable automatic sign-in and set Docker Desktop to start when you sign in.
- It runs without a window through `conhost.exe --headless`, writes its log to `<home>\manager.log`, and restarts the manager 10 seconds after a failure.
- It sees the environment variables of your Windows account, not those of the shell that runs `service install`. Set `DOCKER_HOST` or a `--env` credential variable as a user environment variable.
- Creating the task does not need administrator rights.
- `service uninstall` stops and disables the task and keeps it. `schtasks /delete /tn runnerctl-manager /f` removes it.

`--docker-socket` pools work as with WSL2. Docker Desktop mounts local drives in its VM at `/run/desktop/mnt/host/<drive>`, so runnerctl gives the runner its workspace at that path, for example `/run/desktop/mnt/host/c/Users/me/AppData/Local/runnerctl/runs/...`, and containers that jobs start see the same files. Keep `--home` on a local drive. The runner cannot start shell steps from a path with spaces, so if your user name contains a space, use a home such as `C:\runnerctl`. Files that containers create belong to your Windows user, so cleanup needs no ownership repair. Each runner copies its runtime, about 700 MB, into its workspace through Docker Desktop's file sharing, so a new runner takes a few minutes to come online, longer than on Linux or WSL2. Keep idle replicas available so jobs do not wait for it.

## Checking a Docker setup

From a clone of this repository, on the machine and Docker runtime you plan to use:

```bash
docker build -t local/runnerctl-test-base:1 runner
docker build -t local/runnerctl-test:1 -f tests/Dockerfile .
bash tests/entrypoint.sh
bash tests/docker-socket.sh
cargo test --locked docker_ -- --ignored
```

On native Windows, run the same commands in PowerShell, with `tests\docker-socket.ps1` in place of `tests/docker-socket.sh`. The entrypoint test needs bash, so run it in WSL2 or on Linux.

`tests/docker-socket.sh` checks `--docker-socket` requirements without GitHub: the non-root runner reaches the daemon, and a container it starts shares its workspace through the same host path. Then run the [real GitHub compatibility suite](../tests/README.md#real-github-compatibility-suite) with a dedicated pool.
