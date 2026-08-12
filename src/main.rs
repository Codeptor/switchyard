use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use switchyard::config::{SwitchyardConfig, default_config_path};
use switchyard::gateway::{Gateway, ListenConfig, ProviderBackend};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "switchyard",
    about = "Local provider-agnostic gateway for Claude Code"
)]
struct Cli {
    /// JSON configuration containing provider endpoints and model IDs.
    #[arg(long, env = "SWITCHYARD_CONFIG")]
    config: Option<PathBuf>,

    /// Local bind address. Defaults to loopback for WSL safety.
    #[arg(long, env = "SWITCHYARD_HOST")]
    host: Option<IpAddr>,

    /// Local bind port.
    #[arg(long, env = "SWITCHYARD_PORT")]
    port: Option<u16>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("switchyard=info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(default_config_path);
    let config = SwitchyardConfig::load(&config_path).with_context(|| {
        format!(
            "unable to load Switchyard configuration from {}",
            config_path.display()
        )
    })?;
    let registry = Arc::new(
        config
            .into_registry()
            .context("unable to initialize configured providers")?,
    );
    let defaults = ListenConfig::default();
    let listen = ListenConfig::new(
        cli.host.unwrap_or(defaults.host),
        cli.port.unwrap_or(defaults.port),
    );
    listen
        .validate()
        .map_err(anyhow::Error::msg)
        .context("refusing to expose provider credentials on a non-loopback listener")?;

    info!(
        config = %config_path.display(),
        providers = registry.len(),
        address = %listen.socket_addr(),
        "starting switchyard"
    );

    Gateway::new(ProviderBackend::new(registry))
        .bind_and_serve(listen.socket_addr())
        .await
        .context("Switchyard server stopped")
}
