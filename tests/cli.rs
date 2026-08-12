use std::fs;
use std::process::Command;

use switchyard::config::SwitchyardConfig;

#[test]
fn init_all_writes_a_ready_config_without_credentials() {
    let root = std::env::temp_dir().join(format!("switchyard-cli-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let path = root.join("config.json");

    let output = Command::new(env!("CARGO_BIN_EXE_switchyard"))
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
