use crate::{
    config::{AppAuth, Auth, credential, secret},
    state::now,
};
use anyhow::{Result, anyhow, bail};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use reqwest::{Client, Method, StatusCode};
use ring::{
    rand::SystemRandom,
    signature::{RSA_PKCS1_SHA256, RsaKeyPair},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Debug, Deserialize)]
pub struct RemoteRunner {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub busy: bool,
}
#[derive(Clone)]
pub struct Github {
    client: Client,
    base: String,
    state: Arc<Mutex<Cache>>,
}
#[derive(Default)]
struct Cache {
    cache: HashMap<(String, String), (u64, Vec<RemoteRunner>)>,
    blocked: HashMap<String, (u64, String)>,
    installation_tokens: HashMap<String, (u64, String)>,
    generation: u64,
}
#[derive(Clone, Deserialize)]
pub struct RunnerRelease {
    pub tag_name: String,
    pub published_at: String,
}
pub fn app_jwt(app: &AppAuth) -> Result<String> {
    let pem = secret(&app.private_key, 65536)?;
    let der = STANDARD
        .decode(
            pem.lines()
                .filter(|line| !line.starts_with("-----"))
                .collect::<String>(),
        )
        .map_err(|_| anyhow!("invalid GitHub App RSA PEM private key"))?;
    let key = if pem.starts_with("-----BEGIN PRIVATE KEY-----") {
        RsaKeyPair::from_pkcs8(&der)
    } else if pem.starts_with("-----BEGIN RSA PRIVATE KEY-----") {
        RsaKeyPair::from_der(&der)
    } else {
        bail!("expected an unencrypted RSA PKCS#8 or PKCS#1 PEM private key");
    }
    .map_err(|_| anyhow!("invalid GitHub App RSA private key (at least 2048 bits required)"))?;
    #[derive(Serialize)]
    struct Claims<'a> {
        iat: u64,
        exp: u64,
        iss: &'a str,
    }
    let at = now();
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&Claims {
        iat: at.saturating_sub(60),
        exp: at + 540,
        iss: &app.app_id,
    })?);
    let message = format!("{header}.{claims}");
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        message.as_bytes(),
        &mut signature,
    )
    .map_err(|_| anyhow!("GitHub App JWT signing failed"))?;
    Ok(format!("{message}.{}", URL_SAFE_NO_PAD.encode(signature)))
}
fn auth_key(profile: &str, auth: &Auth) -> String {
    // Snapshots using a rotated profile cannot populate the new profile's cache.
    format!(
        "{profile}:{:x}",
        Sha256::digest(serde_json::to_vec(auth).expect("serializable authentication reference"))
    )
}
impl Github {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("runnerctl/", env!("CARGO_PKG_VERSION")))
                .build()?,
            base: "https://api.github.com".into(),
            state: Arc::new(Mutex::new(Cache::default())),
        })
    }
    pub fn clear(&self) {
        let mut state = self.state.lock().unwrap();
        let generation = state.generation.wrapping_add(1);
        *state = Cache {
            generation,
            ..Cache::default()
        };
    }
    async fn access_token(&self, profile: &str, auth: &Auth) -> Result<String> {
        let Some(app) = &auth.app else {
            return credential(auth);
        };
        let key = auth_key(profile, auth);
        let generation = self.state.lock().unwrap().generation;
        if let Some((expiry, token)) = self.state.lock().unwrap().installation_tokens.get(&key) {
            if *expiry > now() + 60 {
                return Ok(token.clone());
            }
        }
        let jwt = app_jwt(app)?;
        let response = self
            .client
            .post(format!(
                "{}/app/installations/{}/access_tokens",
                self.base, app.installation_id
            ))
            .bearer_auth(jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .map_err(|_| anyhow!("GitHub App token connection failed"))?;
        if !response.status().is_success() {
            return Err(self.response_error(
                &response,
                key,
                generation,
                "GitHub App installation token",
            ));
        }
        let data: Value = response
            .json()
            .await
            .map_err(|_| anyhow!("invalid GitHub App token response"))?;
        let token = data["token"]
            .as_str()
            .ok_or_else(|| anyhow!("GitHub App installation token missing"))?
            .to_owned();
        let expires = data["expires_at"]
            .as_str()
            .and_then(|s| humantime::parse_rfc3339(s).ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .ok_or_else(|| anyhow!("GitHub App installation token expiry missing"))?;
        let mut state = self.state.lock().unwrap();
        if state.generation == generation {
            state
                .installation_tokens
                .insert(key, (expires, token.clone()));
        }
        Ok(token)
    }
    pub async fn latest_runner_release(&self) -> Result<RunnerRelease> {
        let response = self
            .client
            .get(format!(
                "{}/repos/actions/runner/releases/latest",
                self.base
            ))
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|_| anyhow!("cannot check upstream runner release"))?;
        if !response.status().is_success() {
            bail!(
                "upstream runner release check failed ({})",
                response.status()
            );
        }
        response
            .json()
            .await
            .map_err(|_| anyhow!("invalid upstream runner release response"))
    }
    async fn request(
        &self,
        profile: &str,
        auth: &Auth,
        method: Method,
        path: &str,
    ) -> Result<Value> {
        let key = auth_key(profile, auth);
        let generation = self.state.lock().unwrap().generation;
        if let Some((until, message)) = self.state.lock().unwrap().blocked.get(&key) {
            if *until > now() {
                bail!("{message}; retry after {until}");
            }
        }
        let token = self.access_token(profile, auth).await?;
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
            return Err(self.response_error(&response, key, generation, "GitHub"));
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
    fn response_error(
        &self,
        response: &reqwest::Response,
        key: String,
        generation: u64,
        context: &str,
    ) -> anyhow::Error {
        let status = response.status();
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
        let (wait, message) =
            if status == StatusCode::TOO_MANY_REQUESTS || remaining == Some("0") || delay.is_some()
            {
                (
                    delay
                        .unwrap_or_else(|| reset.unwrap_or(now() + 60).saturating_sub(now()))
                        .max(1),
                    format!("{context} rate limit"),
                )
            } else if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                (
                    300,
                    format!("{context} authentication/permission error ({status})"),
                )
            } else {
                (30, format!("{context} API error ({status})"))
            };
        let mut state = self.state.lock().unwrap();
        if state.generation == generation {
            if status == StatusCode::UNAUTHORIZED {
                state.installation_tokens.remove(&key);
            }
            state
                .blocked
                .insert(key, (now().saturating_add(wait), message.clone()));
        }
        anyhow!("{message}")
    }
    pub async fn list(
        &self,
        profile: &str,
        auth: &Auth,
        scope: &str,
        ttl: u64,
    ) -> Result<Vec<RemoteRunner>> {
        let key = (auth_key(profile, auth), scope.to_owned());
        let generation = self.state.lock().unwrap().generation;
        if let Some((at, runners)) = self.state.lock().unwrap().cache.get(&key) {
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
                    &format!("{scope}/actions/runners?per_page=100&page={page}"),
                )
                .await?;
            let batch: Vec<RemoteRunner> =
                serde_json::from_value(data.get("runners").cloned().ok_or_else(|| {
                    anyhow!("cannot list runners; check organization/repository and permissions")
                })?)?;
            let done = batch.len() < 100;
            runners.extend(batch);
            if done {
                let mut state = self.state.lock().unwrap();
                if state.generation == generation {
                    state.cache.insert(key, (now(), runners.clone()));
                }
                return Ok(runners);
            }
        }
        bail!("GitHub pagination limit exceeded")
    }
    pub async fn token(&self, profile: &str, auth: &Auth, scope: &str) -> Result<String> {
        let data = self
            .request(
                profile,
                auth,
                Method::POST,
                &format!("{scope}/actions/runners/registration-token"),
            )
            .await?;
        data["token"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("registration token unavailable; check permissions"))
    }
    pub async fn remove(&self, profile: &str, auth: &Auth, scope: &str, id: u64) -> Result<()> {
        self.request(
            profile,
            auth,
            Method::DELETE,
            &format!("{scope}/actions/runners/{id}"),
        )
        .await?;
        self.state
            .lock()
            .unwrap()
            .cache
            .remove(&(auth_key(profile, auth), scope.into()));
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
    fn app_auth(dir: &std::path::Path) -> Auth {
        let path = dir.join("test-only-app.pem");
        crate::config::atomic_write(&path, include_bytes!("../tests/fixtures/app-test-key.pem"))
            .unwrap();
        Auth {
            credential: String::new(),
            app: Some(AppAuth {
                app_id: "12345".into(),
                installation_id: 6789,
                private_key: format!("file:{}", path.display()),
            }),
        }
    }
    fn verify_jwt(jwt: &str) {
        let (message, signature) = jwt.rsplit_once('.').unwrap();
        let signature = URL_SAFE_NO_PAD.decode(signature).unwrap();
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            include_bytes!("../tests/fixtures/app-test-public.der"),
        )
        .verify(message.as_bytes(), &signature)
        .unwrap();
        let (header, claims) = message.split_once('.').unwrap();
        let header: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header).unwrap()).unwrap();
        let claims: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).unwrap()).unwrap();
        assert_eq!(header["alg"], "RS256");
        assert_eq!(claims["iss"], "12345");
        assert!(claims["iat"].as_u64().unwrap() <= now());
        assert!(claims["exp"].as_u64().unwrap() <= now() + 600);
        assert!(claims["exp"].as_u64().unwrap() > now());
    }
    #[test]
    fn app_jwt_is_signed_and_private_key_permissions_are_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let auth = app_auth(dir.path());
        verify_jwt(&app_jwt(auth.app.as_ref().unwrap()).unwrap());
        let path = dir.path().join("test-only-app.pem");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(app_jwt(auth.app.as_ref().unwrap()).is_err());
    }
    #[tokio::test]
    async fn app_token_rate_limits_are_shared_without_exposing_response_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let g = Github::testing(format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            let body = "private-response-must-not-appear";
            reader.get_mut().write_all(format!("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 120\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let auth = app_auth(dir.path());
        let error = g
            .list("app", &auth, "/repos/example/repository", 0)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("rate limit"));
        assert!(!error.contains("private-response"));
        server.await.unwrap();
        // The server is gone: a second request must use the shared retry deadline.
        let error = g
            .list("app", &auth, "/repos/example/repository", 0)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("retry after"));
    }

    #[tokio::test]
    async fn app_installation_tokens_are_reused_refreshed_and_invalidated() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let count = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let g = Github::testing(format!("http://{}", listener.local_addr().unwrap()));
        let calls = count.clone();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let request = line.clone();
                let mut bearer = String::new();
                loop {
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if line
                        .to_ascii_lowercase()
                        .starts_with("authorization: bearer ")
                    {
                        bearer = line.trim()[22..].to_owned();
                    }
                }
                let body = if request.contains("/app/installations/6789/access_tokens") {
                    verify_jwt(&bearer);
                    let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                    json!({"token":format!("installation-token-{n}"),"expires_at":humantime::format_rfc3339(std::time::SystemTime::now() + Duration::from_secs(3600)).to_string()})
                } else {
                    assert!(bearer.starts_with("installation-token-"));
                    assert!(request.contains("/repos/example/repository/actions/runners"));
                    json!({"runners":[]})
                }.to_string();
                reader.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let auth = app_auth(dir.path());
        for _ in 0..2 {
            g.list("app", &auth, "/repos/example/repository", 0)
                .await
                .unwrap();
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
        g.state
            .lock()
            .unwrap()
            .installation_tokens
            .get_mut(&auth_key("app", &auth))
            .unwrap()
            .0 = now() + 30;
        g.list("app", &auth, "/repos/example/repository", 0)
            .await
            .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2);
        g.clear();
        g.list("app", &auth, "/repos/example/repository", 0)
            .await
            .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 3);
        server.abort();
    }
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
        let g = Github::testing(base);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        crate::config::atomic_write(&path, b"test-pat").unwrap();
        let auth = Auth {
            credential: format!("file:{}", path.display()),
            app: None,
        };
        let e = g
            .list("bad", &auth, "/orgs/denied", 0)
            .await
            .unwrap_err()
            .to_string();
        assert!(!e.contains("secret"));
        assert!(
            g.list("good", &auth, "/orgs/ok", 0)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            g.token("good", &auth, "/orgs/ok").await.unwrap(),
            "short-lived-test-token"
        );
        assert!(g.list("bad", &auth, "/orgs/ok", 0).await.is_err());
        g.remove("good", &auth, "/orgs/ok", 123).await.unwrap();
        task.abort();
    }
}
