use anyhow::{Context, Result, bail, ensure};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub query_path: Option<PathBuf>,
    pub default_cluster: Option<String>,
    pub defaults: Defaults,
    pub clusters: BTreeMap<String, Cluster>,
    pub language_server: LanguageServer,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            query_path: None,
            default_cluster: None,
            defaults: Defaults::default(),
            clusters: BTreeMap::new(),
            language_server: LanguageServer::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Defaults {
    pub query_timeout_secs: u64,
    pub max_rows: usize,
    pub max_bytes: usize,
}
impl Default for Defaults {
    fn default() -> Self {
        Self {
            query_timeout_secs: 240,
            max_rows: 100_000,
            max_bytes: 64 * 1024 * 1024,
        }
    }
}
impl Defaults {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=3600).contains(&self.query_timeout_secs),
            "query timeout must be 1..3600 seconds"
        );
        ensure!(
            (1..=10_000_000).contains(&self.max_rows),
            "max_rows must be 1..10000000"
        );
        ensure!(
            (1024..=1_073_741_824).contains(&self.max_bytes),
            "max_bytes must be 1024..1073741824"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cluster {
    pub endpoint: String,
    pub database: Option<String>,
    pub tenant: Option<String>,
    #[serde(default = "auth_default")]
    pub auth: String,
    pub query_timeout_secs: Option<u64>,
}
fn auth_default() -> String {
    "azure-cli".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LanguageServer {
    pub command: String,
    pub args: Vec<String>,
}
impl Default for LanguageServer {
    fn default() -> Self {
        Self {
            command: "kusto-lsp".into(),
            args: vec!["--stdio".into()],
        }
    }
}

#[derive(Clone, Debug)]
pub struct Target {
    pub label: String,
    pub endpoint: String,
    pub database: String,
    pub tenant: Option<String>,
    pub limits: Defaults,
}

#[derive(Default)]
pub struct Overrides {
    pub cluster: Option<String>,
    pub database: Option<String>,
    pub tenant: Option<String>,
    pub timeout: Option<u64>,
    pub max_rows: Option<usize>,
    pub max_bytes: Option<usize>,
}

pub fn config_path() -> Result<PathBuf> {
    Ok(ProjectDirs::from("com", "dataexplorer", "dataexplorer")
        .context("cannot determine platform config directory; pass --config")?
        .config_dir()
        .join("config.toml"))
}
pub fn state_path(config: &Path) -> PathBuf {
    config.with_file_name("ui-state.toml")
}

pub fn endpoint(input: &str) -> Result<String> {
    let u = url::Url::parse(input).context("invalid cluster URL")?;
    ensure!(
        u.scheme() == "https" && u.host_str().is_some(),
        "cluster endpoint must use HTTPS"
    );
    ensure!(
        u.username().is_empty() && u.password().is_none(),
        "credentials must not appear in endpoint"
    );
    ensure!(
        u.path() == "/" && u.query().is_none() && u.fragment().is_none(),
        "endpoint must be an HTTPS origin without path, query or fragment"
    );
    Ok(u.as_str().trim_end_matches('/').to_string())
}

impl Config {
    pub fn query_directory(&self, config_file: &Path) -> Option<PathBuf> {
        self.query_path.as_ref().map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                config_file.parent().unwrap_or(Path::new(".")).join(path)
            }
        })
    }
    pub fn load(path: &Path) -> Result<Self> {
        let config = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str::<Self>(&text).context("invalid config TOML")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e).context("reading config"),
        };
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.query_path
                .as_ref()
                .is_none_or(|p| !p.as_os_str().is_empty()),
            "query_path cannot be empty"
        );
        ensure!(
            self.version == 1,
            "unsupported config version {}",
            self.version
        );
        self.defaults.validate()?;
        for (alias, c) in &self.clusters {
            ensure!(
                !alias.is_empty() && !alias.contains("://"),
                "invalid cluster alias"
            );
            endpoint(&c.endpoint)?;
            ensure!(
                c.auth == "azure-cli",
                "only explicit azure-cli authentication is supported"
            );
            if let Some(timeout) = c.query_timeout_secs {
                ensure!(
                    (1..=3600).contains(&timeout),
                    "cluster timeout must be 1..3600 seconds"
                );
            }
        }
        if let Some(default) = &self.default_cluster {
            ensure!(
                self.clusters.contains_key(default),
                "default_cluster alias does not exist"
            );
        }
        Ok(())
    }
    pub fn save(&self, path: &Path, overwrite: bool) -> Result<()> {
        self.validate()?;
        atomic_write(path, overwrite, |w| {
            w.write_all(toml::to_string_pretty(self)?.as_bytes())?;
            Ok(())
        })
    }
    pub fn resolve(&self, o: &Overrides) -> Result<Target> {
        let name = o
            .cluster
            .as_ref()
            .or(self.default_cluster.as_ref())
            .context("select a cluster with -c or default_cluster")?;
        let cluster = self.clusters.get(name);
        let ep = match cluster {
            Some(c) => endpoint(&c.endpoint)?,
            None if name.starts_with("https://") => endpoint(name)?,
            None => bail!("unknown cluster alias {name:?}"),
        };
        let database = o
            .database
            .clone()
            .or_else(|| cluster.and_then(|c| c.database.clone()))
            .context("select a database with -d or configure one")?;
        ensure!(!database.trim().is_empty(), "database cannot be empty");
        let mut limits = self.defaults.clone();
        limits.query_timeout_secs = o
            .timeout
            .or_else(|| cluster.and_then(|c| c.query_timeout_secs))
            .unwrap_or(limits.query_timeout_secs);
        limits.max_rows = o.max_rows.unwrap_or(limits.max_rows);
        limits.max_bytes = o.max_bytes.unwrap_or(limits.max_bytes);
        limits.validate()?;
        Ok(Target {
            label: name.clone(),
            endpoint: ep,
            database,
            tenant: o
                .tenant
                .clone()
                .or_else(|| cluster.and_then(|c| c.tenant.clone())),
            limits,
        })
    }
}

pub fn atomic_write(
    path: &Path,
    overwrite: bool,
    write: impl FnOnce(&mut std::fs::File) -> Result<()>,
) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).context("creating output directory")?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent).context("creating temporary file")?;
    write(tmp.as_file_mut())?;
    tmp.as_file_mut().flush()?;
    tmp.as_file().sync_all()?;
    if overwrite {
        tmp.persist(path).map_err(|e| e.error)?;
    } else {
        tmp.persist_noclobber(path)
            .map_err(|e| e.error)
            .with_context(|| {
                format!("cannot create {}; use --force to overwrite", path.display())
            })?;
    }
    Ok(())
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct UiState {
    pub cluster_width: u16,
    pub editor_percent: u16,
}
impl UiState {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(toml::from_str(&s).context("invalid UI state")?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                cluster_width: 25,
                editor_percent: 45,
            }),
            Err(e) => Err(e.into()),
        }
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        atomic_write(path, true, |w| {
            w.write_all(toml::to_string(self)?.as_bytes())?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn query_directory_is_relative_to_configuration_not_launch_directory() {
        let mut c = Config {
            query_path: Some("queries/team".into()),
            ..Default::default()
        };
        assert_eq!(
            c.query_directory(Path::new("/config/place/config.toml"))
                .unwrap(),
            PathBuf::from("/config/place/queries/team")
        );
        c.query_path = Some("/absolute/queries".into());
        assert_eq!(
            c.query_directory(Path::new("/elsewhere/config.toml"))
                .unwrap(),
            PathBuf::from("/absolute/queries")
        );
        c.query_path = Some(PathBuf::new());
        assert!(c.validate().is_err());
    }
    #[test]
    fn documented_example_has_multiple_resolvable_clusters() {
        let config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        config.validate().unwrap();
        assert_eq!(config.clusters.len(), 2);
        assert_eq!(
            config.resolve(&Overrides::default()).unwrap().database,
            "Logs"
        );
        assert_eq!(
            config
                .resolve(&Overrides {
                    cluster: Some("production".into()),
                    ..Default::default()
                })
                .unwrap()
                .database,
            "ProductionLogs"
        );
    }
    #[test]
    fn resolve_precedence_and_validation() {
        let mut c = Config::default();
        c.clusters.insert(
            "dev".into(),
            Cluster {
                endpoint: "https://cluster.example".into(),
                database: Some("db".into()),
                tenant: Some("tenant-a".into()),
                auth: auth_default(),
                query_timeout_secs: Some(30),
            },
        );
        c.default_cluster = Some("dev".into());
        let t = c
            .resolve(&Overrides {
                database: Some("override".into()),
                tenant: Some("tenant-b".into()),
                timeout: Some(10),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(t.database, "override");
        assert_eq!(t.tenant.as_deref(), Some("tenant-b"));
        assert_eq!(t.limits.query_timeout_secs, 10);
        for url in [
            "http://cluster",
            "https://user:secret@cluster",
            "https://cluster/path",
            "https://cluster?q=x",
            "https://cluster#x",
        ] {
            assert!(endpoint(url).is_err());
        }
        assert!(
            c.resolve(&Overrides {
                cluster: Some("unknown".into()),
                ..Default::default()
            })
            .is_err()
        );
        c.version = 2;
        assert!(c.validate().is_err());
    }
    #[test]
    fn atomic_guard_and_state_separation() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        Config::default().save(&p, false).unwrap();
        assert!(Config::default().save(&p, false).is_err());
        assert_eq!(Config::load(&p).unwrap().version, 1);
        let sp = state_path(&p);
        UiState {
            cluster_width: 42,
            editor_percent: 55,
        }
        .save(&sp)
        .unwrap();
        assert_eq!(UiState::load(&sp).unwrap().cluster_width, 42);
        assert!(
            !std::fs::read_to_string(&p)
                .unwrap()
                .contains("cluster_width")
        );
    }
}
