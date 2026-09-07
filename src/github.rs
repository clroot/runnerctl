use crate::{
    config::{Auth, credential},
    state::now,
};
use anyhow::{Result, anyhow, bail};
use reqwest::{Client, Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, time::Duration};

#[derive(Clone, Debug, Deserialize)]
pub struct RemoteRunner {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub busy: bool,
}
pub struct Github {
    client: Client,
    base: String,
    cache: HashMap<(String, String), (u64, Vec<RemoteRunner>)>,
    blocked: HashMap<String, (u64, String)>,
}
impl Github {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("runnerctl/0.1.0")
                .build()?,
            base: "https://api.github.com".into(),
            cache: HashMap::new(),
            blocked: HashMap::new(),
        })
    }
    pub fn clear(&mut self) {
        self.cache.clear();
        self.blocked.clear();
    }
    async fn request(
        &mut self,
        profile: &str,
        auth: &Auth,
        method: Method,
        path: &str,
    ) -> Result<Value> {
        if let Some((until, message)) = self.blocked.get(profile) {
            if *until > now() {
                bail!("{message}; retry after {until}");
            }
        }
        let token = credential(auth)?;
        let response = self
            .client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .map_err(|_| anyhow!("GitHub connection failed"))?;
        let status = response.status();
        if !status.is_success() && status != StatusCode::NOT_FOUND {
            let remaining = response
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok());
            let delay = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            let reset = response
                .headers()
                .get("x-ratelimit-reset")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            let (wait, msg) = if status == StatusCode::TOO_MANY_REQUESTS
                || remaining == Some("0")
                || delay.is_some()
            {
                (
                    delay
                        .unwrap_or_else(|| reset.unwrap_or(now() + 60).saturating_sub(now()))
                        .max(1),
                    "GitHub rate limit".to_owned(),
                )
            } else if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                (
                    300,
                    format!("GitHub authentication/permission error ({status})"),
                )
            } else {
                (30, format!("GitHub API error ({status})"))
            };
            self.blocked
                .insert(profile.into(), (now().saturating_add(wait), msg.clone()));
            bail!("{msg}");
        }
        if status == StatusCode::NOT_FOUND {
            return Ok(json!({"not_found": true}));
        }
        if status == StatusCode::NO_CONTENT {
            return Ok(Value::Null);
        }
        response
            .json()
            .await
            .map_err(|_| anyhow!("invalid GitHub response"))
    }
    pub async fn list(
        &mut self,
        profile: &str,
        auth: &Auth,
        org: &str,
        ttl: u64,
    ) -> Result<Vec<RemoteRunner>> {
        let key = (profile.to_owned(), org.to_owned());
        if let Some((at, runners)) = self.cache.get(&key) {
            if now().saturating_sub(*at) < ttl {
                return Ok(runners.clone());
            }
        }
        let mut runners = vec![];
        for page in 1..=10000 {
            let data = self
                .request(
                    profile,
                    auth,
                    Method::GET,
                    &format!("/orgs/{org}/actions/runners?per_page=100&page={page}"),
                )
                .await?;
            let batch: Vec<RemoteRunner> =
                serde_json::from_value(data.get("runners").cloned().ok_or_else(|| {
                    anyhow!("cannot list organization runners; check organization and permissions")
                })?)?;
            let done = batch.len() < 100;
            runners.extend(batch);
            if done {
                self.cache.insert(key, (now(), runners.clone()));
                return Ok(runners);
            }
        }
        bail!("GitHub pagination limit exceeded")
    }
    pub async fn token(&mut self, profile: &str, auth: &Auth, org: &str) -> Result<String> {
        let data = self
            .request(
                profile,
                auth,
                Method::POST,
                &format!("/orgs/{org}/actions/runners/registration-token"),
            )
            .await?;
        data["token"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("registration token unavailable; check permissions"))
    }
    pub async fn remove(&mut self, profile: &str, auth: &Auth, org: &str, id: u64) -> Result<()> {
        self.request(
            profile,
            auth,
            Method::DELETE,
            &format!("/orgs/{org}/actions/runners/{id}"),
        )
        .await?;
        self.cache.remove(&(profile.into(), org.into()));
        Ok(())
    }
}

#[cfg(test)]
impl Github {
    pub fn testing(base: String) -> Self {
        let mut g = Self::new().unwrap();
        g.base = base;
        g
    }
}
#[cfg(test)]
pub mod tests {
    use super::*;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };
    pub async fn server() -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut first = String::new();
                    reader.read_line(&mut first).await.unwrap();
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).await.unwrap();
                        if line == "\r\n" || line.is_empty() {
                            break;
                        }
                    }
                    let (status, body) = if first.contains("/orgs/denied/") {
                        (
                            "401 Unauthorized",
                            json!({"message":"secret must never be logged"}),
                        )
                    } else if first.contains("registration-token") {
                        ("201 Created", json!({"token":"short-lived-test-token"}))
                    } else if first.starts_with("DELETE") {
                        ("204 No Content", Value::Null)
                    } else {
                        ("200 OK", json!({"total_count":0,"runners":[]}))
                    };
                    let body = if body.is_null() {
                        String::new()
                    } else {
                        body.to_string()
                    };
                    let reply = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    reader.get_mut().write_all(reply.as_bytes()).await.unwrap();
                });
            }
        });
        (base, task)
    }
    #[tokio::test]
    async fn api_tokens_and_profile_isolation() {
        let (base, task) = server().await;
        let mut g = Github::testing(base);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        crate::config::atomic_write(&path, b"test-pat").unwrap();
        let auth = Auth {
            credential: format!("file:{}", path.display()),
        };
        let e = g
            .list("bad", &auth, "denied", 0)
            .await
            .unwrap_err()
            .to_string();
        assert!(!e.contains("secret"));
        assert!(g.list("good", &auth, "ok", 0).await.unwrap().is_empty());
        assert_eq!(
            g.token("good", &auth, "ok").await.unwrap(),
            "short-lived-test-token"
        );
        assert!(g.list("bad", &auth, "ok", 0).await.is_err());
        g.remove("good", &auth, "ok", 123).await.unwrap();
        task.abort();
    }
}
