use crate::config::Pool;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    Status {
        pool: Option<String>,
    },
    Add {
        pool: Pool,
    },
    Start {
        pool: Option<String>,
        all: bool,
    },
    Stop {
        pool: Option<String>,
        all: bool,
        force: bool,
    },
    Scale {
        pool: String,
        replicas: u32,
    },
    Remove {
        pool: String,
        force: bool,
    },
    Upgrade {
        pool: String,
        image: String,
    },
    Apply {
        text: String,
        revision: u64,
    },
    Auth {
        profile: String,
        credential: String,
    },
    Logs {
        pool: String,
        id: String,
    },
    Doctor,
}
impl Request {
    pub fn changes_config(&self) -> bool {
        matches!(
            self,
            Self::Add { .. } | Self::Scale { .. } | Self::Upgrade { .. } | Self::Auth { .. }
        )
    }
}
#[derive(Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub data: Value,
}
