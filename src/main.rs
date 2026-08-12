use std::collections::BTreeMap;
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use switchyard::config::{SwitchyardConfig, default_config_path};
use switchyard::gateway::{Gateway, ListenConfig, ProviderBackend};
use switchyard::setup::{
    ProviderPreset, apply_credentials, build_config, credentials_path, write_config,
    write_credentials,
};
use tokio::signal::unix::{SignalKind, signal};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "switchyard",
    about = "Local provider-agnostic gateway for Claude Code"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    run: RunArgs,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a first-run config and optionally store provider credentials.
    Init(InitArgs),
}

#[derive(Debug, Args)]
struct RunArgs {
    /// JSON configuration containing provider endpoints and model IDs.
    #[arg(long, env = "SWITCHYARD_CONFIG", global = true)]
    config: Option<PathBuf>,

    /// Local bind address. Defaults to loopback for WSL safety.
    #[arg(long, env = "SWITCHYARD_HOST", global = true)]
    host: Option<IpAddr>,

    /// Local bind port.
    #[arg(long, env = "SWITCHYARD_PORT", global = true)]
    port: Option<u16>,
}

#[derive(Debug, Args)]
struct InitArgs {
    /// Write the generated config to this path.
    #[arg(long, env = "SWITCHYARD_CONFIG")]
    config: Option<PathBuf>,

    /// Enable a preset; repeat the flag or use a comma-separated list.
    #[arg(long, value_enum, value_delimiter = ',')]
    provider: Vec<ProviderPreset>,

    /// Enable all built-in provider presets without asking selection questions.
    #[arg(long)]
    all: bool,

    /// Replace existing config and credentials files.
    #[arg(long)]
    force: bool,

    /// Generate only the config template and skip hidden credential prompts.
    #[arg(long)]
    no_credentials: bool,
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
    match cli.command {
        Some(Command::Init(args)) => run_init(args),
        None => run_gateway(cli.run).await,
    }
}

fn run_init(args: InitArgs) -> Result<()> {
    let presets = selected_presets(&args)?;
    let config_path = args.config.unwrap_or_else(default_config_path);
    let credentials_file = credentials_path(&config_path);

    if !args.force && config_path.exists() {
        bail!(
            "config file already exists at {}; use --force to replace it",
            config_path.display()
        );
    }
    if !args.force && !args.no_credentials && credentials_file.exists() {
        bail!(
            "credentials file already exists at {}; use --force to replace it",
            credentials_file.display()
        );
    }

    let credentials = if args.no_credentials {
        BTreeMap::new()
    } else {
        collect_credentials(&presets)?
    };
    let config = build_config(&presets);
    write_config(&config_path, &config, args.force).context("unable to write generated config")?;
    if !credentials.is_empty() {
        write_credentials(&credentials_file, &credentials, args.force)
            .context("unable to write local credentials")?;
    }

    println!("Created {}", config_path.display());
    if credentials.is_empty() {
        println!("No credentials stored; export the provider variables before starting.");
    } else {
        println!("Stored credentials in {}", credentials_file.display());
    }
    println!("Credential variables:");
    for preset in &presets {
        println!("  {}", preset.credential_env());
    }
    println!();
    println!(
        "Start Switchyard: switchyard --config {}",
        config_path.display()
    );
    println!("Then point Claude Code at the local gateway:");
    println!("  export ANTHROPIC_BASE_URL=http://127.0.0.1:3456");
    println!("  export ANTHROPIC_AUTH_TOKEN=switchyard-local");
    println!("  export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1");
    println!();
    println!("Configured models:");
    for preset in presets {
        println!("  {}/{}", preset.id(), preset.models().join(", "));
    }
    Ok(())
}

fn selected_presets(args: &InitArgs) -> Result<Vec<ProviderPreset>> {
    if args.all && !args.provider.is_empty() {
        bail!("use either --all or --provider, not both");
    }
    let presets = if args.all {
        ProviderPreset::ALL.to_vec()
    } else if !args.provider.is_empty() {
        args.provider.clone()
    } else {
        prompt_for_presets()?
    };
    let presets = presets
        .into_iter()
        .fold(Vec::new(), |mut selected, preset| {
            if !selected.contains(&preset) {
                selected.push(preset);
            }
            selected
        });
    if presets.is_empty() {
        bail!("no providers selected; rerun with --provider or --all");
    }
    Ok(presets)
}

fn prompt_for_presets() -> Result<Vec<ProviderPreset>> {
    println!("Switchyard initial setup");
    println!("Select providers. API keys are prompted privately afterward.");
    let mut selected = Vec::new();
    for preset in ProviderPreset::ALL {
        print!(
            "Enable {} ({})? [y/N] ",
            preset.display_name(),
            preset.models().join(", ")
        );
        io::stdout()
            .flush()
            .context("unable to flush setup prompt")?;
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .context("unable to read setup selection")?;
        if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            selected.push(preset);
        }
    }
    Ok(selected)
}

fn collect_credentials(presets: &[ProviderPreset]) -> Result<BTreeMap<String, String>> {
    let mut credentials = BTreeMap::new();
    println!();
    println!("Enter provider API keys. Input is hidden; leave blank to configure later.");
    for preset in presets {
        let prompt = format!(
            "{} API key ({}): ",
            preset.display_name(),
            preset.credential_env()
        );
        let value = rpassword::prompt_password(prompt).context("unable to read API key")?;
        if !value.trim().is_empty() {
            credentials.insert(preset.credential_env().to_string(), value);
        }
    }
    Ok(credentials)
}

async fn run_gateway(args: RunArgs) -> Result<()> {
    let config_path = args.config.unwrap_or_else(default_config_path);
    let credential_count = apply_credentials(credentials_path(&config_path))
        .context("unable to load local credentials")?;
    let config = SwitchyardConfig::load(&config_path).with_context(|| {
        format!(
            "unable to load Switchyard configuration from {}",
            config_path.display()
        )
    })?;
    let aliases = config.aliases.clone();
    let registry = Arc::new(
        config
            .into_registry()
            .context("unable to initialize configured providers")?,
    );
    let defaults = ListenConfig::default();
    let listen = ListenConfig::new(
        args.host.unwrap_or(defaults.host),
        args.port.unwrap_or(defaults.port),
    );
    listen
        .validate()
        .map_err(anyhow::Error::msg)
        .context("refusing to expose provider credentials on a non-loopback listener")?;

    info!(
        config = %config_path.display(),
        providers = registry.len(),
        credentials_loaded = credential_count,
        address = %listen.socket_addr(),
        "starting switchyard"
    );

    let listener = tokio::net::TcpListener::bind(listen.socket_addr()).await?;
    Gateway::new(ProviderBackend::new(registry, aliases))
        .serve_with_shutdown(listener, shutdown_signal())
        .await
        .context("Switchyard server stopped")
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install ctrl-c handler");
    };

    let terminate = async {
        signal(SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
