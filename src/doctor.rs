//! `switchyard doctor` — sequential pre-flight checks for config, credentials,
//! provider reachability, and gateway health.
//!
//! Exit code 1 when config is invalid or any credential is missing.
//! Reachability failures are WARN and do not affect the exit code.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::SwitchyardConfig;
use crate::providers::config::AuthConfig;

const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Pass,
    Warn,
    Fail,
}

impl Severity {
    fn marker(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        }
    }

    fn color(self) -> &'static str {
        match self {
            Self::Pass => GREEN,
            Self::Warn => YELLOW,
            Self::Fail => RED,
        }
    }
}

/// Run all doctor checks, printing results as they execute.
///
/// Returns `true` for exit code 0, `false` for exit code 1.
pub fn run_doctor(config_path: &Path) -> Result<bool> {
    println!("{BOLD}switchyard doctor{RESET}");
    println!();

    let mut hard_fail = false;

    // ── 1. Config ──────────────────────────────────────────────
    let config = match load_and_validate_config(config_path) {
        Ok(config) => {
            print_row(
                Severity::Pass,
                "config",
                &format!("loaded from {}", config_path.display()),
            );
            print_provider_table(&config);
            config
        }
        Err(err) => {
            print_row(Severity::Fail, "config", &format!("{err:#}"));
            println!();
            println!("{RED}Cannot continue without a valid config.{RESET}");
            return Ok(false);
        }
    };

    // ── 2. Credentials ─────────────────────────────────────────
    for provider in &config.providers {
        let severity = check_credential(&provider.auth);
        if severity == Severity::Fail {
            hard_fail = true;
        }
        let detail = credential_detail(provider.id.as_str(), &provider.auth);
        print_row(severity, "credential", &detail);
    }

    // ── 3. Reachability ────────────────────────────────────────
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("unable to build HTTP client for reachability checks")?;

    for provider in &config.providers {
        let base = provider.normalized_base_url();
        let severity = match client.get(&base).send() {
            Ok(_) => Severity::Pass,
            Err(_) => Severity::Warn,
        };
        print_row(severity, "reach", &format!("{} {}", provider.id, base));
    }

    // ── 4. Gateway ─────────────────────────────────────────────
    let gateway_url = "http://127.0.0.1:3456/health";
    let gw_severity = match client.get(gateway_url).send() {
        Ok(resp) if resp.status().is_success() => Severity::Pass,
        Ok(_) => Severity::Warn,
        Err(_) => Severity::Warn,
    };
    let gw_detail = match gw_severity {
        Severity::Pass => format!("{gateway_url} is serving"),
        _ => format!("{gateway_url} is not reachable"),
    };
    print_row(gw_severity, "gateway", &gw_detail);

    // ── Summary ────────────────────────────────────────────────
    println!();
    if hard_fail {
        println!(
            "{RED}Some checks failed. Fix the issues above before starting the gateway.{RESET}"
        );
    } else {
        println!("{GREEN}All required checks passed.{RESET}");
    }

    Ok(!hard_fail)
}

fn load_and_validate_config(config_path: &Path) -> Result<SwitchyardConfig> {
    let config = SwitchyardConfig::load(config_path)
        .with_context(|| format!("unable to load config from {}", config_path.display()))?;
    for provider in &config.providers {
        provider
            .validate()
            .with_context(|| format!("provider '{}' validation failed", provider.id))?;
    }
    Ok(config)
}

fn check_credential(auth: &AuthConfig) -> Severity {
    match auth {
        AuthConfig::None => Severity::Pass,
        AuthConfig::Header { env_var, .. } => {
            let set = std::env::var(env_var)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false);
            if set { Severity::Pass } else { Severity::Fail }
        }
    }
}

fn credential_detail(provider_id: &str, auth: &AuthConfig) -> String {
    match auth {
        AuthConfig::None => format!("{provider_id} {DIM}(no auth){RESET}"),
        AuthConfig::Header { env_var, .. } => {
            format!("{provider_id} {DIM}{env_var}{RESET}")
        }
    }
}

fn print_row(severity: Severity, check: &str, detail: &str) {
    let color = severity.color();
    let marker = severity.marker();
    println!("  {color}{marker}{RESET}  {BOLD}{check:<12}{RESET}  {detail}");
}

fn print_provider_table(config: &SwitchyardConfig) {
    if config.providers.is_empty() {
        println!("  {DIM}(no providers configured){RESET}");
        return;
    }
    println!();
    println!(
        "  {BOLD}{:<16}  {:<40}  models{RESET}",
        "provider", "base_url"
    );
    println!("  {}", "\u{2500}".repeat(76));
    for provider in &config.providers {
        let models = if provider.models.is_empty() {
            "(none)".to_string()
        } else {
            provider
                .models
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        println!(
            "  {:<16}  {:<40}  {}",
            provider.id,
            provider.normalized_base_url(),
            models
        );
    }
    println!();
}
