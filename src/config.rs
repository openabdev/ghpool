use serde::Deserialize;
use std::fs;

#[derive(Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    pub identities: Vec<IdentityConfig>,
    #[serde(default)]
    pub allowed_owners: Vec<String>,
    #[serde(default)]
    pub cache: CacheConfig,
}

#[derive(Clone, Deserialize)]
pub struct IdentityConfig {
    pub id: String,
    pub token: String,
}

#[derive(Clone, Deserialize)]
pub struct CacheConfig {
    #[serde(default = "default_max_entries")]
    pub max_entries: u64,
    #[serde(default = "default_pr_ttl")]
    pub pr_view_ttl_secs: u64,
    #[serde(default = "default_pr_ttl")]
    pub issue_list_ttl_secs: u64,
    #[serde(default = "default_run_ttl")]
    pub run_list_ttl_secs: u64,
    #[serde(default = "default_commit_ttl")]
    pub commit_list_ttl_secs: u64,
    #[serde(default = "default_repo_ttl")]
    pub repo_view_ttl_secs: u64,
    #[serde(default = "default_ttl")]
    pub default_ttl_secs: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_entries: default_max_entries(),
            pr_view_ttl_secs: default_pr_ttl(),
            issue_list_ttl_secs: default_pr_ttl(),
            run_list_ttl_secs: default_run_ttl(),
            commit_list_ttl_secs: default_commit_ttl(),
            repo_view_ttl_secs: default_repo_ttl(),
            default_ttl_secs: default_ttl(),
        }
    }
}

fn default_port() -> u16 { 8080 }
fn default_max_entries() -> u64 { 10000 }
fn default_pr_ttl() -> u64 { 30 }
fn default_run_ttl() -> u64 { 15 }
fn default_commit_ttl() -> u64 { 120 }
fn default_repo_ttl() -> u64 { 300 }
fn default_ttl() -> u64 { 60 }

impl Config {
    pub fn load() -> Self {
        let path = std::env::var("GHPOOL_CONFIG")
            .unwrap_or_else(|_| "config.yaml".to_string());

        if let Ok(content) = fs::read_to_string(&path) {
            let mut config: Config = serde_yaml::from_str(&content)
                .expect("failed to parse config file");
            config.apply_env_overrides();
            return config;
        }

        // Fallback: load from environment variables only
        let identities = Self::identities_from_env();
        let allowed_owners = std::env::var("GHPOOL_ALLOWED_OWNERS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let port = std::env::var("GHPOOL_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default_port());

        Config {
            port,
            identities,
            allowed_owners,
            cache: CacheConfig::default(),
        }
    }

    fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("GHPOOL_PORT") {
            if let Ok(p) = v.parse() { self.port = p; }
        }
        if let Ok(v) = std::env::var("GHPOOL_ALLOWED_OWNERS") {
            self.allowed_owners = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        }
    }

    /// Parse GHPOOL_PAT_<ID>=<token> env vars
    fn identities_from_env() -> Vec<IdentityConfig> {
        std::env::vars()
            .filter(|(k, _)| k.starts_with("GHPOOL_PAT_"))
            .map(|(k, v)| IdentityConfig {
                id: k.strip_prefix("GHPOOL_PAT_").unwrap().to_lowercase(),
                token: v,
            })
            .collect()
    }
}
