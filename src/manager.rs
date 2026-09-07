use crate::{
    config::{self, Config, Paths},
    docker::Docker,
    github::Github,
    protocol::{Request, Response},
    state::{PoolState, Run, State, Store, now},
};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};
use uuid::Uuid;

pub fn hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}
pub fn lock(paths: &Paths) -> Result<fs::File> {
    let f = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(paths.home.join("manager.lock"))?;
    f.try_lock_exclusive()
        .context("manager is already running or another command holds the lock")?;
    Ok(f)
}
pub struct Engine {
    pub state: State,
    store: Store,
    paths: Paths,
    github: Github,
    cursor: usize,
}
impl Engine {
    pub fn open(paths: Paths) -> Result<Self> {
        let store = Store::open(&paths.home.join("state.sqlite"))?;
        let state = match store.load()? {
            Some(state) => state,
            None => {
                let text =
                    fs::read_to_string(paths.config()).context("run runnerctl init first")?;
                let state = State::new(Config::parse(&text)?, hash(&text));
                store.save(&state)?;
                state
            }
        };
        Ok(Self {
            state,
            store,
            paths,
            github: Github::new()?,
            cursor: 0,
        })
    }
    fn save(&self) -> Result<()> {
        self.store.save(&self.state)
    }
    fn clean_file(&self) -> Result<()> {
        ensure!(
            hash(&fs::read_to_string(self.paths.config())?) == self.state.file_hash,
            "config.toml has unapplied changes; run config apply first"
        );
        Ok(())
    }
    fn commit_config(&mut self, next: Config) -> Result<()> {
        next.validate()?;
        let text = toml::to_string_pretty(&next)?;
        let mut state = self.state.clone();
        state.config = next;
        state.revision += 1;
        state.file_hash = hash(&text);
        // SQLite is authoritative. If interrupted before file write, config apply can
        // recover explicitly; startup never silently overwrites user-edited TOML.
        self.store.save(&state)?;
        self.state = state;
        config::atomic_write(&self.paths.config(), text.as_bytes())?;
        self.github.clear();
        Ok(())
    }
    fn targets(&self, pool: Option<String>, all: bool) -> Result<Vec<String>> {
        ensure!(pool.is_some() != all, "specify a pool or --all");
        if let Some(pool) = pool {
            ensure!(self.state.pools.contains_key(&pool), "unknown pool: {pool}");
            Ok(vec![pool])
        } else {
            Ok(self.state.pools.keys().cloned().collect())
        }
    }
    pub async fn handle(&mut self, request: Request) -> Result<Value> {
        let result = self.handle_inner(request).await;
        if result.is_err() {
            // A failed mutation must not leave uncommitted changes in memory.
            self.state = self.store.load()?.context("state disappeared")?;
        }
        result
    }
    async fn handle_inner(&mut self, request: Request) -> Result<Value> {
        if request.changes_config() {
            self.clean_file()?;
        }
        match request {
            Request::Status { pool } => {
                if let Some(p) = &pool {
                    ensure!(self.state.pools.contains_key(p), "unknown pool: {p}");
                }
                let pools: Vec<Value> = self.state.config.pools.iter().filter(|p| pool.as_ref().is_none_or(|n| n == &p.name)).map(|p| {
                    let state = &self.state.pools[&p.name];
                    let runs: Vec<&Run> = self.state.runs.values().filter(|r| r.pool_id == state.id).collect();
                    let count = |phase: &str| runs.iter().filter(|r| r.phase == phase).count();
                    json!({"name":p.name,"organization":p.organization,"replicas":p.replicas,"intent":if state.deleting {"deleting"} else if state.running {"running"} else {"stopped"},"active":runs.len(),"starting":count("creating")+count("starting"),"idle":count("idle"),"busy":count("busy"),"unknown":count("unknown"),"draining":runs.iter().filter(|r|r.retiring).count(),"generation":state.generation,"error":state.error,"retry_at":state.retry_at,"config":p,"runners":runs.iter().map(|r|json!({"id":r.id,"phase":r.phase,"retiring":r.retiring,"generation":r.generation})).collect::<Vec<_>>()})
                }).collect();
                Ok(json!({"revision":self.state.revision,"pools":pools}))
            }
            Request::Add { pool } => {
                ensure!(
                    !self.state.pools.contains_key(&pool.name),
                    "pool already exists"
                );
                let mut next = self.state.config.clone();
                next.pools.push(pool.clone());
                next.validate()?;
                self.state
                    .pools
                    .insert(pool.name.clone(), PoolState::default());
                self.commit_config(next)?;
                Ok(json!({"added":pool.name,"intent":"stopped"}))
            }
            Request::Start { pool, all } => {
                let targets = self.targets(pool, all)?;
                ensure!(
                    targets.iter().all(|n| !self.state.pools[n].deleting),
                    "a target pool is being deleted"
                );
                for name in &targets {
                    let p = self.state.pools.get_mut(name).unwrap();
                    p.running = true;
                    p.retry_at = 0;
                    p.failures = 0;
                    p.error = None;
                }
                self.github.clear();
                self.save()?;
                Ok(json!({"started":targets}))
            }
            Request::Stop { pool, all, force } => {
                let targets = self.targets(pool, all)?;
                for name in &targets {
                    let p = self.state.pools.get_mut(name).unwrap();
                    p.running = false;
                    for r in self.state.runs.values_mut().filter(|r| r.pool_id == p.id) {
                        r.retiring = true;
                        r.force |= force;
                    }
                }
                self.save()?;
                Ok(
                    json!({"stopped":targets,"pending":true,"message":"No replacement runners; existing runners drain after their job. Idle runners may wait for a job. Use --force only to cancel work."}),
                )
            }
            Request::Scale { pool, replicas } => {
                let mut next = self.state.config.clone();
                let p = next
                    .pools
                    .iter_mut()
                    .find(|p| p.name == pool)
                    .context("unknown pool")?;
                ensure!(!self.state.pools[&pool].deleting, "pool is being deleted");
                p.replicas = replicas;
                self.commit_config(next)?;
                Ok(json!({"pool":pool,"replicas":replicas}))
            }
            Request::Upgrade { pool, image } => {
                let mut next = self.state.config.clone();
                let p = next
                    .pools
                    .iter_mut()
                    .find(|p| p.name == pool)
                    .context("unknown pool")?;
                ensure!(!self.state.pools[&pool].deleting, "pool is being deleted");
                p.image = image;
                next.validate()?;
                Docker::image(&p_image(&next, &pool)).await?;
                self.state.pools.get_mut(&pool).unwrap().generation += 1;
                self.commit_config(next)?;
                Ok(json!({"upgrading":pool}))
            }
            Request::Remove { pool, force } => {
                let p = self.state.pools.get_mut(&pool).context("unknown pool")?;
                p.running = false;
                p.deleting = true;
                for r in self.state.runs.values_mut().filter(|r| r.pool_id == p.id) {
                    r.retiring = true;
                    r.force |= force;
                }
                self.save()?;
                Ok(json!({"removing":pool,"pending":true}))
            }
            Request::Auth {
                profile,
                credential,
            } => {
                ensure!(config::valid_name(&profile), "invalid profile");
                let mut next = self.state.config.clone();
                next.auth
                    .insert(profile.clone(), config::Auth { credential });
                self.commit_config(next)?;
                for (name, p) in &mut self.state.pools {
                    if self
                        .state
                        .config
                        .pools
                        .iter()
                        .any(|c| c.name == *name && c.auth == profile)
                    {
                        p.retry_at = 0;
                        p.error = None;
                        p.failures = 0;
                    }
                }
                self.save()?;
                Ok(json!({"profile":profile}))
            }
            Request::Apply { text, revision } => {
                ensure!(
                    self.state.revision == revision,
                    "stale revision; read status and retry"
                );
                ensure!(
                    fs::read_to_string(self.paths.config())? == text,
                    "config changed while applying; retry"
                );
                let next = Config::parse(&text)?;
                ensure!(
                    self.state
                        .config
                        .pools
                        .iter()
                        .all(|p| next.pools.iter().any(|n| n.name == p.name)),
                    "use pool remove to drain and delete pools before removing configuration"
                );
                let mut pool_states = self.state.pools.clone();
                for new in &next.pools {
                    if let Some(old) = self.state.config.pools.iter().find(|p| p.name == new.name) {
                        let ps = pool_states.get_mut(&new.name).unwrap();
                        ensure!(!ps.deleting, "cannot edit a deleting pool");
                        if old.organization != new.organization || old.auth != new.auth {
                            ensure!(
                                !ps.running
                                    && !self.state.runs.values().any(|r| r.pool_id == ps.id),
                                "stop and fully clean pool before changing organization/auth"
                            );
                        }
                        if old.image != new.image
                            || old.labels != new.labels
                            || old.runner_group != new.runner_group
                            || old.docker_socket != new.docker_socket
                        {
                            ps.generation += 1;
                        }
                    } else {
                        pool_states.insert(new.name.clone(), PoolState::default());
                    }
                }
                for run in self.state.runs.values() {
                    ensure!(
                        next.auth.contains_key(&run.pool.auth),
                        "auth profile still required for cleanup"
                    );
                }
                self.state.pools = pool_states;
                self.commit_config(next)?;
                Ok(json!({"revision":self.state.revision,"applied":true}))
            }
            Request::Logs { pool, id } => {
                let ps = self.state.pools.get(&pool).context("unknown pool")?;
                let run = self
                    .state
                    .runs
                    .get(&id)
                    .context("runner not active; archived logs are in the logs directory")?;
                ensure!(run.pool_id == ps.id, "runner does not belong to pool");
                let containers = Docker::list(&self.state.manager_id).await?;
                ensure!(
                    containers
                        .iter()
                        .any(|c| Docker::owned(c, &self.state.manager_id, run)),
                    "owned container not found"
                );
                Ok(json!({"logs":Docker::logs(&id).await?}))
            }
            Request::Doctor => {
                let mut checks = vec![
                    json!({"check":"config", "ok":true, "warnings":self.state.config.warnings()}),
                ];
                let docker = Docker::info().await;
                checks.push(json!({"check":"docker", "ok":docker.is_ok(), "error":docker.err().map(|e|e.to_string())}));
                for p in self.state.config.pools.clone() {
                    let auth = &self.state.config.auth[&p.auth];
                    let gh = self.github.list(&p.auth, auth, &p.organization, 0).await;
                    checks.push(json!({"check":format!("github:{}",p.name),"ok":gh.is_ok(),"error":gh.err().map(|e|e.to_string())}));
                    let image = Docker::image(&p.image).await;
                    checks.push(json!({"check":format!("image:{}",p.name),"ok":image.is_ok(),"error":image.err().map(|e|e.to_string())}));
                }
                Ok(json!({"healthy":checks.iter().all(|c|c["ok"]==true),"checks":checks}))
            }
        }
    }
    pub async fn tick(&mut self) -> Result<()> {
        let names: Vec<String> = self.state.pools.keys().cloned().collect();
        if names.is_empty() {
            return Ok(());
        }
        let name = names[self.cursor % names.len()].clone();
        self.cursor = self.cursor.wrapping_add(1);
        if self.state.pools[&name].retry_at > now() {
            return Ok(());
        }
        let result = self.reconcile(&name).await;
        if let Err(error) = result {
            let p = self
                .state
                .pools
                .get_mut(&name)
                .context("pool disappeared")?;
            p.failures = p.failures.saturating_add(1);
            p.retry_at = now()
                + (5 * 2u64.pow(p.failures.min(7))).min(600)
                + u64::from(Uuid::new_v4().as_bytes()[0] % 7);
            p.error = Some(error.to_string());
            // Last known busy/idle is not evidence after an observation failure.
            for r in self.state.runs.values_mut().filter(|r| {
                r.pool_id == p.id && matches!(r.phase.as_str(), "idle" | "busy" | "starting")
            }) {
                r.phase = "unknown".into();
            }
        }
        self.save()?;
        Ok(())
    }
    async fn reconcile(&mut self, name: &str) -> Result<()> {
        let pool = self
            .state
            .config
            .pools
            .iter()
            .find(|p| p.name == name)
            .context("missing pool config")?
            .clone();
        let ps = self.state.pools[name].clone();
        let containers = Docker::list(&self.state.manager_id).await?;
        // An owned but unrecorded container counts against capacity and is never deleted blindly.
        let unknown_containers = containers
            .iter()
            .filter(|c| {
                c["Config"]["Labels"]["io.runnerctl.run"]
                    .as_str()
                    .is_none_or(|id| !self.state.runs.contains_key(id))
            })
            .count();
        let no_runs = !self.state.runs.values().any(|r| r.pool_id == ps.id);
        if no_runs && !ps.running {
            if ps.deleting {
                self.clean_file()?;
                let mut next = self.state.config.clone();
                next.pools.retain(|p| p.name != name);
                self.state.pools.remove(name);
                self.commit_config(next)?;
            }
            return Ok(());
        }
        // Explicit force can stop local work even when the GitHub credential is revoked.
        let forced: Vec<Run> = self
            .state
            .runs
            .values()
            .filter(|r| r.pool_id == ps.id && r.force)
            .cloned()
            .collect();
        for mut run in forced {
            if let Some(container) = containers
                .iter()
                .find(|c| Docker::owned(c, &self.state.manager_id, &run))
            {
                run.phase = "cleanup".into();
                self.state.runs.insert(run.id.clone(), run.clone());
                self.save()?;
                self.archive(&mut run, container).await?;
                Docker::remove(&run.id, true).await?;
            }
        }
        let containers = if self
            .state
            .runs
            .values()
            .any(|r| r.pool_id == ps.id && r.force)
        {
            Docker::list(&self.state.manager_id).await?
        } else {
            containers
        };
        let auth = self.state.config.auth[&pool.auth].clone();
        let remotes = self
            .github
            .list(
                &pool.auth,
                &auth,
                &pool.organization,
                self.state.config.manager.poll_interval_seconds,
            )
            .await?;
        let ids: Vec<String> = self
            .state
            .runs
            .values()
            .filter(|r| r.pool_id == ps.id)
            .map(|r| r.id.clone())
            .collect();
        let keep = if ps.running && !ps.deleting {
            pool.replicas as usize
        } else {
            0
        };
        // Keep the newest generation first when shrinking, but never cancel a job.
        let mut order = ids.clone();
        order.sort_by_key(|id| {
            std::cmp::Reverse((
                self.state.runs[id].generation,
                self.state.runs[id].created_at,
            ))
        });
        for (index, id) in order.iter().enumerate() {
            if index >= keep || self.state.runs[id].generation != ps.generation {
                self.state.runs.get_mut(id).unwrap().retiring = true;
            }
        }
        self.save()?;
        for id in ids {
            let mut run = self.state.runs[&id].clone();
            let remote = remotes.iter().find(|r| r.name == id);
            if let Some(remote) = remote {
                run.remote_id = Some(remote.id);
            }
            let container = containers
                .iter()
                .find(|c| Docker::owned(c, &self.state.manager_id, &run));
            let status = container.and_then(|c| c["State"]["Status"].as_str());
            if status == Some("running") || status == Some("restarting") || status == Some("paused")
            {
                run.phase = match remote {
                    Some(r) if r.busy => "busy",
                    Some(r) if r.status == "online" => "idle",
                    _ => "unknown",
                }
                .into();
                // Remove short-lived bootstrap secret after registration is observed.
                if remote.is_some() {
                    let _ = fs::remove_file(self.paths.home.join("runs").join(&id).join("token"));
                }
                self.state.runs.insert(id, run);
                continue;
            } else if status == Some("created") && !run.retiring {
                // Token might have expired during manager downtime. Refresh before start.
                let token = self
                    .github
                    .token(&pool.auth, &auth, &pool.organization)
                    .await?;
                config::atomic_write(
                    &self.paths.home.join("runs").join(&id).join("token"),
                    token.as_bytes(),
                )?;
                Docker::start(&id).await?;
                run.phase = "starting".into();
                self.state.runs.insert(id, run);
                self.save()?;
                continue;
            } else if container.is_none() && run.phase == "creating" && !run.retiring {
                // Generation was saved before Docker create; deterministic name recovers retries.
                Docker::image(&run.pool.image).await?;
                let dir = self.paths.home.join("runs").join(&id);
                config::secure_dir(&dir)?;
                let token = self
                    .github
                    .token(&pool.auth, &auth, &pool.organization)
                    .await?;
                config::atomic_write(&dir.join("token"), token.as_bytes())?;
                Docker::create(&self.state.manager_id, &run, &self.paths).await?;
                Docker::start(&id).await?;
                run.phase = "starting".into();
                self.state.runs.insert(id, run);
                self.save()?;
                continue;
            } else if let Some(container) = container {
                run.completed_job = self
                    .paths
                    .home
                    .join("runs")
                    .join(&id)
                    .join("completed")
                    .exists();
                run.phase = "cleanup".into();
                self.state.runs.insert(id.clone(), run.clone());
                self.save()?;
                self.archive(&mut run, container).await?;
                Docker::remove(&id, false).await?;
            }
            // No live container remains. Fresh lookup avoids leaving a registration that
            // appeared after the cached list (or a lost response from config.sh).
            let fresh = self
                .github
                .list(&pool.auth, &auth, &pool.organization, 0)
                .await?;
            if let Some(remote) = fresh.iter().find(|r| r.name == id) {
                // No live local worker exists, so stale busy state cannot represent running work here.
                self.github
                    .remove(&pool.auth, &auth, &pool.organization, remote.id)
                    .await?;
            }
            if run.completed_job {
                self.state.pools.get_mut(name).unwrap().failures = 0;
            }
            let failed = !run.completed_job && !run.retiring && !run.force;
            self.state.runs.remove(&id);
            self.save()?;
            let _ = fs::remove_dir_all(self.paths.home.join("runs").join(&id));
            if failed {
                bail!("runner exited before completing a job; inspect archived diagnostics");
            }
        }
        let active = self
            .state
            .runs
            .values()
            .filter(|r| r.pool_id == ps.id)
            .count();
        if ps.deleting && active == 0 {
            self.clean_file()?;
            let mut next = self.state.config.clone();
            next.pools.retain(|p| p.name != name);
            self.state.pools.remove(name);
            self.commit_config(next)?;
            return Ok(());
        }
        let total = self.state.runs.len() + unknown_containers;
        if active < keep && total < self.state.config.manager.max_runners as usize {
            // Only one create per pool turn; the single writer is below max_parallel_creates.
            Docker::image(&pool.image).await?;
            let id = format!(
                "rc-{}-{}",
                &self.state.manager_id[..8],
                Uuid::new_v4().simple()
            );
            let run = Run {
                id: id.clone(),
                pool_id: ps.id,
                pool,
                generation: ps.generation,
                created_at: now(),
                phase: "creating".into(),
                remote_id: None,
                retiring: false,
                force: false,
                completed_job: false,
                log_saved: false,
            };
            self.state.runs.insert(id, run);
            self.save()?;
        }
        if let Some(p) = self.state.pools.get_mut(name) {
            p.error = None;
            p.retry_at = 0;
        }
        self.expire_logs()?;
        Ok(())
    }
    async fn archive(&mut self, run: &mut Run, container: &Value) -> Result<()> {
        if run.log_saved {
            return Ok(());
        }
        let logs = Docker::logs(&run.id).await?;
        config::atomic_write(
            &self.paths.home.join("logs").join(format!("{}.log", run.id)),
            logs.as_bytes(),
        )?;
        // Never-started containers do not have diagnostic files.
        if container["State"]["StartedAt"]
            .as_str()
            .is_some_and(|s| !s.starts_with("0001-"))
        {
            Docker::archive(&run.id, &self.paths).await?;
        }
        run.log_saved = true;
        self.state.runs.insert(run.id.clone(), run.clone());
        self.save()?;
        Ok(())
    }
    fn expire_logs(&self) -> Result<()> {
        let age = Duration::from_secs(
            self.state
                .config
                .manager
                .log_retention_days
                .saturating_mul(86400),
        );
        for entry in fs::read_dir(self.paths.home.join("logs"))? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_file() && meta.modified()?.elapsed().unwrap_or_default() > age {
                let filename = entry.file_name().to_string_lossy().into_owned();
                if !self.state.runs.keys().any(|id| filename.starts_with(id)) {
                    fs::remove_file(entry.path())?;
                }
            }
        }
        Ok(())
    }
}
fn p_image(config: &Config, name: &str) -> String {
    config
        .pools
        .iter()
        .find(|p| p.name == name)
        .unwrap()
        .image
        .clone()
}

pub async fn serve(paths: Paths) -> Result<()> {
    paths.prepare()?;
    let _lock = lock(&paths)?;
    let mut engine = Engine::open(paths.clone())?;
    if paths.socket().exists() {
        fs::remove_file(paths.socket())?;
    }
    let listener = UnixListener::bind(paths.socket())?;
    fs::set_permissions(paths.socket(), fs::Permissions::from_mode(0o600))?;
    eprintln!(
        "runnerctl manager listening at {}",
        paths.socket().display()
    );
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            _ = term.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
            result = listener.accept() => {
                let (mut stream,_) = result?;
                let request = tokio::time::timeout(Duration::from_secs(5),read_request(&mut stream)).await;
                let result = match request { Ok(Ok(request)) => engine.handle(request).await, Ok(Err(e)) => Err(e), Err(_) => Err(anyhow::anyhow!("request timeout")) };
                let response = match result { Ok(data) => Response{ok:true,data}, Err(error) => Response{ok:false,data:json!({"error":error.to_string()})} };
                let mut bytes=serde_json::to_vec(&response)?; bytes.push(b'\n');
                let _=tokio::time::timeout(Duration::from_secs(5),stream.write_all(&bytes)).await;
            },
            _ = interval.tick() => { if let Err(e)=engine.tick().await { eprintln!("state persistence error: {e}"); return Err(e); } }
        }
    }
    fs::remove_file(paths.socket())?;
    Ok(())
}
async fn read_request(stream: &mut UnixStream) -> Result<Request> {
    use tokio::io::AsyncReadExt;
    let mut line = String::new();
    BufReader::new(stream.take(1024 * 1024 + 1))
        .read_line(&mut line)
        .await?;
    ensure!(
        line.len() <= 1024 * 1024 && line.ends_with('\n'),
        "request too large or incomplete"
    );
    Ok(serde_json::from_str(&line)?)
}
pub async fn send(paths: &Paths, request: &Request) -> Result<Value> {
    let mut stream = UnixStream::connect(paths.socket())
        .await
        .context("manager is not running; run runnerctl manager or service install")?;
    let mut bytes = serde_json::to_vec(request)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    let mut line = String::new();
    tokio::time::timeout(
        Duration::from_secs(300),
        BufReader::new(stream).read_line(&mut line),
    )
    .await
    .context("manager request timed out; check status before retrying changes")??;
    let response: Response = serde_json::from_str(&line)?;
    ensure!(
        response.ok,
        "{}",
        response.data["error"]
            .as_str()
            .unwrap_or("manager request failed")
    );
    Ok(response.data)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Engine) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().to_owned()).unwrap();
        paths.prepare().unwrap();
        let mut c = Config::default();
        c.auth.insert(
            "team".into(),
            config::Auth {
                credential: "env:TEST_TOKEN".into(),
            },
        );
        for (name, replicas) in [("validate", 4), ("build", 2)] {
            c.pools.push(config::Pool {
                name: name.into(),
                organization: "test-org".into(),
                auth: "team".into(),
                labels: vec![name.into()],
                runner_group: "Default".into(),
                replicas,
                image: config::default_image(),
                docker_socket: false,
            });
        }
        config::atomic_write(&paths.config(), toml::to_string(&c).unwrap().as_bytes()).unwrap();
        let e = Engine::open(paths).unwrap();
        (dir, e)
    }
    #[tokio::test]
    async fn pool_stop_scale_and_restart_are_independent() {
        let (_dir, mut e) = fixture();
        e.handle(Request::Start {
            pool: None,
            all: true,
        })
        .await
        .unwrap();
        e.handle(Request::Stop {
            pool: Some("build".into()),
            all: false,
            force: false,
        })
        .await
        .unwrap();
        e.handle(Request::Scale {
            pool: "build".into(),
            replicas: 3,
        })
        .await
        .unwrap();
        let paths = e.paths.clone();
        drop(e);
        let e = Engine::open(paths).unwrap();
        assert!(e.state.pools["validate"].running);
        assert!(!e.state.pools["build"].running);
        assert_eq!(e.state.config.pools[1].replicas, 3);
    }
    #[tokio::test]
    async fn rejected_changes_preserve_state_and_file() {
        let (_dir, mut e) = fixture();
        let previous = fs::read_to_string(e.paths.config()).unwrap();
        assert!(
            e.handle(Request::Scale {
                pool: "build".into(),
                replicas: 100
            })
            .await
            .is_err()
        );
        assert_eq!(e.state.config.pools[1].replicas, 2);
        assert_eq!(fs::read_to_string(e.paths.config()).unwrap(), previous);
        assert!(
            e.handle(Request::Start {
                pool: None,
                all: false
            })
            .await
            .is_err()
        );
        assert!(e.state.pools.values().all(|p| !p.running));
    }
    #[tokio::test]
    async fn manual_edits_and_revision_conflicts_are_preserved() {
        let (_dir, mut e) = fixture();
        let mut c = e.state.config.clone();
        c.pools[0].replicas = 5;
        let text = toml::to_string(&c).unwrap();
        config::atomic_write(&e.paths.config(), text.as_bytes()).unwrap();
        assert!(
            e.handle(Request::Scale {
                pool: "build".into(),
                replicas: 1
            })
            .await
            .is_err()
        );
        assert!(
            e.handle(Request::Apply {
                text: text.clone(),
                revision: 0
            })
            .await
            .is_err()
        );
        e.handle(Request::Apply { text, revision: 1 })
            .await
            .unwrap();
        assert_eq!(e.state.config.pools[0].replicas, 5);
    }
    #[tokio::test]
    async fn organization_change_requires_fully_stopped_pool() {
        let (_dir, mut e) = fixture();
        e.handle(Request::Start {
            pool: Some("build".into()),
            all: false,
        })
        .await
        .unwrap();
        let mut c = e.state.config.clone();
        c.pools[1].organization = "different".into();
        let text = toml::to_string(&c).unwrap();
        config::atomic_write(&e.paths.config(), text.as_bytes()).unwrap();
        assert!(
            e.handle(Request::Apply { text, revision: 1 })
                .await
                .is_err()
        );
        assert_eq!(e.state.config.pools[1].organization, "test-org");
    }
    #[test]
    fn manager_lock_is_exclusive() {
        let (_dir, e) = fixture();
        let _lock = lock(&e.paths).unwrap();
        assert!(lock(&e.paths).is_err());
    }
    #[test]
    fn ownership_requires_manager_pool_and_run_ids() {
        let (_dir, e) = fixture();
        let run = Run {
            id: "test".into(),
            pool_id: e.state.pools["build"].id.clone(),
            pool: e.state.config.pools[1].clone(),
            generation: 1,
            created_at: now(),
            phase: "busy".into(),
            remote_id: None,
            retiring: false,
            force: false,
            completed_job: false,
            log_saved: false,
        };
        let mut c = json!({"Config":{"Labels":{"io.runnerctl.manager":e.state.manager_id,"io.runnerctl.pool":run.pool_id,"io.runnerctl.run":run.id}}});
        assert!(Docker::owned(&c, &e.state.manager_id, &run));
        c["Config"]["Labels"]["io.runnerctl.pool"] = json!("another");
        assert!(!Docker::owned(&c, &e.state.manager_id, &run));
    }
    #[tokio::test]
    #[ignore = "requires Docker and the local/runnerctl-test:1 image; see tests/README.md"]
    async fn docker_replacement_and_restart_recovery() {
        let (_dir, mut e) = fixture();
        let (base, server) = crate::github::tests::server().await;
        let path = e.paths.home.join("credentials/test");
        config::atomic_write(&path, b"test-pat").unwrap();
        e.state.config.auth.get_mut("team").unwrap().credential =
            format!("file:{}", path.display());
        for pool in &mut e.state.config.pools {
            pool.image = "local/runnerctl-test:1".into();
            pool.replicas = 1;
        }
        e.github = Github::testing(base.clone());
        e.handle(Request::Start {
            pool: None,
            all: true,
        })
        .await
        .unwrap();
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if let Ok(out) = std::process::Command::new("docker")
                    .args([
                        "ps",
                        "-aq",
                        "--filter",
                        &format!("label=io.runnerctl.manager={}", self.0),
                    ])
                    .output()
                {
                    for id in String::from_utf8_lossy(&out.stdout).split_whitespace() {
                        let _ = std::process::Command::new("docker")
                            .args(["rm", "-f", id])
                            .output();
                    }
                }
            }
        }
        let _cleanup = Cleanup(e.state.manager_id.clone());
        for _ in 0..4 {
            e.tick().await.unwrap();
        }
        assert_eq!(
            e.state.runs.len(),
            2,
            "{:?}",
            e.state.pools.values().map(|p| &p.error).collect::<Vec<_>>()
        );
        let initial: Vec<String> = e.state.runs.keys().cloned().collect();
        assert!(e.state.runs.values().all(|r| r.phase == "starting"));
        e.handle(Request::Stop {
            pool: Some("build".into()),
            all: false,
            force: false,
        })
        .await
        .unwrap();
        let paths = e.paths.clone();
        drop(e);
        let mut e = Engine::open(paths).unwrap();
        e.github = Github::testing(base);
        tokio::time::sleep(Duration::from_secs(3)).await;
        for _ in 0..4 {
            e.tick().await.unwrap();
        }
        assert!(e.state.runs.values().all(|r| r.pool.name == "validate"));
        assert!(e.state.runs.keys().all(|id| !initial.contains(id)));
        assert!(e.state.pools["validate"].error.is_none());
        assert!(!e.state.pools["build"].running);
        server.abort();
    }
}
