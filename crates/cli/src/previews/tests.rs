use super::*;

#[test]
fn hostnames_are_stable_valid_and_distinguish_threads_with_the_same_name() -> Result<()> {
    let hostname = thread_hostname("Braintrust Dev", "Hello / world", "first", "exo.localhost")?;
    assert_eq!(
        hostname,
        thread_hostname("Braintrust Dev", "Hello / world", "first", "exo.localhost")?
    );
    assert_ne!(
        hostname,
        thread_hostname("Braintrust Dev", "Hello / world", "second", "exo.localhost")?
    );
    assert!(hostname.starts_with("hello-world-"));
    assert!(hostname.ends_with(".braintrust-dev.exo.localhost"));
    let long = thread_hostname(&"a".repeat(128), &"b".repeat(128), "id", "exo.localhost")?;
    assert!(long.split('.').all(|label| label.len() <= 63));
    Ok(())
}
