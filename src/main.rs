mod config;
mod docker;
mod github;
mod image;
mod manager;
mod protocol;
mod service;
mod state;

use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand};
use config::{AppAuth, Auth, Config, Paths, Pool};
use protocol::Request;
use serde_json::Value;
use std::{fs, path::PathBuf, time::Duration};

#[derive(Parser)]
#[command(
    version,
    about = "Manage independent ephemeral GitHub Actions runner pools"
)]
struct Cli {
    /// Config, state, socket, credentials and logs directory
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Cmd,
}
#[derive(Subcommand)]
enum Cmd {
    Init {
        #[arg(long, required_unless_present = "repo", conflicts_with = "repo")]
        org: Option<String>,
        #[arg(long)]
        repo: Option<String>,
    },
    Auth {
        #[command(subcommand)]
        command: AuthCmd,
    },
    Pool {
        #[command(subcommand)]
        command: PoolCmd,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCmd,
    },
    Manager,
    /// Pull a published runner image without building it locally
    Image {
        #[command(subcommand)]
        command: ImageCmd,
    },
    Service {
        #[command(subcommand)]
        command: ServiceCmd,
    },
    Status {
        #[arg(long)]
        pool: Option<String>,
    },
    Start(Target),
    Stop {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        wait: WaitArgs,
    },
    Scale {
        pool: String,
        replicas: u32,
    },
    Upgrade {
        #[arg(long)]
        pool: String,
        #[arg(long)]
        image: String,
    },
    Logs {
        id: String,
        #[arg(long)]
        pool: String,
        #[arg(long)]
        follow: bool,
    },
    Doctor,
}
#[derive(Subcommand)]
enum ImageCmd {
    Pull {
        #[arg(long)]
        image: Option<String>,
    },
}
#[derive(Args)]
struct WaitArgs {
    /// Wait for all target runners and registrations to be cleaned up
    #[arg(long)]
    wait: bool,
    /// Stop waiting after this duration; never force-cancel jobs on timeout
    #[arg(long, default_value = "5m", value_parser = humantime::parse_duration)]
    timeout: Duration,
}
#[derive(Args)]
struct Target {
    #[arg(required_unless_present = "all", conflicts_with = "all")]
    pool: Option<String>,
    #[arg(long)]
    all: bool,
}
#[derive(Subcommand)]
enum AuthCmd {
    Login {
        #[arg(long)]
        profile: String,
        /// Read token from stdin instead of hidden terminal prompt
        #[arg(long, conflicts_with = "env")]
        token_stdin: bool,
        /// Reference an environment variable available to the manager
        #[arg(long)]
        env: Option<String>,
    },
    /// Authenticate using a GitHub App installation and an RSA PEM key reference
    App {
        #[arg(long)]
        profile: String,
        #[arg(long)]
        app_id: String,
        #[arg(long)]
        installation_id: u64,
        #[arg(long, required_unless_present = "key_env", conflicts_with = "key_env")]
        private_key: Option<PathBuf>,
        #[arg(long)]
        key_env: Option<String>,
    },
}
#[derive(Subcommand)]
enum PoolCmd {
    Add {
        name: String,
        #[arg(long, required_unless_present = "repo", conflicts_with = "repo")]
        org: Option<String>,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        auth: String,
        #[arg(long, value_delimiter = ',', required = true)]
        labels: Vec<String>,
        #[arg(long, default_value_t = 1)]
        replicas: u32,
        #[arg(long, default_value = "Default")]
        runner_group: String,
        #[arg(long, default_value_t = config::default_image())]
        image: String,
        #[arg(long)]
        docker_socket: bool,
        /// CPU quota for each runner container (for example 2 or 0.5)
        #[arg(long)]
        cpus: Option<f64>,
        /// Memory limit for each runner container (for example 2g or 512m)
        #[arg(long)]
        memory: Option<String>,
    },
    List,
    Show {
        name: String,
    },
    Remove {
        name: String,
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        wait: WaitArgs,
    },
}
#[derive(Subcommand)]
enum ConfigCmd {
    Check,
    Apply {
        #[arg(long)]
        revision: Option<u64>,
    },
}
#[derive(Subcommand)]
enum ServiceCmd {
    Install,
    Uninstall,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
async fn dispatch(paths: &Paths, request: Request) -> Result<Value> {
    // Only fall back after a failed connect, never after an uncertain mutation response.
    if tokio::net::UnixStream::connect(paths.socket())
        .await
        .is_ok()
    {
        return manager::send(paths, &request).await;
    }
    let _lock = manager::lock(paths)?;
    let mut engine = manager::Engine::open(paths.clone())?;
    let mut response = engine.handle(request).await?;
    response["manager_running"] = Value::Bool(false);
    Ok(response)
}
async fn run() -> Result<()> {
    let cli = Cli::parse();
    let home = cli.home.unwrap_or_else(|| {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
            })
            .join("runnerctl")
    });
    let paths = Paths::new(home)?;
    paths.prepare()?;
    let mut waiting: Option<(Vec<String>, bool, Duration)> = None;
    let request = match cli.command {
        Cmd::Init { org, repo } => {
            let _lock = manager::lock(&paths)?;
            ensure!(
                !paths.config().exists() && !paths.home.join("state.sqlite").exists(),
                "already initialized"
            );
            if let Some(org) = &org {
                ensure!(config::valid_org(org), "invalid organization");
            }
            if let Some(repo) = &repo {
                ensure!(
                    config::valid_repo(repo),
                    "invalid repository; use OWNER/REPO"
                );
            }
            let profile = org
                .as_deref()
                .or_else(|| repo.as_deref().and_then(|s| s.split('/').next()))
                .context("missing target")?;
            let target_flag = if org.is_some() {
                format!("--org {profile}")
            } else {
                format!("--repo {}", repo.as_ref().unwrap())
            };
            let mut config = Config::default();
            config.auth.insert(
                profile.to_owned(),
                Auth::pat(format!(
                    "file:{}",
                    paths.home.join("credentials").join(profile).display()
                )),
            );
            config::atomic_write(&paths.config(), toml::to_string_pretty(&config)?.as_bytes())?;
            println!(
                "Initialized {}\nNext: runnerctl auth login --profile {profile}\nThen: runnerctl image pull\nThen: runnerctl pool add test {target_flag} --auth {profile} --labels test --replicas 1",
                paths.config().display()
            );
            return Ok(());
        }
        Cmd::Manager => return manager::serve(paths).await,
        Cmd::Image {
            command: ImageCmd::Pull { image },
        } => {
            let result = image::pull(image).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            return Ok(());
        }
        Cmd::Service { command } => {
            return match command {
                ServiceCmd::Install => service::install(&paths),
                ServiceCmd::Uninstall => service::uninstall(),
            };
        }
        Cmd::Auth {
            command:
                AuthCmd::Login {
                    profile,
                    token_stdin,
                    env,
                },
        } => {
            ensure!(config::valid_name(&profile), "invalid profile name");
            let credential = if let Some(env) = env {
                format!("env:{env}")
            } else {
                let token = if token_stdin {
                    use std::io::Read;
                    let mut token = String::new();
                    std::io::stdin().take(16385).read_to_string(&mut token)?;
                    token
                } else {
                    rpassword::prompt_password("GitHub PAT: ")?
                };
                let token = token.trim();
                ensure!(
                    !token.is_empty()
                        && token.len() <= 16384
                        && !token.chars().any(char::is_whitespace),
                    "invalid token"
                );
                // Use a fresh path so an unsuccessful config change leaves old credentials intact.
                let path = paths
                    .home
                    .join("credentials")
                    .join(format!("{profile}-{}", uuid::Uuid::new_v4().simple()));
                config::atomic_write(&path, token.as_bytes())?;
                format!("file:{}", path.display())
            };
            Request::Auth {
                profile,
                credential,
                app: None,
            }
        }
        Cmd::Auth {
            command:
                AuthCmd::App {
                    profile,
                    app_id,
                    installation_id,
                    private_key,
                    key_env,
                },
        } => {
            ensure!(config::valid_name(&profile), "invalid profile name");
            let private_key = match private_key {
                Some(path) => format!("file:{}", fs::canonicalize(path)?.display()),
                None => format!("env:{}", key_env.context("missing private key reference")?),
            };
            let app = AppAuth {
                app_id,
                installation_id,
                private_key,
            };
            let mut check = Config::default();
            check.auth.insert(
                profile.clone(),
                Auth {
                    credential: String::new(),
                    app: Some(app.clone()),
                },
            );
            check.validate()?;
            github::app_jwt(&app)?;
            Request::Auth {
                profile,
                credential: String::new(),
                app: Some(app),
            }
        }
        Cmd::Pool { command } => match command {
            PoolCmd::Add {
                name,
                org,
                repo,
                auth,
                labels,
                replicas,
                runner_group,
                image,
                docker_socket,
                cpus,
                memory,
            } => Request::Add {
                pool: Pool {
                    name,
                    organization: org.unwrap_or_default(),
                    repository: repo,
                    auth,
                    labels,
                    replicas,
                    runner_group,
                    image,
                    docker_socket,
                    cpus,
                    memory,
                },
            },
            PoolCmd::List => Request::Status { pool: None },
            PoolCmd::Show { name } => Request::Status { pool: Some(name) },
            PoolCmd::Remove { name, force, wait } => {
                if wait.wait {
                    waiting = Some((vec![name.clone()], true, wait.timeout));
                }
                Request::Remove { pool: name, force }
            }
        },
        Cmd::Config { command } => match command {
            ConfigCmd::Check => {
                let config = Config::parse(
                    &fs::read_to_string(paths.config()).context("missing config; run init")?,
                )?;
                println!(
                    "Config valid: {} pools, {} / {} replicas",
                    config.pools.len(),
                    config
                        .pools
                        .iter()
                        .map(|p| u64::from(p.replicas))
                        .sum::<u64>(),
                    config.manager.max_runners
                );
                for warning in config.warnings() {
                    eprintln!("warning: {warning}");
                }
                return Ok(());
            }
            ConfigCmd::Apply { revision } => {
                let revision = match revision {
                    Some(r) => r,
                    None => dispatch(&paths, Request::Status { pool: None }).await?["revision"]
                        .as_u64()
                        .context("missing revision")?,
                };
                Request::Apply {
                    text: fs::read_to_string(paths.config())?,
                    revision,
                }
            }
        },
        Cmd::Status { pool } => Request::Status { pool },
        Cmd::Start(Target { pool, all }) => Request::Start { pool, all },
        Cmd::Stop {
            target: Target { pool, all },
            force,
            wait,
        } => {
            if wait.wait {
                let targets = if all {
                    dispatch(&paths, Request::Status { pool: None }).await?["pools"]
                        .as_array()
                        .context("missing pools")?
                        .iter()
                        .filter_map(|p| p["name"].as_str().map(str::to_owned))
                        .collect()
                } else {
                    vec![pool.clone().context("missing pool")?]
                };
                waiting = Some((targets, false, wait.timeout));
            }
            Request::Stop { pool, all, force }
        }
        Cmd::Scale { pool, replicas } => Request::Scale { pool, replicas },
        Cmd::Upgrade { pool, image } => Request::Upgrade { pool, image },
        Cmd::Doctor => Request::Doctor,
        Cmd::Logs { id, pool, follow } => {
            let mut previous = String::new();
            loop {
                let data = dispatch(
                    &paths,
                    Request::Logs {
                        pool: pool.clone(),
                        id: id.clone(),
                    },
                )
                .await?;
                let logs = data["logs"].as_str().unwrap_or_default();
                if let Some(suffix) = logs.strip_prefix(&previous) {
                    print!("{suffix}");
                } else {
                    print!("{logs}");
                }
                use std::io::Write;
                std::io::stdout().flush()?;
                previous = logs.to_owned();
                if !follow {
                    return Ok(());
                }
                tokio::select! {_=tokio::signal::ctrl_c()=>return Ok(()),_=tokio::time::sleep(std::time::Duration::from_secs(2))=>{}}
            }
        }
    };
    let mut data = dispatch(&paths, request).await?;
    if let Some((targets, removed, timeout)) = waiting {
        if !cli.json {
            eprintln!(
                "{}",
                data["message"].as_str().unwrap_or("Waiting for cleanup...")
            );
        }
        ensure!(
            data["manager_running"] != false,
            "request saved, but manager is stopped; start the manager before waiting for cleanup"
        );
        wait_for_cleanup(&paths, &targets, removed, timeout).await?;
        data["pending"] = Value::Bool(false);
        data["message"] = Value::String("Cleanup complete.".into());
    }
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&data)?);
    } else if let Some(pools) = data["pools"].as_array() {
        let target_width = pools
            .iter()
            .filter_map(|p| p["target"].as_str())
            .map(str::len)
            .max()
            .unwrap_or(18)
            .max(18);
        println!(
            "{:<20} {:<target_width$} {:<10} {:>6} {:>6} {:>6} {:>6} {:>8} {:>8} {:>8}",
            "POOL",
            "SCOPE",
            "INTENT",
            "TARGET",
            "ACTIVE",
            "IDLE",
            "BUSY",
            "STARTING",
            "UNKNOWN",
            "DRAINING"
        );
        for p in pools {
            // JSON values ignore format widths; strings honor the column alignment.
            println!(
                "{:<20} {:<target_width$} {:<10} {:>6} {:>6} {:>6} {:>6} {:>8} {:>8} {:>8}",
                p["name"].as_str().unwrap_or(""),
                p["target"]
                    .as_str()
                    .or_else(|| p["organization"].as_str())
                    .unwrap_or(""),
                p["intent"].as_str().unwrap_or(""),
                p["replicas"].to_string(),
                p["active"].to_string(),
                p["idle"].to_string(),
                p["busy"].to_string(),
                p["starting"].to_string(),
                p["unknown"].to_string(),
                p["draining"].to_string()
            );
            if let Some(error) = p["error"].as_str() {
                println!("  error: {error}");
            }
            if p["draining"].as_u64().unwrap_or(0) > 0 {
                println!(
                    "  draining: idle runners can accept one final job; busy jobs finish before cleanup"
                );
            }
            if pools.len() == 1 {
                println!(
                    "  image: {} | labels: {} | group: {}",
                    p["config"]["image"].as_str().unwrap_or(""),
                    p["config"]["labels"],
                    p["config"]["runner_group"].as_str().unwrap_or("")
                );
                for r in p["runners"].as_array().unwrap() {
                    println!(
                        "  {} {}{}",
                        r["id"].as_str().unwrap(),
                        r["phase"].as_str().unwrap(),
                        if r["retiring"] == true {
                            match r["drain_reason"].as_str() {
                                Some("waiting_for_next_job") => " (draining: waiting for next job)",
                                Some("job_in_progress") => " (draining: finishing current job)",
                                _ => " (draining: awaiting cleanup)",
                            }
                        } else {
                            ""
                        }
                    );
                }
            }
        }
        if data["manager_running"] == false {
            println!(
                "Manager is stopped; showing persisted state. Run runnerctl manager or service install."
            );
        }
    } else {
        println!("{}", serde_json::to_string_pretty(&data)?);
    }
    ensure!(data["healthy"] != false, "doctor found failed checks");
    Ok(())
}
async fn wait_for_cleanup(
    paths: &Paths,
    targets: &[String],
    removed: bool,
    timeout: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let status =
            tokio::time::timeout_at(deadline, dispatch(paths, Request::Status { pool: None }))
                .await
                .context(
                    "cleanup wait timed out; intent is saved and no additional jobs were cancelled",
                )??;
        ensure!(
            status["manager_running"] != false,
            "manager stopped while waiting; cleanup is still pending"
        );
        let pools = status["pools"].as_array().context("missing pools")?;
        let done = targets.iter().all(|target| {
            pools
                .iter()
                .find(|p| p["name"] == *target)
                .is_none_or(|p| !removed && p["active"] == 0)
        });
        if done {
            return Ok(());
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "cleanup wait timed out; intent is saved and timeout does not force-cancel jobs; idle runners may need one final job"
        );
        tokio::select! {
            _ = tokio::signal::ctrl_c() => anyhow::bail!("wait interrupted; cleanup remains pending; stopping the wait does not cancel jobs"),
            _ = tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + Duration::from_secs(1))) => {}
        }
    }
}
