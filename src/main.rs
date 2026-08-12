use std::collections::BTreeMap;
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use switchyard::config::{SwitchyardConfig, default_config_path};
use switchyard::doctor;
use switchyard::gateway::{
    Backend, FallbackBackend, Gateway, HotBackend, ListenConfig, ProviderBackend, Telemetry,
    TelemetryState, UsageSnapshotRow,
};
use switchyard::setup::{
    ProviderPreset, apply_credentials, build_config, credentials_path, reload_credentials,
    write_config, write_credentials,
};
use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

/// Concrete backend stack: Telemetry → Fallback → Provider.
type ConcreteBackend = Telemetry<FallbackBackend<ProviderBackend>>;

#[derive(Debug, Parser)]
#[command(
    name = "switchyard",
    about = "Local provider-agnostic gateway for Claude Code",
    long_version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("SWITCHYARD_GIT_SHA"), ")")
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
    /// Run pre-flight checks on config, credentials, reachability, and gateway.
    Doctor(DoctorArgs),
    /// List model routes from a running gateway.
    Models(ModelsArgs),
    /// Show gateway health and usage summary.
    Status(StatusArgs),
    /// Query usage statistics from a running gateway.
    Usage(UsageArgs),
    /// Manage the systemd user service unit.
    Service(ServiceArgs),
}

#[derive(Debug, Clone, Args)]
struct ServiceArgs {
    #[command(subcommand)]
    command: ServiceSubcommand,
}

#[derive(Debug, Clone, Subcommand)]
enum ServiceSubcommand {
    /// Write a systemd user service unit for switchyard.
    Install(ServiceInstallArgs),
    /// Remove the systemd user service unit.
    Uninstall,
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

    /// Optional bearer token for API authentication. When set, /v1/* routes
    /// require Authorization: Bearer <token>. Non-loopback binds are only
    /// permitted when a token is configured.
    #[arg(long, env = "SWITCHYARD_TOKEN", global = true)]
    token: Option<String>,
}

#[derive(Debug, Clone, Args)]
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

#[derive(Debug, Clone, Args)]
struct UsageArgs {
    /// Gateway host to connect to.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Gateway port to connect to.
    #[arg(long, default_value_t = 3456)]
    port: u16,

    /// Bearer token for authenticated gateways.
    #[arg(long, env = "SWITCHYARD_TOKEN")]
    token: Option<String>,

    /// JSON configuration containing provider endpoints and model IDs.
    #[arg(long, env = "SWITCHYARD_CONFIG", global = true)]
    config: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct DoctorArgs {}

#[derive(Debug, Args)]
struct ModelsArgs {}

#[derive(Debug, Args)]
struct StatusArgs {}

#[derive(Debug, Clone, Args)]
struct ServiceInstallArgs {
    /// Path to the switchyard config file to bake into the unit.
    #[arg(long, env = "SWITCHYARD_CONFIG")]
    config: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Sync subcommands run without a tokio runtime so reqwest::blocking
    // does not conflict with an outer runtime.
    match &cli.command {
        Some(Command::Init(args)) => return run_init(args.clone()),
        Some(Command::Doctor(_)) => return run_doctor(&cli.run),
        Some(Command::Models(_)) => return run_models(&cli.run),
        Some(Command::Status(_)) => return run_status(&cli.run),
        Some(Command::Usage(args)) => return run_usage(args.clone()),
        Some(Command::Service(args)) => return run_service(args.command.clone(), &cli.run),
        None => {}
    }

    // Gateway command needs the full async runtime.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("switchyard=info")),
        )
        .with_target(false)
        .init();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("unable to build tokio runtime")?
        .block_on(run_gateway(cli.run))
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

fn run_usage(args: UsageArgs) -> Result<()> {
    let url = format!("http://{}:{}/usage", args.host, args.port);
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .context("unable to build HTTP client")?;
    let mut req = client.get(&url);
    if let Some(token) = &args.token {
        req = req.bearer_auth(token);
    }
    let response = match req.send() {
        Ok(resp) => resp,
        Err(err) if err.is_connect() || err.is_timeout() => {
            bail!("gateway not reachable at {}:{}", args.host, args.port);
        }
        Err(err) => return Err(err).context("usage request failed"),
    };
    if !response.status().is_success() {
        bail!("gateway returned status {}", response.status());
    }
    let rows: Vec<UsageSnapshotRow> = response.json().context("unable to parse usage response")?;
    print_usage_table(&rows);
    Ok(())
}

fn print_usage_table(rows: &[UsageSnapshotRow]) {
    if rows.is_empty() {
        println!("No usage recorded yet.");
        return;
    }

    // Column widths
    let w_prov = rows
        .iter()
        .map(|r| r.provider.len())
        .max()
        .unwrap_or(8)
        .max(8);
    let w_model = rows.iter().map(|r| r.model.len()).max().unwrap_or(5).max(5);
    let w_day = 10;

    println!(
        "{:<w_prov$}  {:<w_model$}  {:<w_day$}  {:>8}  {:>12}  {:>13}  {:>6}",
        "provider", "model", "day", "requests", "input tokens", "output tokens", "errors"
    );
    println!(
        "{}",
        "-".repeat(w_prov + w_model + w_day + 8 + 12 + 13 + 6 + 12)
    );

    let mut total_requests: u64 = 0;
    let mut total_input: u64 = 0;
    let mut total_output: u64 = 0;
    let mut total_errors: u64 = 0;

    for row in rows {
        println!(
            "{:<w_prov$}  {:<w_model$}  {:<w_day$}  {:>8}  {:>12}  {:>13}  {:>6}",
            row.provider,
            row.model,
            row.day,
            row.requests,
            row.input_tokens,
            row.output_tokens,
            row.errors
        );
        total_requests += row.requests;
        total_input += row.input_tokens;
        total_output += row.output_tokens;
        total_errors += row.errors;
    }

    println!(
        "{}",
        "-".repeat(w_prov + w_model + w_day + 8 + 12 + 13 + 6 + 12)
    );
    println!(
        "{:<w_prov$}  {:<w_model$}  {:<w_day$}  {:>8}  {:>12}  {:>13}  {:>6}",
        "TOTAL", "", "", total_requests, total_input, total_output, total_errors
    );
}

fn run_doctor(run: &RunArgs) -> Result<()> {
    let config_path = run.config.clone().unwrap_or_else(default_config_path);
    let cred_path = credentials_path(&config_path);
    // Load credentials into env so the doctor sees them.
    let _ = apply_credentials(&cred_path);
    let ok = doctor::run_doctor(&config_path)?;
    if !ok {
        bail!("doctor found issues");
    }
    Ok(())
}

fn run_models(run: &RunArgs) -> Result<()> {
    let host = run.host.unwrap_or_else(|| ListenConfig::default().host);
    let port = run.port.unwrap_or_else(|| ListenConfig::default().port);
    let data = fetch_models(run, host, port)?;

    if data.is_empty() {
        println!("No models configured.");
        return Ok(());
    }

    let mut routes: Vec<(String, String)> = data
        .iter()
        .map(|m| {
            let id = m
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            let owned_by = m
                .get("owned_by")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            (id, owned_by)
        })
        .collect();
    routes.sort_by(|a, b| a.0.cmp(&b.0));

    let w_id = routes
        .iter()
        .map(|(id, _)| id.len())
        .max()
        .unwrap_or(5)
        .max(5);
    let w_owner = routes
        .iter()
        .map(|(_, o)| o.len())
        .max()
        .unwrap_or(8)
        .max(8);

    println!("{:<w_id$}  {:<w_owner$}", "route", "owned_by");
    println!("{}", "-".repeat(w_id + w_owner + 2));
    for (id, owner) in &routes {
        println!("{:<w_id$}  {:<w_owner$}", id, owner);
    }
    Ok(())
}

fn fetch_models(run: &RunArgs, host: IpAddr, port: u16) -> Result<Vec<serde_json::Value>> {
    let url = format!("http://{host}:{port}/v1/models");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .context("unable to build HTTP client")?;
    let mut req = client.get(&url);
    if let Some(token) = &run.token {
        req = req.bearer_auth(token);
    }
    let response = match req.send() {
        Ok(resp) => resp,
        Err(err) if err.is_connect() || err.is_timeout() => {
            bail!("gateway not reachable at {host}:{port}");
        }
        Err(err) => return Err(err).context("models request failed"),
    };
    if !response.status().is_success() {
        bail!("gateway returned status {}", response.status());
    }
    let body: serde_json::Value = response.json().context("unable to parse models response")?;
    let data = body
        .get("data")
        .and_then(|v| v.as_array())
        .context("models response missing 'data' array")?
        .clone();
    Ok(data)
}

fn run_status(run: &RunArgs) -> Result<()> {
    let host = run.host.unwrap_or_else(|| ListenConfig::default().host);
    let port = run.port.unwrap_or_else(|| ListenConfig::default().port);

    let (health, usage_rows) = fetch_status(run, host, port)?;

    println!("\x1b[1mswitchyard status\x1b[0m");
    println!();
    println!("  gateway   http://{host}:{port}  \x1b[32mhealthy\x1b[0m");
    if let Some(version) = health.get("version").and_then(|v| v.as_str()) {
        println!("  version   {version}");
    }
    if let Some(sha) = health.get("git_sha").and_then(|v| v.as_str()) {
        println!("  git_sha   {sha}");
    }

    if let Some(rows) = usage_rows
        && !rows.is_empty()
    {
        println!();
        print_usage_table(&rows);
    }

    Ok(())
}

fn fetch_status(
    run: &RunArgs,
    host: IpAddr,
    port: u16,
) -> Result<(serde_json::Value, Option<Vec<UsageSnapshotRow>>)> {
    let base = format!("http://{host}:{port}");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .context("unable to build HTTP client")?;

    // Health
    let mut req = client.get(format!("{base}/health"));
    if let Some(token) = &run.token {
        req = req.bearer_auth(token);
    }
    let health_resp = match req.send() {
        Ok(resp) => resp,
        Err(err) if err.is_connect() || err.is_timeout() => {
            bail!("gateway not reachable at {host}:{port}");
        }
        Err(err) => return Err(err).context("health request failed"),
    };
    if !health_resp.status().is_success() {
        bail!("gateway returned status {}", health_resp.status());
    }
    let health: serde_json::Value = health_resp
        .json()
        .context("unable to parse health response")?;

    // Usage (best-effort)
    let mut req = client.get(format!("{base}/usage"));
    if let Some(token) = &run.token {
        req = req.bearer_auth(token);
    }
    let usage_rows = if let Ok(resp) = req.send()
        && resp.status().is_success()
        && let Ok(rows) = resp.json::<Vec<UsageSnapshotRow>>()
    {
        Some(rows)
    } else {
        None
    };

    Ok((health, usage_rows))
}

fn run_service(sub: ServiceSubcommand, run_args: &RunArgs) -> Result<()> {
    match sub {
        ServiceSubcommand::Install(args) => install_service(args, run_args),
        ServiceSubcommand::Uninstall => uninstall_service(),
    }
}

fn install_service(args: ServiceInstallArgs, run_args: &RunArgs) -> Result<()> {
    let home = std::env::var("HOME").context("HOME not set; cannot determine user config dir")?;
    let unit_dir = PathBuf::from(&home).join(".config/systemd/user");
    let unit_path = unit_dir.join("switchyard.service");

    let config_path = args
        .config
        .or_else(|| run_args.config.clone())
        .unwrap_or_else(default_config_path);
    let config_path = if config_path.is_absolute() {
        config_path
    } else {
        std::env::current_dir()
            .context("unable to resolve current directory")?
            .join(config_path)
    };

    let exe = std::env::current_exe().context("unable to determine switchyard binary path")?;
    let unit_content = render_systemd_unit(&exe, &config_path);

    std::fs::create_dir_all(&unit_dir)
        .with_context(|| format!("unable to create {}", unit_dir.display()))?;
    std::fs::write(&unit_path, unit_content)
        .with_context(|| format!("unable to write {}", unit_path.display()))?;

    println!("Installed {}", unit_path.display());
    println!();
    println!("To activate:");
    println!("  systemctl --user daemon-reload");
    println!("  systemctl --user enable --now switchyard");
    Ok(())
}

fn uninstall_service() -> Result<()> {
    let home = std::env::var("HOME").context("HOME not set; cannot determine user config dir")?;
    let unit_path = PathBuf::from(&home)
        .join(".config/systemd/user")
        .join("switchyard.service");

    if unit_path.exists() {
        std::fs::remove_file(&unit_path)
            .with_context(|| format!("unable to remove {}", unit_path.display()))?;
        println!("Removed {}", unit_path.display());
        println!();
        println!("Run: systemctl --user daemon-reload");
    } else {
        println!("No service unit found at {}", unit_path.display());
    }
    Ok(())
}

fn render_systemd_unit(exe: &Path, config: &Path) -> String {
    format!(
        "\
[Unit]
Description=Switchyard — local Anthropic gateway for Claude Code
After=network-online.target

[Service]
ExecStart=\"{exe}\" --config {config}
Restart=on-failure

[Install]
WantedBy=default.target
",
        exe = exe.display(),
        config = config.display(),
    )
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

/// Build the full backend stack from a config.
fn build_backend(
    config: SwitchyardConfig,
    telemetry_state: Arc<TelemetryState>,
) -> Result<ConcreteBackend> {
    let aliases = config.aliases.clone();
    let fallbacks = config.fallbacks.clone();
    let registry = Arc::new(
        config
            .into_registry()
            .context("unable to initialize configured providers")?,
    );
    let provider = ProviderBackend::new(registry, aliases);
    let fallback = FallbackBackend::new(provider, fallbacks);
    Ok(Telemetry::new(fallback, telemetry_state))
}

async fn run_gateway(args: RunArgs) -> Result<()> {
    let config_path = args.config.unwrap_or_else(default_config_path);
    let cred_path = credentials_path(&config_path);
    let applied_cred_names =
        apply_credentials(&cred_path).context("unable to load local credentials")?;
    let credential_count = applied_cred_names.len();
    let config = SwitchyardConfig::load(&config_path).with_context(|| {
        format!(
            "unable to load Switchyard configuration from {}",
            config_path.display()
        )
    })?;
    let defaults = ListenConfig::default();
    let authenticated = args.token.is_some();
    let listen = ListenConfig::new(
        args.host.unwrap_or(defaults.host),
        args.port.unwrap_or(defaults.port),
    );
    listen
        .validate(authenticated)
        .map_err(anyhow::Error::msg)
        .context("refusing to expose provider credentials on a non-loopback listener")?;

    if !listen.host.is_loopback() {
        warn!(
            address = %listen.socket_addr(),
            authenticated = authenticated,
            "binding to non-loopback address — ensure network-level access controls are in place"
        );
    }

    let telemetry_state = TelemetryState::new();
    let backend = build_backend(config, Arc::clone(&telemetry_state))?;
    let models = backend.models();
    let hot = HotBackend::new(backend);

    info!(
        config = %config_path.display(),
        providers = models.iter().map(|m| &m.id).collect::<Vec<_>>().len(),
        credentials_loaded = credential_count,
        address = %listen.socket_addr(),
        authenticated = authenticated,
        "starting switchyard"
    );

    let listener = tokio::net::TcpListener::bind(listen.socket_addr()).await?;
    let mut gateway = Gateway::new(hot.clone()).with_telemetry(Arc::clone(&telemetry_state));
    if let Some(token) = args.token {
        gateway = gateway.with_token(token);
    }
    gateway
        .serve_with_shutdown(
            listener,
            reload_or_shutdown(
                hot,
                config_path,
                cred_path,
                applied_cred_names,
                telemetry_state,
            ),
        )
        .await
        .context("Switchyard server stopped")
}

/// Run the reload loop: SIGHUP reloads config, Ctrl-C/SIGTERM shuts down.
async fn reload_or_shutdown(
    hot: HotBackend<ConcreteBackend>,
    config_path: PathBuf,
    cred_path: PathBuf,
    tracked_creds: Vec<String>,
    telemetry_state: Arc<TelemetryState>,
) {
    let mut sighup = signal(SignalKind::hangup()).expect("failed to install SIGHUP handler");
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

    tokio::pin!(ctrl_c);
    tokio::pin!(terminate);

    loop {
        tokio::select! {
            () = &mut ctrl_c => break,
            () = &mut terminate => break,
            _ = sighup.recv() => {
                info!("SIGHUP received, reloading configuration");
                if let Err(err) = do_reload(
                    &hot,
                    &config_path,
                    &cred_path,
                    &tracked_creds,
                    &telemetry_state,
                ) {
                    error!(error = %err, "reload failed, keeping previous configuration");
                }
            }
        }
    }
}

fn do_reload(
    hot: &HotBackend<ConcreteBackend>,
    config_path: &PathBuf,
    cred_path: &PathBuf,
    tracked_creds: &[String],
    telemetry_state: &Arc<TelemetryState>,
) -> Result<()> {
    // Reload credentials — only update vars we originally loaded from the file.
    let cred_updated =
        reload_credentials(cred_path, tracked_creds).context("unable to reload credentials")?;

    // Reload config
    let config = SwitchyardConfig::load(config_path)
        .with_context(|| format!("unable to reload config from {}", config_path.display()))?;

    let model_count = config
        .providers
        .iter()
        .map(|p| p.models.len())
        .sum::<usize>();
    let provider_count = config.providers.len();

    // Build new backend stack with the shared telemetry state
    let new_backend = build_backend(config, Arc::clone(telemetry_state))?;
    hot.swap(new_backend);

    info!(
        providers = provider_count,
        models = model_count,
        credentials_updated = cred_updated,
        "configuration reloaded successfully"
    );
    info!("listener host/port and auth token unchanged");
    Ok(())
}
