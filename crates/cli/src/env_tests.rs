use std::fs;

use tempfile::TempDir;

use crate::env::CliEnvironment;

#[test]
fn env_file_parsing_supports_basic_key_values() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let path = tempdir.path().join(".env.local");
    fs::write(
        &path,
        "BRAINTRUST_API_KEY=bt_key\nOPENAI_API_KEY=\"openai_key\"\nexport ANTHROPIC_API_KEY='anthropic_key'\n",
    )
    .expect("env file should write");

    let env = CliEnvironment::load(None, Some(&path)).expect("env should load");

    assert_eq!(env.get("BRAINTRUST_API_KEY"), Some("bt_key"));
    assert_eq!(env.get("OPENAI_API_KEY"), Some("openai_key"));
    assert_eq!(env.get("ANTHROPIC_API_KEY"), Some("anthropic_key"));
}

#[test]
fn explicit_env_file_overrides_optional_env_file_if_exists() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let optional_path = tempdir.path().join(".env.local");
    let explicit_path = tempdir.path().join(".env.override");
    fs::write(&optional_path, "BRAINTRUST_API_KEY=optional\n").expect("optional env should write");
    fs::write(&explicit_path, "BRAINTRUST_API_KEY=explicit\n").expect("explicit env should write");

    let env =
        CliEnvironment::load(Some(&optional_path), Some(&explicit_path)).expect("env should load");

    assert_eq!(env.get("BRAINTRUST_API_KEY"), Some("explicit"));
}
