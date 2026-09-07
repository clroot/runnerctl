use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    #[serde(default)]
    pub auth: BTreeMap<String, Auth>,
    #[serde(default)]
    pub manager: Manager,
    #[serde(default)]
    pub pools: Vec<Pool>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    pub credential: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Manager {
    pub poll_interval_seconds: u64,
    pub max_runners: u32,
    pub max_parallel_creates: u32,
    pub log_retention_days: u64,
}
impl Default for Manager {
    fn default() -> Self {
        Self {
            poll_interval_seconds: 30,
            max_runners: 16,
            max_parallel_creates: 2,
            log_retention_days: 7,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Pool {
    pub name: String,
    pub organization: String,
    pub auth: String,
    pub labels: Vec<String>,
    #[serde(default = "default_group")]
    pub runner_group: String,
    pub replicas: u32,
    #[serde(default = "default_image")]
    pub image: String,
    #[serde(default)]
    pub docker_socket: bool,
}
fn default_group() -> String {
    "Default".into()
}
pub fn default_image() -> String {
    "local/runnerctl-runner:2.337.0".into()
}
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 48
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && s.as_bytes()[0].is_ascii_alphanumeric()
}
impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: 1,
            auth: BTreeMap::new(),
            manager: Manager::default(),
            pools: vec![],
        }
    }
}
impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        let c: Self = toml::from_str(text).context("invalid config TOML")?;
        c.validate()?;
        Ok(c)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema_version == 1, "unsupported schema_version");
        ensure!(
            (5..=3600).contains(&self.manager.poll_interval_seconds),
            "poll interval must be 5..3600 seconds"
        );
        ensure!(
            self.manager.max_runners > 0
                && self.manager.max_parallel_creates > 0
                && self.manager.max_parallel_creates <= self.manager.max_runners,
            "invalid manager capacity"
        );
        let mut names = BTreeSet::new();
        for (name, auth) in &self.auth {
            ensure!(valid_name(name), "invalid auth profile name");
            if let Some(p) = auth.credential.strip_prefix("file:") {
                ensure!(
                    Path::new(p).is_absolute(),
                    "credential file must be absolute"
                );
            } else if let Some(e) = auth.credential.strip_prefix("env:") {
                ensure!(
                    !e.is_empty() && e.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                    "invalid credential environment reference"
                );
            } else {
                bail!("credential must use file: or env:");
            }
        }
        for p in &self.pools {
            ensure!(
                valid_name(&p.name) && names.insert(&p.name),
                "invalid or duplicate pool name: {}",
                p.name
            );
            ensure!(
                !p.organization.is_empty()
                    && p.organization.len() <= 39
                    && p.organization
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                "invalid organization"
            );
            ensure!(
                self.auth.contains_key(&p.auth),
                "unknown auth profile: {}",
                p.auth
            );
            ensure!(
                !p.labels.is_empty() && p.labels.len() < 100,
                "pool needs 1..99 custom labels"
            );
            ensure!(
                p.labels.iter().all(|l| !l.trim().is_empty()
                    && l.len() <= 255
                    && !l.contains([',', '\n', '\r'])
                    && !l.starts_with("pool:")),
                "invalid or reserved label"
            );
            ensure!(
                !p.runner_group.trim().is_empty() && !p.runner_group.contains(['\n', '\r']),
                "invalid runner group"
            );
            ensure!(
                !p.image.is_empty()
                    && !p.image.starts_with('-')
                    && !p.image.chars().any(char::is_whitespace),
                "invalid image"
            );
            ensure!(
                p.image.contains('@')
                    || p.image.rsplit('/').next().unwrap_or_default().contains(':'),
                "pin image with a version tag or digest"
            );
            ensure!(
                !p.image.ends_with(":latest"),
                "latest image tag is not allowed; pin a version"
            );
        }
        ensure!(
            self.pools
                .iter()
                .map(|p| u64::from(p.replicas))
                .sum::<u64>()
                <= u64::from(self.manager.max_runners),
            "sum of replicas exceeds max_runners (including stopped pools)"
        );
        Ok(())
    }
    pub fn warnings(&self) -> Vec<String> {
        let mut out = vec![];
        for (i, a) in self.pools.iter().enumerate() {
            for b in self.pools.iter().skip(i + 1) {
                if a.organization == b.organization && a.labels.iter().any(|l| b.labels.contains(l))
                {
                    out.push(format!(
                        "{} and {} share routing labels; use pool:<name> to target a pool",
                        a.name, b.name
                    ));
                }
            }
        }
        out
    }
}
#[derive(Clone)]
pub struct Paths {
    pub home: PathBuf,
}
impl Paths {
    pub fn new(home: PathBuf) -> Result<Self> {
        let home = if home.is_absolute() {
            home
        } else {
            std::env::current_dir()?.join(home)
        };
        Ok(Self { home })
    }
    pub fn prepare(&self) -> Result<()> {
        secure_dir(&self.home)?;
        secure_dir(&self.home.join("credentials"))?;
        secure_dir(&self.home.join("runs"))?;
        secure_dir(&self.home.join("logs"))
    }
    pub fn config(&self) -> PathBuf {
        self.home.join("config.toml")
    }
    pub fn socket(&self) -> PathBuf {
        self.home.join("manager.sock")
    }
}
pub fn secure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    ensure!(
        !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "directory must not be a symlink"
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().context("missing parent")?;
    let mut f = tempfile::NamedTempFile::new_in(parent)?;
    f.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    f.write_all(data)?;
    f.as_file().sync_all()?;
    f.persist(path).map_err(|e| e.error)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
pub fn credential(auth: &Auth) -> Result<String> {
    let value = if let Some(path) = auth.credential.strip_prefix("file:") {
        let mut f = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .context("cannot open credential file")?;
        let meta = f.metadata()?;
        ensure!(
            meta.is_file() && meta.permissions().mode() & 0o077 == 0,
            "credential must be a regular file with mode 0600"
        );
        use std::io::Read;
        let mut value = String::new();
        (&mut f).take(16385).read_to_string(&mut value)?;
        value
    } else if let Some(name) = auth.credential.strip_prefix("env:") {
        std::env::var(name).context("credential environment variable is missing")?
    } else {
        bail!("invalid credential reference");
    };
    let value = value.trim().to_owned();
    ensure!(
        !value.is_empty() && value.len() <= 16384 && !value.chars().any(char::is_whitespace),
        "invalid token contents"
    );
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    pub fn fixture() -> Config {
        let mut c = Config::default();
        c.auth.insert(
            "team".into(),
            Auth {
                credential: "env:RUNNERCTL_TEST_TOKEN".into(),
            },
        );
        c.pools.push(Pool {
            name: "validate".into(),
            organization: "test-org".into(),
            auth: "team".into(),
            labels: vec!["validate".into()],
            runner_group: "Default".into(),
            replicas: 4,
            image: default_image(),
            docker_socket: false,
        });
        let mut b = c.pools[0].clone();
        b.name = "build".into();
        b.labels = vec!["build".into()];
        b.replicas = 2;
        c.pools.push(b);
        c
    }
    #[test]
    fn config_roundtrip_and_capacity() {
        let mut c = fixture();
        assert_eq!(Config::parse(&toml::to_string(&c).unwrap()).unwrap(), c);
        c.manager.max_runners = 5;
        assert!(c.validate().is_err());
    }
    #[test]
    fn reject_ambiguous_and_unsafe_config() {
        let mut c = fixture();
        c.pools[1].name = "validate".into();
        assert!(c.validate().is_err());
        let mut c = fixture();
        c.pools[0].auth = "missing".into();
        assert!(c.validate().is_err());
        let mut c = fixture();
        c.pools[0].labels = vec!["pool:other".into()];
        assert!(c.validate().is_err());
        let mut c = fixture();
        c.pools[0].organization = "../bad".into();
        assert!(c.validate().is_err());
        let mut c = fixture();
        c.pools[0].image = "image:latest".into();
        assert!(c.validate().is_err());
        assert!(Config::parse("schema_version=1\nunknown=true").is_err());
    }
    #[test]
    fn token_permissions_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("token");
        atomic_write(&file, b"example-secret").unwrap();
        let auth = Auth {
            credential: format!("file:{}", file.display()),
        };
        assert_eq!(credential(&auth).unwrap(), "example-secret");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(credential(&auth).is_err());
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(
            credential(&Auth {
                credential: format!("file:{}", link.display())
            })
            .is_err()
        );
    }
    #[test]
    fn shared_labels_warn_but_remain_valid() {
        let mut c = fixture();
        c.pools[1].labels = c.pools[0].labels.clone();
        c.validate().unwrap();
        assert_eq!(c.warnings().len(), 1);
    }
}
