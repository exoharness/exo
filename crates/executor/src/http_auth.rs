/// Shared static bearer-token authentication for native and Worker HTTP hosts.
pub fn bearer_token_matches(token: &str, authorization: Option<&str>) -> bool {
    !token.trim().is_empty()
        && authorization.and_then(|header| header.strip_prefix("Bearer ")) == Some(token)
}
