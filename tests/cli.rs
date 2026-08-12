use std::fs;
use std::net::SocketAddr;
use std::process::Command;
use std::sync::{Arc, Mutex};

use futures_util::stream;
use serde_json::{Value, json};
use switchyard::config::SwitchyardConfig;
use switchyard::gateway::{
    Backend, BackendFuture, BackendRequest, BackendStream, Gateway, ModelInfo,
};

#[derive(Clone, Default)]
struct MockBackend {
    calls: Arc<Mutex<Vec<BackendRequest>>>,
}

impl Backend for MockBackend {
    fn models(&self) -> Vec<ModelInfo> {
        vec![
            ModelInfo::new("kimi/kimi-k3[1m]"),
            ModelInfo::new("qwen/qwen3.8-max"),
        ]
    }

    fn complete(&self, request: BackendRequest) -> BackendFuture<'_, Value> {
        let calls = Arc::clone(&self.calls);
        Box::pin(async move {
            calls.lock().expect("test mutex").push(request);
            Ok(json!({"id":"msg_test","type":"message"}))
        })
    }

    fn stream(&self, request: BackendRequest) -> BackendFuture<'_, BackendStream> {
        let calls = Arc::clone(&self.calls);
        Box::pin(async move {
            calls.lock().expect("test mutex").push(request);
            Ok(Box::pin(stream::iter(vec![Ok(json!({"type":"message_stop"}))])) as BackendStream)
        })
    }
}

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_switchyard"))
}

fn write_temp_config(dir: &std::path::Path, content: &str) -> std::path::PathBuf {
    let path = dir.join("config.json");
    fs::write(&path, content).expect("write config");
    path
}

/// Start a local axum server that speaks a minimal Anthropic-compatible
/// protocol on a dedicated thread with its own runtime.
/// Returns the address and a shutdown sender.
fn start_fake_provider_thread() -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    std::thread::Builder::new()
        .name("fake-provider".to_string())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async {
                let app = axum::Router::new().route(
                    "/v1/messages",
                    axum::routing::post(|| async {
                        axum::Json(json!({
                            "id": "msg_fake",
                            "type": "message",
                            "role": "assistant",
                            "content": [],
                            "model": "fake-model",
                            "stop_reason": "end_turn",
                            "usage": {"input_tokens": 0, "output_tokens": 0}
                        }))
                    }),
                );
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind");
                let addr = listener.local_addr().expect("addr");
                addr_tx.send(addr).expect("send addr");
                axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .expect("serve");
            });
        })
        .expect("spawn");

    let addr = addr_rx.recv().expect("recv addr");
    (addr, shutdown_tx)
}

/// Start a real Gateway on a dedicated thread with its own runtime.
fn start_gateway_thread() -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    std::thread::Builder::new()
        .name("test-gateway".to_string())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async {
                let backend = MockBackend::default();
                let gateway = Gateway::new(backend);
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind");
                let addr = listener.local_addr().expect("addr");
                addr_tx.send(addr).expect("send addr");
                gateway
                    .serve_with_shutdown(listener, async {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .expect("serve");
            });
        })
        .expect("spawn");

    let addr = addr_rx.recv().expect("recv addr");
    // Give the server a moment to start accepting.
    std::thread::sleep(std::time::Duration::from_millis(50));
    (addr, shutdown_tx)
}

// ── Existing test ──────────────────────────────────────────────────

#[test]
fn init_all_writes_a_ready_config_without_credentials() {
    let root = std::env::temp_dir().join(format!("switchyard-cli-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let path = root.join("config.json");

    let output = binary()
        .args([
            "init",
            "--all",
            "--no-credentials",
            "--config",
            path.to_str().expect("config path"),
        ])
        .output()
        .expect("run switchyard init");

    assert!(
        output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config = SwitchyardConfig::load(&path).expect("generated config");
    assert_eq!(config.providers.len(), 3);
    assert!(String::from_utf8_lossy(&output.stdout).contains("MOONSHOT_API_KEY"));
    assert!(
        !fs::read_to_string(&path)
            .expect("config text")
            .contains("sk-")
    );

    let _ = fs::remove_dir_all(root);
}

// ── F10: doctor ────────────────────────────────────────────────────

#[test]
fn doctor_passes_with_valid_config_and_reachable_provider() {
    let (addr, _shutdown) = start_fake_provider_thread();
    let dir = std::env::temp_dir().join(format!(
        "switchyard-doctor-pass-{}-{}",
        std::process::id(),
        addr.port()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("mkdir");

    let config_content = format!(
        r#"{{
  "providers": [{{
    "id": "fake",
    "base_url": "http://127.0.0.1:{}/",
    "auth": {{"type": "none"}},
    "models": [{{"id": "fake-model"}}]
  }}]
}}"#,
        addr.port()
    );
    let config_path = write_temp_config(&dir, &config_content);

    let output = binary()
        .args(["doctor", "--config", config_path.to_str().expect("config")])
        .output()
        .expect("run doctor");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "doctor should pass: stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("PASS"), "expected PASS marker: {stdout}");
    assert!(
        stdout.contains("fake"),
        "expected provider name in output: {stdout}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn doctor_warns_on_unreachable_provider() {
    let dir = std::env::temp_dir().join(format!("switchyard-doctor-warn-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("mkdir");

    let config_content = r#"{
  "providers": [{
    "id": "dead",
    "base_url": "http://127.0.0.1:1/",
    "auth": {"type": "none"},
    "models": [{"id": "dead-model"}]
  }]
}"#;
    let config_path = write_temp_config(&dir, config_content);

    let output = binary()
        .args(["doctor", "--config", config_path.to_str().expect("config")])
        .output()
        .expect("run doctor");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "doctor should exit 0 (reachability is WARN): stdout={stdout}"
    );
    assert!(stdout.contains("WARN"), "expected WARN marker: {stdout}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn doctor_fails_on_missing_credential() {
    let dir = std::env::temp_dir().join(format!("switchyard-doctor-cred-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("mkdir");

    let config_content = r#"{
  "providers": [{
    "id": "needskey",
    "base_url": "http://127.0.0.1:1/",
    "auth": {
      "type": "header",
      "header": "Authorization",
      "env_var": "SWITCHYARD_DOCTOR_TEST_MISSING_KEY_XYZ"
    },
    "models": [{"id": "some-model"}]
  }]
}"#;
    let config_path = write_temp_config(&dir, config_content);

    let output = binary()
        .args(["doctor", "--config", config_path.to_str().expect("config")])
        .env_remove("SWITCHYARD_DOCTOR_TEST_MISSING_KEY_XYZ")
        .output()
        .expect("run doctor");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "doctor should exit 1 on missing credential: stdout={stdout}"
    );
    assert!(stdout.contains("FAIL"), "expected FAIL marker: {stdout}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn doctor_fails_on_invalid_config() {
    let dir = std::env::temp_dir().join(format!("switchyard-doctor-bad-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("mkdir");

    let config_path = write_temp_config(&dir, "{ not valid json }");

    let output = binary()
        .args(["doctor", "--config", config_path.to_str().expect("config")])
        .output()
        .expect("run doctor");

    assert!(!output.status.success(), "doctor should fail on bad config");

    let _ = fs::remove_dir_all(&dir);
}

// ── F11: models ────────────────────────────────────────────────────

#[test]
fn models_lists_routes_from_running_gateway() {
    let (addr, _shutdown) = start_gateway_thread();

    let output = binary()
        .args([
            "models",
            "--host",
            "127.0.0.1",
            "--port",
            &addr.port().to_string(),
        ])
        .output()
        .expect("run models");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "models failed: stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("kimi/kimi-k3[1m]"),
        "expected kimi route: {stdout}"
    );
    assert!(
        stdout.contains("qwen/qwen3.8-max"),
        "expected qwen route: {stdout}"
    );
}

#[test]
fn models_exits_1_on_unreachable_gateway() {
    let output = binary()
        .args(["models", "--host", "127.0.0.1", "--port", "1"])
        .output()
        .expect("run models");

    assert!(!output.status.success(), "models should exit 1");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not reachable"),
        "expected unreachable message: {stderr}"
    );
}

// ── F11: status ────────────────────────────────────────────────────

#[test]
fn status_shows_health_and_version() {
    let (addr, _shutdown) = start_gateway_thread();

    let output = binary()
        .args([
            "status",
            "--host",
            "127.0.0.1",
            "--port",
            &addr.port().to_string(),
        ])
        .output()
        .expect("run status");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "status failed: stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("healthy"), "expected healthy: {stdout}");
    assert!(stdout.contains("version"), "expected version: {stdout}");
    assert!(stdout.contains("git_sha"), "expected git_sha: {stdout}");
}

#[test]
fn status_exits_1_on_unreachable_gateway() {
    let output = binary()
        .args(["status", "--host", "127.0.0.1", "--port", "1"])
        .output()
        .expect("run status");

    assert!(!output.status.success(), "status should exit 1");
}

// ── F12: version SHA ───────────────────────────────────────────────

#[test]
fn long_version_contains_git_sha() {
    let output = binary()
        .args(["--version"])
        .output()
        .expect("run --version");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "version should succeed");
    assert!(
        stdout.contains('(') && stdout.contains(')'),
        "expected sha in parens: {stdout}"
    );
    let sha_part = stdout
        .split('(')
        .nth(1)
        .and_then(|s| s.split(')').next())
        .unwrap_or("");
    assert!(
        sha_part.len() >= 4,
        "expected a sha-looking string, got: '{sha_part}'"
    );
}

#[test]
fn health_json_contains_version_and_git_sha() {
    let (addr, _shutdown) = start_gateway_thread();

    let output = binary()
        .args([
            "status",
            "--host",
            "127.0.0.1",
            "--port",
            &addr.port().to_string(),
        ])
        .output()
        .expect("run status");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    assert!(stdout.contains("version"), "missing version: {stdout}");
    assert!(stdout.contains("git_sha"), "missing git_sha: {stdout}");
}

// ── F12: systemd service install/uninstall ─────────────────────────

#[test]
fn service_install_and_uninstall_round_trip() {
    let dir = std::env::temp_dir().join(format!("switchyard-svc-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let home = dir.join("home");
    fs::create_dir_all(&home).expect("mkdir");

    let config_path = dir.join("config.json");
    fs::write(&config_path, "{}").expect("write config");

    // Install
    let output = binary()
        .args([
            "service",
            "install",
            "--config",
            config_path.to_str().expect("config"),
        ])
        .env("HOME", home.to_str().expect("home"))
        .output()
        .expect("run service install");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "install failed: stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("Installed"),
        "expected 'Installed' message: {stdout}"
    );
    assert!(
        stdout.contains("systemctl --user daemon-reload"),
        "expected systemctl hint: {stdout}"
    );

    let unit_path = home.join(".config/systemd/user/switchyard.service");
    assert!(unit_path.exists(), "unit file should exist");

    let unit_content = fs::read_to_string(&unit_path).expect("read unit");
    assert!(
        unit_content.contains("[Unit]"),
        "unit missing [Unit]: {unit_content}"
    );
    assert!(
        unit_content.contains("ExecStart="),
        "unit missing ExecStart: {unit_content}"
    );
    assert!(
        unit_content.contains("--config"),
        "unit missing --config: {unit_content}"
    );
    assert!(
        !unit_content.contains("secret") && !unit_content.contains("sk-"),
        "unit must not contain secrets: {unit_content}"
    );
    assert!(
        unit_content.contains("After=network-online.target"),
        "unit missing After: {unit_content}"
    );
    assert!(
        unit_content.contains("WantedBy=default.target"),
        "unit missing WantedBy: {unit_content}"
    );
    assert!(
        unit_content.contains("Restart=on-failure"),
        "unit missing Restart: {unit_content}"
    );

    // Uninstall
    let output = binary()
        .args(["service", "uninstall"])
        .env("HOME", home.to_str().expect("home"))
        .output()
        .expect("run service uninstall");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "uninstall failed: stdout={stdout}");
    assert!(
        stdout.contains("Removed"),
        "expected 'Removed' message: {stdout}"
    );
    assert!(
        !unit_path.exists(),
        "unit file should be removed after uninstall"
    );

    let _ = fs::remove_dir_all(&dir);
}
