#[test]
fn default_workspace_build_is_the_headless_server() {
    let output = std::process::Command::new("cargo")
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--offline",
        ])
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
        .output()
        .unwrap();
    assert!(output.status.success());
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let defaults = metadata["workspace_default_members"].as_array().unwrap();
    assert_eq!(
        defaults.len(),
        1,
        "default builds must not compile desktop/mobile applications"
    );
    assert!(defaults[0].as_str().unwrap().contains("skykeep-mcp"));
}
