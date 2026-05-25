use std::collections::HashMap;

use crate::secret_value_from_env_arg;

#[test]
fn env_secret_error_does_not_echo_shell_expanded_secret() {
    let expanded_secret = "sk-proj-sensitive-secret-value";

    let error = secret_value_from_env_arg(expanded_secret, &HashMap::new())
        .expect_err("secret should be rejected");

    assert!(!error.contains(expanded_secret));
    assert!(error.contains("not the secret value"));
}

#[test]
fn unset_env_secret_error_does_not_echo_env_name() {
    let env_name = "EXO_TEST_SECRET_THAT_SHOULD_NOT_EXIST";

    let error =
        secret_value_from_env_arg(env_name, &HashMap::new()).expect_err("env var should be unset");

    assert!(!error.contains(env_name));
    assert!(error.contains("--env"));
}

#[test]
fn env_secret_can_come_from_loaded_env_file_values() {
    let env_name = "EXO_TEST_SECRET_FROM_ENV_FILE";
    let mut loaded_env = HashMap::new();
    loaded_env.insert(env_name.to_string(), "loaded-secret".to_string());

    let secret = secret_value_from_env_arg(env_name, &loaded_env).expect("secret should resolve");

    assert_eq!(secret, "loaded-secret");
}
