mod config;
mod docker;
mod github;
mod manager;
mod protocol;
mod service;
mod state;

use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand};
use config::{Config, Paths, Pool};
use protocol::Request;
use serde_json::Value;
use std::{fs, path::PathBuf};

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
        #[arg(long)]
        org: String,
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
}
#[derive(Subcommand)]
enum PoolCmd {
    Add {
        name: String,
        #[arg(long)]
        org: String,
        #[arg(long)]
        auth: String,
        #[arg(long, value_delimiter = ',', required = true)]
        labels: Vec<String>,
        #[arg(long, default_value_t = 1)]
        replicas: u32,
        #[arg(long, default_value = "Default")]
        runner_group: String,
        #[arg(long, default_value = "local/runnerctl-runner:2.337.0")]
        image: String,
        #[arg(long)]
        docker_socket: bool,
    },
    List,
    Show {
        name: String,
    },
    Remove {
        name: String,
        #[arg(long)]
        force: bool,
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
    let request = match cli.command {
        Cmd::Init { org } => {
            let _lock = manager::lock(&paths)?;
            ensure!(
                !paths.config().exists() && !paths.home.join("state.sqlite").exists(),
                "already initialized"
            );
            ensure!(
                config::valid_name(&org) && !org.contains('_') && org.len() <= 39,
                "invalid organization"
            );
            let mut config = Config::default();
            config.auth.insert(
                org.clone(),
                config::Auth {
                    credential: format!(
                        "file:{}",
                        paths.home.join("credentials").join(&org).display()
                    ),
                },
            );
            config::atomic_write(&paths.config(), toml::to_string_pretty(&config)?.as_bytes())?;
            println!(
                "Initialized {}\nNext: runnerctl auth login --profile {org}\nThen: runnerctl pool add validate --org {org} --auth {org} --labels validate --replicas 4",
                paths.config().display()
            );
            return Ok(());
        }
        Cmd::Manager => return manager::serve(paths).await,
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
            }
        }
        Cmd::Pool { command } => match command {
            PoolCmd::Add {
                name,
                org,
                auth,
                labels,
                replicas,
                runner_group,
                image,
                docker_socket,
            } => Request::Add {
                pool: Pool {
                    name,
                    organization: org,
                    auth,
                    labels,
                    replicas,
                    runner_group,
                    image,
                    docker_socket,
                },
            },
            PoolCmd::List => Request::Status { pool: None },
            PoolCmd::Show { name } => Request::Status { pool: Some(name) },
            PoolCmd::Remove { name, force } => Request::Remove { pool: name, force },
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
        } => Request::Stop { pool, all, force },
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
    let data = dispatch(&paths, request).await?;
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&data)?);
    } else if let Some(pools) = data["pools"].as_array() {
        println!(
            "{:<20} {:<18} {:<10} {:>6} {:>6} {:>6} {:>6} {:>8} {:>8} {:>8}",
            "POOL",
            "ORGANIZATION",
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
            println!(
                "{:<20} {:<18} {:<10} {:>6} {:>6} {:>6} {:>6} {:>8} {:>8} {:>8}",
                p["name"].as_str().unwrap_or(""),
                p["organization"].as_str().unwrap_or(""),
                p["intent"].as_str().unwrap_or(""),
                p["replicas"],
                p["active"],
                p["idle"],
                p["busy"],
                p["starting"],
                p["unknown"],
                p["draining"]
            );
            if let Some(error) = p["error"].as_str() {
                println!("  error: {error}");
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
                            " (draining)"
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
