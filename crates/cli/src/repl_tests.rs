use crate::pick_repl_model;

#[test]
fn pick_repl_model_prefers_an_explicit_request() {
    let registered = vec!["gpt-5.4".to_string(), "claude".to_string()];
    assert_eq!(
        pick_repl_model(&registered, Some("claude".to_string()))
            .expect("a registered request resolves"),
        "claude"
    );
}

#[test]
fn pick_repl_model_falls_back_to_the_first_registered() {
    let registered = vec!["gpt-5.4".to_string(), "claude".to_string()];
    assert_eq!(
        pick_repl_model(&registered, None).expect("the first registered model resolves"),
        "gpt-5.4"
    );
}

#[test]
fn pick_repl_model_rejects_an_unregistered_request() {
    let registered = vec!["gpt-5.4".to_string()];
    assert!(pick_repl_model(&registered, Some("missing".to_string())).is_err());
}

#[test]
fn pick_repl_model_requires_a_registered_model() {
    assert!(pick_repl_model(&[], None).is_err());
}
