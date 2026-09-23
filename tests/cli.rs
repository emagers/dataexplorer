use std::process::Command;

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dataexplorer"))
}

#[test]
fn config_commands_and_headless_failures_never_start_lsp() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    let query = dir.path().join("query.kql");
    std::fs::write(&query, "print x=1; print y=2").unwrap();
    let init = binary()
        .args(["--config", config.to_str().unwrap(), "config", "init"])
        .output()
        .unwrap();
    assert!(init.status.success());
    assert!(init.stdout.is_empty());
    let second = binary()
        .args(["--config", config.to_str().unwrap(), "config", "init"])
        .output()
        .unwrap();
    assert_eq!(second.status.code(), Some(2));
    let add = binary()
        .args([
            "--config",
            config.to_str().unwrap(),
            "clusters",
            "add",
            "dev",
            "https://example.invalid",
            "--database",
            "Test",
            "--default",
        ])
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );
    let list = binary()
        .args(["--config", config.to_str().unwrap(), "clusters", "list"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(list.stdout).unwrap(),
        "dev\thttps://example.invalid\tTest\n"
    );
    let headless = binary()
        .env("PATH", dir.path())
        .args([
            "--config",
            config.to_str().unwrap(),
            "-r",
            query.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(headless.status.code(), Some(3));
    assert!(headless.stdout.is_empty());
    let error = String::from_utf8(headless.stderr).unwrap();
    assert!(error.contains("Azure CLI"));
    assert!(!error.contains("language server"));
    assert!(!dir.path().join("ui-state.toml").exists());
    let invalid = binary()
        .args([
            "--config",
            config.to_str().unwrap(),
            "-r",
            query.to_str().unwrap(),
            "-c",
            "http://bad",
        ])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(2));
    assert!(invalid.stdout.is_empty());
}

#[test]
#[ignore = "requires explicit live cluster/database and Azure CLI login"]
fn live_cli_exports_multiple_tables_partial_and_overwrite() {
    let endpoint =
        std::env::var("DATAEXPLORER_TEST_CLUSTER").expect("explicit live cluster required");
    let database =
        std::env::var("DATAEXPLORER_TEST_DATABASE").expect("explicit live database required");
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    let query = dir.path().join("query.kql");
    let invoke = |args: &[&str]| {
        let mut cmd = binary();
        cmd.args([
            "--config",
            config.to_str().unwrap(),
            "-r",
            query.to_str().unwrap(),
            "-c",
            &endpoint,
            "-d",
            &database,
            "--timeout",
            "30",
        ]);
        if let Ok(tenant) = std::env::var("DATAEXPLORER_TEST_TENANT") {
            cmd.args(["--tenant", &tenant]);
        }
        cmd.args(args).output().unwrap()
    };
    std::fs::write(&query, "print x=long(9007199254740993), d=decimal(-1.25), nested=dynamic({\"k\":1}), nothing=long(null)").unwrap();
    let result = invoke(&["--format", "json"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(value["rows"][0][0].as_i64(), Some(9007199254740993));
    assert_eq!(value["rows"][0][2]["k"], 1);
    assert!(value["rows"][0][3].is_null());
    assert_eq!(value["partial"], false);
    let path = dir.path().join("result.csv");
    std::fs::write(&path, "original").unwrap();
    assert_eq!(
        invoke(&["--format", "csv", "--output", path.to_str().unwrap()])
            .status
            .code(),
        Some(6)
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
    assert!(
        invoke(&[
            "--format",
            "csv",
            "--output",
            path.to_str().unwrap(),
            "--force"
        ])
        .status
        .success()
    );
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("9007199254740993")
    );
    std::fs::write(&query, "print a=1; print a=2").unwrap();
    let ambiguous = invoke(&["--format", "jsonl"]);
    assert_eq!(ambiguous.status.code(), Some(2));
    assert!(ambiguous.stdout.is_empty());
    let chosen = invoke(&["--format", "jsonl", "--table", "1"]);
    assert!(
        chosen.status.success(),
        "{}",
        String::from_utf8_lossy(&chosen.stderr)
    );
    assert_eq!(String::from_utf8(chosen.stdout).unwrap(), "[2]\n");
    std::fs::write(&query, "range x from 1 to 10 step 1").unwrap();
    let refused = invoke(&["--max-rows", "2", "--format", "csv"]);
    assert_eq!(
        refused.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(refused.stdout.is_empty());
    let accepted = invoke(&["--max-rows", "2", "--format", "csv", "--accept-partial"]);
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    assert_eq!(String::from_utf8(accepted.stdout).unwrap(), "x\n1\n2\n");
    assert!(
        !dir.path().join("ui-state.toml").exists(),
        "headless mode touched UI state"
    );
}
