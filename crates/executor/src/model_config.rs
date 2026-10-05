/// Anthropic models route to the native Messages API. We detect them by
/// model name (`claude*`). Bedrock/Vertex Anthropic ids carry provider prefixes
/// (e.g. `us.anthropic.claude-...`) so they do not match here and keep falling
/// through to the OpenAI-compatible path.
pub(crate) fn is_anthropic_model(model: &str) -> bool {
    model.to_ascii_lowercase().starts_with("claude")
}
