use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use crate::harness_helpers::to_lingua_value;
use crate::{
    ModelClient, ModelRequest, ModelResponse, ModelResponseStream, PendingToolCall, ToolDefinition,
};
use async_trait::async_trait;
use braintrust_llm_router::{
    AuthConfig, ClientHeaders, ModelCatalog, ModelFlavor, ModelSpec, Router, create_provider,
};
use bytes::Bytes;
use exoharness::{Result, Uuid7};
use futures::{Stream, StreamExt};
use lingua::processing::adapter_for_format;
use lingua::serde_json::{self as lingua_json, Value as LinguaValue};
use lingua::universal::{
    AssistantContent, AssistantContentPart, TextContentPart, TokenBudget, ToolCallArguments,
    ToolChoiceConfig, ToolChoiceMode, UniversalParams, UniversalRequest, UniversalResponse,
    UniversalStreamChunk, UniversalTool,
};
use lingua::{Message, ProviderFormat};
use reqwest::Url;

type UniversalChunkStream = Pin<Box<dyn Stream<Item = Result<UniversalStreamChunk>> + Send>>;

#[derive(Debug, Default, Clone)]
pub struct RouterModelClient {
    env: Arc<HashMap<String, String>>,
}

impl RouterModelClient {
    pub fn new(env: HashMap<String, String>) -> Self {
        Self { env: Arc::new(env) }
    }
}

#[async_trait]
impl ModelClient for RouterModelClient {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let format = ProviderFormat::Responses;
        let config = resolve_runtime_config(&request, self.env.as_ref())?;
        let router = build_router(&request, format, &config)?;
        let universal_request = build_universal_request(&request, format, false)?;
        let payload = serialize_request(format, &universal_request)?;
        let body = router
            .complete(payload, &request.model, format, &ClientHeaders::new())
            .await?;
        let response = lingua::response_to_universal(body)?;
        normalize_model_response(response)
    }

    async fn complete_stream(&self, request: ModelRequest) -> Result<Box<dyn ModelResponseStream>> {
        let format = ProviderFormat::Responses;
        let config = resolve_runtime_config(&request, self.env.as_ref())?;
        let router = build_router(&request, format, &config)?;
        let universal_request = build_universal_request(&request, format, true)?;
        let payload = serialize_request(format, &universal_request)?;
        let raw_stream = router
            .complete_stream(payload, &request.model, format, &ClientHeaders::new())
            .await?;
        Ok(Box::new(RouterModelResponseStream {
            stream: map_universal_stream(raw_stream, format),
            accumulator: UniversalResponseAccumulator::default(),
        }))
    }
}

struct RouterModelResponseStream {
    stream: UniversalChunkStream,
    accumulator: UniversalResponseAccumulator,
}

#[async_trait]
impl ModelResponseStream for RouterModelResponseStream {
    async fn next_chunk(&mut self) -> Result<Option<UniversalStreamChunk>> {
        let Some(chunk) = self.stream.next().await.transpose()? else {
            return Ok(None);
        };
        self.accumulator.push(&chunk);
        Ok(Some(chunk))
    }

    async fn finish(self: Box<Self>) -> Result<ModelResponse> {
        normalize_model_response(self.accumulator.finalize())
    }
}

#[derive(Debug, Clone)]
struct ResolvedRuntimeConfig {
    provider_alias: String,
    provider_kind: String,
    endpoint: Option<Url>,
    endpoint_template: Option<String>,
    metadata: HashMap<String, lingua_json::Value>,
    auth: AuthConfig,
}

fn resolve_runtime_config(
    request: &ModelRequest,
    env: &HashMap<String, String>,
) -> Result<ResolvedRuntimeConfig> {
    let key = request
        .api_key
        .clone()
        .or_else(|| optional_env(env, "OPENAI_API_KEY"))
        .ok_or_else(|| anyhow::anyhow!("model request is missing an API key"))?;
    let endpoint = request
        .base_url
        .clone()
        .or_else(|| optional_env(env, "OPENAI_BASE_URL"))
        .map(|raw| Url::parse(&raw))
        .transpose()?;
    let mut metadata = HashMap::new();
    if let Some(organization_id) = optional_env(env, "OPENAI_ORG_ID") {
        metadata.insert(
            "organization_id".to_string(),
            lingua_json::Value::String(organization_id),
        );
    }
    if let Some(project) = optional_env(env, "OPENAI_PROJECT") {
        metadata.insert("project".to_string(), lingua_json::Value::String(project));
    }
    Ok(ResolvedRuntimeConfig {
        provider_alias: "openai".to_string(),
        provider_kind: "openai".to_string(),
        endpoint,
        endpoint_template: None,
        metadata,
        auth: AuthConfig::ApiKey {
            key,
            header: Some("authorization".to_string()),
            prefix: Some("Bearer".to_string()),
        },
    })
}

fn optional_env(env: &HashMap<String, String>, key: &str) -> Option<String> {
    env.get(key).cloned().or_else(|| std::env::var(key).ok())
}

fn build_router(
    request: &ModelRequest,
    format: ProviderFormat,
    config: &ResolvedRuntimeConfig,
) -> Result<Router> {
    let provider = create_provider(
        &config.provider_kind,
        config.endpoint.as_ref(),
        config.endpoint_template.as_deref(),
        None,
        &config.metadata,
    )?;

    let mut catalog = ModelCatalog::empty();
    catalog.insert(
        request.model.clone(),
        ModelSpec {
            model: request.model.clone(),
            format,
            flavor: match format {
                ProviderFormat::Responses => ModelFlavor::Responses,
                _ => ModelFlavor::Chat,
            },
            display_name: None,
            parent: None,
            input_cost_per_mil_tokens: None,
            output_cost_per_mil_tokens: None,
            input_cache_read_cost_per_mil_tokens: None,
            multimodal: None,
            reasoning: None,
            max_input_tokens: None,
            max_output_tokens: request.max_output_tokens.map(|tokens| tokens as u32),
            supports_streaming: true,
            extra: Default::default(),
            available_providers: vec![config.provider_alias.clone()],
        },
    );

    Router::builder()
        .with_catalog(std::sync::Arc::new(catalog))
        .add_provider_arc(
            config.provider_alias.clone(),
            provider,
            config.auth.clone(),
            vec![format],
        )
        .build()
        .map_err(Into::into)
}

fn build_universal_request(
    request: &ModelRequest,
    format: ProviderFormat,
    stream: bool,
) -> Result<UniversalRequest> {
    let tools = build_universal_tools(&request.tools)?;
    let has_tools = !tools.is_empty();
    let mut params = UniversalParams {
        token_budget: request.max_output_tokens.map(TokenBudget::OutputTokens),
        tools: if tools.is_empty() { None } else { Some(tools) },
        tool_choice: has_tools.then_some(ToolChoiceConfig {
            mode: Some(ToolChoiceMode::Auto),
            tool_name: None,
        }),
        parallel_tool_calls: has_tools.then_some(true),
        stream: Some(stream),
        ..Default::default()
    };

    if stream
        && matches!(
            format,
            ProviderFormat::ChatCompletions | ProviderFormat::Responses
        )
    {
        let mut stream_options = lingua_json::Map::new();
        stream_options.insert("include_usage".into(), LinguaValue::Bool(true));

        let mut extras = lingua_json::Map::new();
        extras.insert("stream_options".into(), LinguaValue::Object(stream_options));
        params.extras.insert(format, extras);
    }

    Ok(UniversalRequest {
        model: Some(request.model.clone()),
        messages: request.messages.clone(),
        params,
    })
}

fn build_universal_tools(tools: &[ToolDefinition]) -> Result<Vec<UniversalTool>> {
    tools.iter().map(tool_definition_to_universal).collect()
}

fn tool_definition_to_universal(tool: &ToolDefinition) -> Result<UniversalTool> {
    Ok(UniversalTool::function(
        tool.name.clone(),
        Some(tool.description.clone()),
        Some(to_lingua_value(tool.parameters.clone())),
        Some(true),
    ))
}

fn serialize_request(format: ProviderFormat, request: &UniversalRequest) -> Result<Bytes> {
    let adapter = adapter_for_format(format)
        .ok_or_else(|| anyhow::anyhow!("unsupported provider format for request: {format}"))?;
    let payload = adapter.request_from_universal(request)?;
    Ok(Bytes::from(lingua_json::to_vec(&payload)?))
}

fn map_universal_stream<S>(raw_stream: S, format: ProviderFormat) -> UniversalChunkStream
where
    S: Stream<
            Item = std::result::Result<
                braintrust_llm_router::StreamChunk,
                braintrust_llm_router::Error,
            >,
        > + Send
        + 'static,
{
    Box::pin(raw_stream.filter_map(move |item| async move {
        match item {
            Ok(chunk) => match lingua::parse_stream_event(chunk.data, format, format) {
                Ok(parsed) => parsed.universal.map(Ok),
                Err(error) => Some(Err(error.into())),
            },
            Err(error) => Some(Err(error.into())),
        }
    }))
}

fn normalize_model_response(response: UniversalResponse) -> Result<ModelResponse> {
    let response_id = Some(Uuid7::now());
    let tool_calls = extract_tool_calls(&response.messages)?;

    Ok(ModelResponse {
        response_id,
        messages: response.messages,
        tool_calls,
        usage: response.usage,
    })
}

fn extract_tool_calls(messages: &[Message]) -> Result<Vec<PendingToolCall>> {
    let mut tool_calls = Vec::new();

    for message in messages {
        let Message::Assistant { content, .. } = message else {
            continue;
        };
        let AssistantContent::Array(parts) = content else {
            continue;
        };

        for part in parts {
            if let AssistantContentPart::ToolCall {
                tool_call_id,
                tool_name,
                arguments,
                ..
            } = part
            {
                let ToolCallArguments::Valid(arguments) = arguments else {
                    continue;
                };
                tool_calls.push(PendingToolCall {
                    tool_call_id: tool_call_id.clone(),
                    request: exoharness::ToolRequest {
                        function_name: tool_name.clone(),
                        arguments: to_exoharness_arguments(arguments)?,
                    },
                });
            }
        }
    }

    Ok(tool_calls)
}

fn to_exoharness_arguments(
    arguments: &lingua::serde_json::Map<String, lingua::serde_json::Value>,
) -> Result<serde_json::Map<String, serde_json::Value>> {
    let serialized = lingua_json::to_string(arguments)?;
    Ok(serde_json::from_str(&serialized)?)
}

#[derive(Default)]
struct UniversalResponseAccumulator {
    model: Option<String>,
    usage: Option<lingua::UniversalUsage>,
    assistant_id: Option<String>,
    text: String,
    reasoning: Vec<String>,
    tool_calls: Vec<lingua::UniversalToolCallDelta>,
}

impl UniversalResponseAccumulator {
    fn push(&mut self, chunk: &UniversalStreamChunk) {
        if chunk.is_keep_alive() {
            return;
        }

        if self.model.is_none() {
            self.model = chunk.model.clone();
        }
        if self.assistant_id.is_none() {
            self.assistant_id = chunk.id.clone();
        }
        if let Some(usage) = &chunk.usage {
            self.usage = Some(usage.clone());
        }

        for choice in &chunk.choices {
            let Some(delta) = choice.delta_view() else {
                continue;
            };

            if let Some(content) = delta.content {
                self.text.push_str(&content);
            }

            for reasoning in delta.reasoning {
                if let Some(content) = reasoning.content {
                    self.reasoning.push(content);
                }
            }

            for (fallback_index, tool_call) in delta.tool_calls.into_iter().enumerate() {
                let index = tool_call
                    .index
                    .map(|value| value as usize)
                    .unwrap_or(fallback_index);
                if self.tool_calls.len() <= index {
                    self.tool_calls
                        .resize_with(index + 1, lingua::UniversalToolCallDelta::default);
                }
                let accumulator = &mut self.tool_calls[index];
                if accumulator.id.is_none() {
                    accumulator.id = tool_call.id;
                }
                if accumulator.call_type.is_none() {
                    accumulator.call_type = tool_call.call_type;
                }
                let accumulator_function = accumulator
                    .function
                    .get_or_insert_with(lingua::UniversalToolFunctionDelta::default);
                if let Some(function) = tool_call.function {
                    if accumulator_function.name.is_none() {
                        accumulator_function.name = function.name;
                    }
                    if let Some(arguments_delta) = function.arguments {
                        let current = accumulator_function.arguments.take().unwrap_or_default();
                        accumulator_function.arguments = Some(current + &arguments_delta);
                    }
                }
            }
        }
    }

    fn finalize(&self) -> UniversalResponse {
        let mut content_parts = Vec::new();

        if !self.text.is_empty() {
            content_parts.push(AssistantContentPart::Text(TextContentPart {
                text: self.text.clone(),
                encrypted_content: None,
                provider_options: None,
            }));
        }

        for reasoning in &self.reasoning {
            content_parts.push(AssistantContentPart::Reasoning {
                text: reasoning.clone(),
                encrypted_content: None,
            });
        }

        for tool_call in &self.tool_calls {
            let function = tool_call.function.clone().unwrap_or_default();
            content_parts.push(AssistantContentPart::ToolCall {
                tool_call_id: tool_call
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("tool-call-{}", content_parts.len())),
                tool_name: function.name.unwrap_or_else(|| "unknown".to_string()),
                arguments: ToolCallArguments::from(function.arguments.unwrap_or_default()),
                encrypted_content: None,
                provider_options: None,
                provider_executed: None,
            });
        }

        let assistant_message = if content_parts.is_empty() {
            None
        } else if content_parts.len() == 1 {
            match &content_parts[0] {
                AssistantContentPart::Text(text) => Some(Message::Assistant {
                    content: AssistantContent::String(text.text.clone()),
                    id: self.assistant_id.clone(),
                }),
                _ => Some(Message::Assistant {
                    content: AssistantContent::Array(content_parts),
                    id: self.assistant_id.clone(),
                }),
            }
        } else {
            Some(Message::Assistant {
                content: AssistantContent::Array(content_parts),
                id: self.assistant_id.clone(),
            })
        };

        UniversalResponse {
            id: None,
            id_format: None,
            model: self.model.clone(),
            messages: assistant_message.into_iter().collect(),
            usage: self.usage.clone(),
            finish_reason: None,
        }
    }
}
