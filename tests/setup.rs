use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use switchyard::config::SwitchyardConfig;
use switchyard::setup::{
    ProviderPreset, apply_credentials, build_config, credentials_path, reload_credentials,
    write_config, write_credentials,
};

#[test]
fn builds_selected_provider_presets_without_credentials() {
    let config = build_config(&[ProviderPreset::Kimi, ProviderPreset::Qwen]);

    assert_eq!(config.providers.len(), 2);
    assert_eq!(config.providers[0].id, "kimi");
    assert_eq!(config.providers[1].id, "qwen");
    let wire = serde_json::to_string(&config).expect("config json");
    assert!(!wire.contains("sk-"));
    assert!(!wire.contains("LLM_"));
    assert!(
        config
            .providers
            .iter()
            .all(|provider| provider.validate().is_ok())
    );
}

#[test]
fn duplicate_presets_are_written_once_in_stable_order() {
    let config = build_config(&[
        ProviderPreset::Qwen,
        ProviderPreset::Kimi,
        ProviderPreset::Qwen,
    ]);

    let ids: Vec<_> = config
        .providers
        .iter()
        .map(|provider| provider.id.as_str())
        .collect();
    assert_eq!(ids, ["qwen", "kimi"]);
}

#[test]
fn write_config_creates_parent_and_refuses_accidental_overwrite() {
    let root = tempfile_dir();
    let path = root.join("nested/config.json");
    let config = SwitchyardConfig::default();

    write_config(&path, &config, false).expect("first write");
    assert_eq!(
        fs::read_to_string(&path).expect("config file"),
        "{\n  \"providers\": [],\n  \"aliases\": {},\n  \"fallbacks\": {}\n}\n"
    );

    let error = write_config(&path, &config, false).expect_err("overwrite must be refused");
    assert!(error.to_string().contains("already exists"));

    write_config(&path, &config, true).expect("forced write");
}

#[test]
fn credentials_are_stored_separately_and_loaded_into_missing_environment() {
    let root = tempfile_dir();
    let config_path = root.join("config.json");
    let secrets_path = credentials_path(&config_path);
    let values = [("SWITCHYARD_SETUP_TEST_KEY", "secret-value")]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();

    write_credentials(&secrets_path, &values, false).expect("credentials write");
    assert!(!config_path.exists());
    assert!(
        !fs::read_to_string(&secrets_path)
            .expect("credentials file")
            .contains("api_key")
    );

    unsafe { std::env::remove_var("SWITCHYARD_SETUP_TEST_KEY") };
    let applied = apply_credentials(&secrets_path).expect("apply credentials");
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0], "SWITCHYARD_SETUP_TEST_KEY");
    assert_eq!(
        std::env::var("SWITCHYARD_SETUP_TEST_KEY").expect("environment"),
        "secret-value"
    );
    unsafe { std::env::remove_var("SWITCHYARD_SETUP_TEST_KEY") };

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reload_does_not_override_caller_set_env_vars() {
    let root = tempfile_dir();
    let config_path = root.join("config.json");
    let secrets_path = credentials_path(&config_path);

    // File has both keys; caller pre-sets one of them.
    let values = [
        ("SWITCHYARD_RELOAD_TEST_CALLER", "file-value"),
        ("SWITCHYARD_RELOAD_TEST_FILEONLY", "file-value"),
    ]
    .into_iter()
    .map(|(n, v)| (n.to_string(), v.to_string()))
    .collect();
    write_credentials(&secrets_path, &values, false).expect("write credentials");

    unsafe {
        std::env::remove_var("SWITCHYARD_RELOAD_TEST_CALLER");
        std::env::remove_var("SWITCHYARD_RELOAD_TEST_FILEONLY");
        std::env::set_var("SWITCHYARD_RELOAD_TEST_CALLER", "caller-value");
    }

    let applied = apply_credentials(&secrets_path).expect("apply");
    // Only the file-only key was applied; the caller-set one is skipped.
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0], "SWITCHYARD_RELOAD_TEST_FILEONLY");
    assert_eq!(
        std::env::var("SWITCHYARD_RELOAD_TEST_CALLER").expect("env"),
        "caller-value"
    );

    // Rewrite the file with new values, then reload using the applied set.
    let updated = [
        ("SWITCHYARD_RELOAD_TEST_CALLER", "new-file-value"),
        ("SWITCHYARD_RELOAD_TEST_FILEONLY", "new-file-value"),
    ]
    .into_iter()
    .map(|(n, v)| (n.to_string(), v.to_string()))
    .collect();
    write_credentials(&secrets_path, &updated, true).expect("rewrite");

    reload_credentials(&secrets_path, &applied).expect("reload");
    // Caller-set var must remain untouched.
    assert_eq!(
        std::env::var("SWITCHYARD_RELOAD_TEST_CALLER").expect("env"),
        "caller-value"
    );
    // File-only var was updated.
    assert_eq!(
        std::env::var("SWITCHYARD_RELOAD_TEST_FILEONLY").expect("env"),
        "new-file-value"
    );

    unsafe {
        std::env::remove_var("SWITCHYARD_RELOAD_TEST_CALLER");
        std::env::remove_var("SWITCHYARD_RELOAD_TEST_FILEONLY");
    }
    let _ = fs::remove_dir_all(&root);
}

fn tempfile_dir() -> std::path::PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "switchyard-setup-test-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    path
}
