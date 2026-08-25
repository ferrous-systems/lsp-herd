use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::{fmt, fs};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use glob::{Pattern, PatternError};
use serde::de::{Error, Unexpected};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

mod default {
    use super::*;

    pub fn instance_timeout() -> Option<u32> {
        // 5 minutes
        Some(5 * 60)
    }

    pub fn gc_interval() -> u32 {
        // 10 seconds
        10
    }

    pub fn git_worktree_prewarm() -> bool {
        true
    }

    pub fn git_worktree_poll_interval() -> u32 {
        // 30 seconds
        30
    }

    pub fn rust_analyzer_target_dir_root() -> PathBuf {
        PathBuf::from("${cache_dir}/rust-analyzer-targets")
    }

    pub fn rust_analyzer_target_dir_grouping() -> RustAnalyzerTargetDirGrouping {
        RustAnalyzerTargetDirGrouping::PerInstance
    }

    pub fn listen() -> Address {
        // localhost & some random unprivileged port
        Address::Tcp(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 27_631)
    }

    pub fn connect() -> Address {
        listen()
    }

    pub fn log_filters() -> String {
        "info".to_owned()
    }

    pub fn pass_environment() -> Vec<Filter> {
        vec![Filter {
            negative: false,
            pattern: Pattern::new("*").unwrap(),
        }]
    }
}

mod de {
    use super::*;

    /// parse either bool(false) or u32
    pub fn instance_timeout<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OneOf {
            Bool(bool),
            U32(u32),
        }

        match OneOf::deserialize(deserializer) {
            Ok(OneOf::U32(value)) => Ok(Some(value)),
            Ok(OneOf::Bool(false)) => Ok(None),
            Ok(OneOf::Bool(true)) => Err(Error::invalid_value(
                Unexpected::Bool(true),
                &"a non-negative integer or false",
            )),
            Err(_) => Err(Error::custom(
                "invalid type: expected a non-negative integer or false",
            )),
        }
    }

    /// make sure the value is greater than 0 to giver users feedback on invalid configuration
    pub fn gc_interval<'de, D>(deserializer: D) -> Result<u32, D::Error>
    where
        D: Deserializer<'de>,
    {
        match u32::deserialize(deserializer)? {
            0 => Err(Error::invalid_value(
                Unexpected::Unsigned(0),
                &"an integer 1 or greater",
            )),
            value => Ok(value),
        }
    }

    pub fn git_worktree_poll_interval<'de, D>(deserializer: D) -> Result<u32, D::Error>
    where
        D: Deserializer<'de>,
    {
        match u32::deserialize(deserializer)? {
            0 => Err(Error::invalid_value(
                Unexpected::Unsigned(0),
                &"an integer 1 or greater",
            )),
            value => Ok(value),
        }
    }

    pub fn pass_environment<'de, D>(deserializer: D) -> Result<Vec<Filter>, D::Error>
    where
        D: Deserializer<'de>,
    {
        <BTreeSet<String>>::deserialize(deserializer)?
            .into_iter()
            .map(|pat| {
                let negative = pat.starts_with("!");
                let pat = pat.strip_prefix("!").unwrap_or(&pat);

                match Pattern::new(&pat) {
                    Ok(pattern) => Ok(Filter { negative, pattern }),
                    Err(PatternError { pos, msg }) => Err(Error::custom(format!(
                        "invalid pattern `{pat}` near {pos}: {msg}"
                    ))),
                }
            })
            .collect()
    }
}

mod ser {
    use super::*;

    pub fn pass_environment<S>(filters: &Vec<Filter>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut serializer = serializer.serialize_seq(Some(filters.len()))?;
        for filter in filters {
            serializer.serialize_element(&filter.to_string())?;
        }
        serializer.end()
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
pub enum Address {
    Tcp(IpAddr, u16),
    #[cfg(target_family = "unix")]
    Unix(PathBuf),
}

#[derive(Debug)]
pub struct Filter {
    pub negative: bool,
    pub pattern: Pattern,
}

impl fmt::Display for Filter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{}",
            if self.negative { "!" } else { "" },
            self.pattern.as_str(),
        )
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RustAnalyzerTargetDirGrouping {
    Shared,
    PerGitRemote,
    PerInstance,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default::instance_timeout")]
    #[serde(deserialize_with = "de::instance_timeout")]
    pub instance_timeout: Option<u32>,

    #[serde(default = "default::gc_interval")]
    #[serde(deserialize_with = "de::gc_interval")]
    pub gc_interval: u32,

    #[serde(default = "default::git_worktree_prewarm")]
    pub git_worktree_prewarm: bool,

    #[serde(default = "default::git_worktree_poll_interval")]
    #[serde(deserialize_with = "de::git_worktree_poll_interval")]
    pub git_worktree_poll_interval: u32,

    #[serde(default = "default::rust_analyzer_target_dir_root")]
    pub rust_analyzer_target_dir_root: PathBuf,

    #[serde(default = "default::rust_analyzer_target_dir_grouping")]
    pub rust_analyzer_target_dir_grouping: RustAnalyzerTargetDirGrouping,

    #[serde(default = "default::listen")]
    pub listen: Address,

    #[serde(default = "default::connect")]
    pub connect: Address,

    #[serde(default = "default::log_filters")]
    pub log_filters: String,

    #[serde(default = "default::pass_environment")]
    #[serde(deserialize_with = "de::pass_environment")]
    #[serde(serialize_with = "ser::pass_environment")]
    pub pass_environment: Vec<Filter>,
}

#[cfg(test)]
#[test]
fn generate_default_and_check_it_matches_commited_defaults() {
    use std::fs;
    use std::path::Path;

    let generated_defaults = Config::default();
    let generated_defaults = toml::to_string(&generated_defaults).expect("failed serialize");

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("defaults.toml");
    let saved_defaults = fs::read_to_string(path).expect("failed reading defaults.toml file");

    assert_eq!(generated_defaults, saved_defaults);
}

#[cfg(test)]
#[test]
fn parses_rust_analyzer_target_dir_grouping() {
    let config = toml::from_str::<Config>(
        r#"
        rust_analyzer_target_dir_grouping = "per-git-remote"
        "#,
    )
    .expect("parse config");

    assert_eq!(
        config.rust_analyzer_target_dir_grouping,
        RustAnalyzerTargetDirGrouping::PerGitRemote
    );
}

impl Default for Config {
    fn default() -> Self {
        Config {
            instance_timeout: default::instance_timeout(),
            gc_interval: default::gc_interval(),
            git_worktree_prewarm: default::git_worktree_prewarm(),
            git_worktree_poll_interval: default::git_worktree_poll_interval(),
            rust_analyzer_target_dir_root: default::rust_analyzer_target_dir_root(),
            rust_analyzer_target_dir_grouping: default::rust_analyzer_target_dir_grouping(),
            listen: default::listen(),
            connect: default::connect(),
            log_filters: default::log_filters(),
            pass_environment: default::pass_environment(),
        }
    }
}

impl Config {
    /// Try loading config file from the system default location
    pub fn try_load() -> Result<Self> {
        let pkg_name = env!("CARGO_PKG_NAME");
        let config_path = ProjectDirs::from("", "", pkg_name)
            .context("project config directory not found")?
            .config_dir()
            .join("config.toml");
        let path = config_path.display();
        let config_data =
            fs::read(&config_path).with_context(|| format!("cannot read config file `{path}`"))?;
        toml::from_slice(&config_data).with_context(|| format!("cannot parse config file `{path}`"))
    }

    pub fn resolved_rust_analyzer_target_dir_root(&self) -> PathBuf {
        const CACHE_DIR_PLACEHOLDER: &str = "${cache_dir}";

        let root = self.rust_analyzer_target_dir_root.to_string_lossy();
        let root = if root.contains(CACHE_DIR_PLACEHOLDER) {
            let cache_dir = project_dirs()
                .map(|dirs| dirs.cache_dir().to_owned())
                .unwrap_or_else(|| std::env::temp_dir().join(env!("CARGO_PKG_NAME")));
            PathBuf::from(root.replace(CACHE_DIR_PLACEHOLDER, cache_dir.to_string_lossy().as_ref()))
        } else {
            self.rust_analyzer_target_dir_root.clone()
        };

        if root.is_absolute() {
            root
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(root)
        }
    }

    /// Configure tracing-subscriber with env filter set to `log_filters` (if
    /// not overriden by RUST_LOG env var)
    ///
    /// Panics if called multiple times.
    pub fn init_logger(&self) {
        use tracing_subscriber::prelude::*;
        use tracing_subscriber::EnvFilter;

        let format = tracing_subscriber::fmt::layer()
            .without_time()
            .with_target(false)
            .with_writer(std::io::stderr);

        let filter = EnvFilter::try_from_default_env()
            .or_else(|_| EnvFilter::try_new(&self.log_filters))
            .unwrap_or_else(|_| EnvFilter::new("info"));

        tracing_subscriber::registry()
            .with(filter)
            .with(format)
            .init();
    }
}

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", env!("CARGO_PKG_NAME"))
}
