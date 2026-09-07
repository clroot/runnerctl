use crate::config::{Config, Pool};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
#[derive(Clone, Serialize, Deserialize)]
pub struct State {
    pub manager_id: String,
    pub revision: u64,
    pub config: Config,
    pub file_hash: String,
    pub pools: BTreeMap<String, PoolState>,
    pub runs: BTreeMap<String, Run>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct PoolState {
    pub id: String,
    pub running: bool,
    pub deleting: bool,
    pub generation: u64,
    pub failures: u32,
    pub retry_at: u64,
    pub error: Option<String>,
}
impl Default for PoolState {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4().simple().to_string(),
            running: false,
            deleting: false,
            generation: 1,
            failures: 0,
            retry_at: 0,
            error: None,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub pool_id: String,
    pub pool: Pool,
    pub generation: u64,
    pub created_at: u64,
    pub phase: String,
    pub remote_id: Option<u64>,
    pub retiring: bool,
    pub force: bool,
    pub completed_job: bool,
    pub log_saved: bool,
}
impl State {
    pub fn new(config: Config, file_hash: String) -> Self {
        let pools = config
            .pools
            .iter()
            .map(|p| (p.name.clone(), PoolState::default()))
            .collect();
        Self {
            manager_id: Uuid::new_v4().simple().to_string(),
            revision: 1,
            config,
            file_hash,
            pools,
            runs: BTreeMap::new(),
        }
    }
}
pub struct Store {
    conn: rusqlite::Connection,
}
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = rusqlite::Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS state (id INTEGER PRIMARY KEY CHECK(id=1), body TEXT NOT NULL);")?;
        Ok(Self { conn })
    }
    pub fn load(&self) -> Result<Option<State>> {
        use rusqlite::OptionalExtension;
        let body: Option<String> = self
            .conn
            .query_row("SELECT body FROM state WHERE id=1", [], |r| r.get(0))
            .optional()?;
        body.map(|b| serde_json::from_str(&b).map_err(Into::into))
            .transpose()
    }
    pub fn save(&self, state: &State) -> Result<()> {
        self.conn.execute("INSERT INTO state(id,body) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET body=excluded.body", [serde_json::to_string(state)?])?;
        Ok(())
    }
}
