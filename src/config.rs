//! Switchyard application configuration.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::providers::{ProviderConfig, ProviderError, ProviderRegistry};

#[derive(Debug, Error)]
pub enum ConfigFileError {
    #[error("failed to read config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse config: {0}")]
    Parse(#[from] serde_json::Error),
}

/// Top-level Switchyard configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SwitchyardConfig {
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
}

impl SwitchyardConfig {
    pub fn parse(contents: &str) -> Result<Self, ConfigFileError> {
        serde_json::from_str(contents).map_err(ConfigFileError::from)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigFileError> {
        let path = path.as_ref().to_path_buf();
        let contents = std::fs::read_to_string(&path).map_err(|source| ConfigFileError::Read {
            path: path.clone(),
            source,
        })?;
        Self::parse(&contents)
    }

    pub fn into_registry(self) -> Result<ProviderRegistry, ProviderError> {
        ProviderRegistry::from_configs(self.providers)
    }
}

pub fn default_config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("switchyard/config.json")
}
