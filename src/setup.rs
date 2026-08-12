//! First-run configuration helpers for the `switchyard init` command.
//!
//! Presets only describe public endpoints, model IDs, and environment-variable
//! names. Generated provider config never contains API keys; optional local
//! credential storage is handled separately with private file permissions.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use crate::config::SwitchyardConfig;
use crate::providers::{AuthConfig, ModelCapabilities, ModelConfig, ProviderConfig};

/// Built-in provider presets used by the interactive first-run setup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum ProviderPreset {
    Kimi,
    Muse,
    Qwen,
}

impl ProviderPreset {
    pub const ALL: [Self; 3] = [Self::Kimi, Self::Muse, Self::Qwen];

    pub const fn id(self) -> &'static str {
        match self {
            Self::Kimi => "kimi",
            Self::Muse => "muse",
            Self::Qwen => "qwen",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Kimi => "Kimi",
            Self::Muse => "Meta Muse",
            Self::Qwen => "QwenCloud",
        }
    }

    pub const fn credential_env(self) -> &'static str {
        match self {
            Self::Kimi => "MOONSHOT_API_KEY",
            Self::Muse => "MODEL_API_KEY",
            Self::Qwen => "QWEN_API_KEY",
        }
    }

    pub const fn models(self) -> &'static [&'static str] {
        match self {
            Self::Kimi => &["kimi-k3[1m]"],
            Self::Muse => &["muse-spark-1.2-contributor"],
            Self::Qwen => &["qwen3.8-max", "qwen3.7-max", "qwen3.6-flash"],
        }
    }

    fn provider_config(self) -> ProviderConfig {
        let (base_url, default_model) = match self {
            Self::Kimi => ("https://api.moonshot.ai/anthropic", "kimi-k3[1m]"),
            Self::Muse => ("https://api.meta.ai", "muse-spark-1.2-contributor"),
            Self::Qwen => (
                "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic",
                "qwen3.8-max",
            ),
        };

        ProviderConfig {
            id: self.id().to_string(),
            base_url: Url::parse(base_url).expect("built-in provider URL must be valid"),
            auth: AuthConfig::Header {
                header: "Authorization".to_string(),
                env_var: self.credential_env().to_string(),
                prefix: Some("Bearer ".to_string()),
            },
            models: self
                .models()
                .iter()
                .map(|id| ModelConfig {
                    id: (*id).to_string(),
                    display_name: None,
                    context_window: None,
                    max_output_tokens: None,
                    capabilities: ModelCapabilities::default(),
                })
                .collect(),
            timeout_ms: None,
            default_model: Some(default_model.to_string()),
            extra_headers: vec![],
        }
    }
}

/// Build a config from selected presets, preserving caller order and removing
/// duplicate selections.
pub fn build_config(presets: &[ProviderPreset]) -> SwitchyardConfig {
    let providers = presets
        .iter()
        .copied()
        .fold(Vec::new(), |mut providers, preset| {
            if !providers
                .iter()
                .any(|provider: &ProviderConfig| provider.id == preset.id())
            {
                providers.push(preset.provider_config());
            }
            providers
        });
    SwitchyardConfig { providers }
}

/// Errors raised while writing the first-run configuration.
#[derive(Debug, Error)]
pub enum SetupError {
    #[error("config file already exists at {0}; use --force to replace it")]
    AlreadyExists(PathBuf),
    #[error("failed to create config directory {path}: {source}")]
    CreateDirectory { path: PathBuf, source: io::Error },
    #[error("failed to serialize config: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("failed to read credentials {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("failed to write config {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
}

/// Write a generated config, refusing accidental overwrite unless `force` is
/// explicitly set. The non-forced path uses `create_new` to remain race-safe.
pub fn write_config(
    path: impl AsRef<Path>,
    config: &SwitchyardConfig,
    force: bool,
) -> Result<(), SetupError> {
    let path = path.as_ref();
    let contents = format!("{}\n", serde_json::to_string_pretty(config)?);
    write_private_file(path, contents.as_bytes(), force)
}

#[derive(Debug, Serialize, Deserialize)]
struct CredentialsFile {
    credentials: BTreeMap<String, String>,
}

/// Return the credential file paired with a config path.
pub fn credentials_path(config_path: impl AsRef<Path>) -> PathBuf {
    config_path
        .as_ref()
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join("credentials.json")
}

/// Write credentials separately from the provider config with private file
/// permissions. Values are never printed or included in the generated config.
pub fn write_credentials(
    path: impl AsRef<Path>,
    credentials: &BTreeMap<String, String>,
    force: bool,
) -> Result<(), SetupError> {
    for name in credentials.keys() {
        if !valid_environment_name(name) {
            return Err(SetupError::Write {
                path: path.as_ref().to_path_buf(),
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid credential environment variable {name:?}"),
                ),
            });
        }
    }
    let file = CredentialsFile {
        credentials: credentials.clone(),
    };
    let contents = format!("{}\n", serde_json::to_string_pretty(&file)?);
    write_private_file(path.as_ref(), contents.as_bytes(), force)
}

/// Load stored credentials into the current process without replacing values
/// already supplied by the caller's environment.
pub fn apply_credentials(path: impl AsRef<Path>) -> Result<usize, SetupError> {
    let path = path.as_ref();
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(source) => {
            return Err(SetupError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let file: CredentialsFile = serde_json::from_str(&contents)?;
    let mut applied = 0;
    for (name, value) in file.credentials {
        if !valid_environment_name(&name) {
            return Err(SetupError::Write {
                path: path.to_path_buf(),
                source: io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid credential environment variable {name:?}"),
                ),
            });
        }
        if std::env::var_os(&name).is_none() {
            // SAFETY: the setup file is local, validated JSON, and this only
            // mutates the current process before the async server starts.
            unsafe { std::env::set_var(&name, value) };
            applied += 1;
        }
    }
    Ok(applied)
}

fn valid_environment_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic() || first == '_')
        && chars.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn write_private_file(path: &Path, contents: &[u8], force: bool) -> Result<(), SetupError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|source| SetupError::CreateDirectory {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    if path.exists() && !force {
        return Err(SetupError::AlreadyExists(path.to_path_buf()));
    }

    if force {
        fs::write(path, contents).map_err(|source| SetupError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    } else {
        let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                return Err(SetupError::AlreadyExists(path.to_path_buf()));
            }
            Err(source) => {
                return Err(SetupError::Write {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        file.write_all(contents)
            .and_then(|()| file.sync_all())
            .map_err(|source| SetupError::Write {
                path: path.to_path_buf(),
                source,
            })?;
    }

    set_private_permissions(path)
}

fn set_private_permissions(path: &Path) -> Result<(), SetupError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path)
            .map_err(|source| SetupError::Write {
                path: path.to_path_buf(),
                source,
            })?
            .permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(path, permissions).map_err(|source| SetupError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}
