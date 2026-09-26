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

    let env = CliEnvironment::load(Some(&path)).expect("env should load");

    assert_eq!(env.get("BRAINTRUST_API_KEY"), Some("bt_key"));
    assert_eq!(env.get("OPENAI_API_KEY"), Some("openai_key"));
    assert_eq!(env.get("ANTHROPIC_API_KEY"), Some("anthropic_key"));
}

#[test]
fn explicit_env_file_must_exist() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("missing.env");
    let error = CliEnvironment::load(Some(&path)).unwrap_err();
    assert!(error.to_string().contains("missing.env"));
    assert!(CliEnvironment::load(None).unwrap().into_vars().is_empty());
}
