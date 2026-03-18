//! LLM provider abstraction — trait + Anthropic / Vertex AI implementations.
//! Adapted from krust-core's provider patterns with typed response structs,
//! message conversion helpers, and OAuth2 token caching for Vertex AI.

use super::types::*;
use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::sync::Mutex as StdMutex;

// ── Provider trait ───────────────────────────────────────────────────────────

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    async fn chat(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        model: &str,
        temperature: f64,
    ) -> Result<ChatResponse>;
}

// ── Typed response structs ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
    #[serde(default)]
    usage: AnthropicUsage,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum AnthropicContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Deserialize, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

// ── Shared helpers ───────────────────────────────────────────────────────────

/// Convert a single ChatMessage's content into Anthropic API content blocks.
/// Filters out empty/whitespace-only text blocks (Anthropic/Vertex reject them).
fn convert_message_content(msg: &ChatMessage) -> Vec<serde_json::Value> {
    match &msg.content {
        MessageContent::Text(text) => {
            if text.trim().is_empty() {
                vec![]
            } else {
                vec![serde_json::json!({ "type": "text", "text": text })]
            }
        }
        MessageContent::Parts(parts) => parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } if text.trim().is_empty() => None,
                ContentPart::Text { text } => Some(serde_json::json!({
                    "type": "text",
                    "text": text,
                })),
                ContentPart::ToolUse { id, name, input } => Some(serde_json::json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": input,
                })),
                ContentPart::ToolResult {
                    tool_use_id,
                    content,
                } => Some(serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content,
                })),
            })
            .collect(),
    }
}

/// Convert ChatMessage history to Anthropic API format.
/// Merges consecutive same-role messages (required by Anthropic API:
/// messages must alternate roles). Skips system messages and empty content.
fn convert_messages(messages: &[ChatMessage]) -> Vec<serde_json::Value> {
    let mut result: Vec<serde_json::Value> = Vec::new();

    for msg in messages {
        if msg.role == "system" {
            continue;
        }

        let parts = convert_message_content(msg);
        if parts.is_empty() {
            continue;
        }

        // If the last message in result has the same role, merge content into it.
        if let Some(last) = result.last_mut() {
            if last["role"].as_str() == Some(&msg.role) {
                if let Some(arr) = last["content"].as_array_mut() {
                    arr.extend(parts);
                }
                continue;
            }
        }

        // Different role — start a new message
        result.push(serde_json::json!({
            "role": msg.role,
            "content": parts,
        }));
    }

    result
}

/// Parse a typed AnthropicResponse into our ChatResponse.
/// Used by both AnthropicProvider and VertexAnthropicProvider.
fn parse_anthropic_response(response: AnthropicResponse) -> Result<ChatResponse> {
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();

    for content in response.content {
        match content {
            AnthropicContent::Text { text } => text_parts.push(text),
            AnthropicContent::ToolUse { id, name, input } => {
                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments: serde_json::to_string(&input)
                        .unwrap_or_else(|_| "{}".to_string()),
                });
            }
        }
    }

    let text = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join("\n"))
    };

    Ok(ChatResponse {
        text,
        tool_calls,
        input_tokens: response.usage.input_tokens,
        output_tokens: response.usage.output_tokens,
    })
}

/// Build the tools array for Anthropic API format.
fn build_tools_json(tools: &[ToolSpec]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            })
        })
        .collect()
}

// ── Anthropic Messages API ───────────────────────────────────────────────────

pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
}

impl AnthropicProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_default(),
            api_key,
        }
    }

    fn build_request_body(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        model: &str,
        temperature: f64,
    ) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": model,
            "max_tokens": 4096,
            "system": system,
            "messages": convert_messages(messages),
            "temperature": temperature,
        });

        let tools_json = build_tools_json(tools);
        if !tools_json.is_empty() {
            body["tools"] = serde_json::Value::Array(tools_json);
        }

        body
    }
}

#[async_trait::async_trait]
impl Provider for AnthropicProvider {
    async fn chat(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        model: &str,
        temperature: f64,
    ) -> Result<ChatResponse> {
        let body = self.build_request_body(system, messages, tools, model, temperature);

        let max_retries = 3usize;
        let mut last_error: Option<anyhow::Error> = None;

        for attempt in 0..=max_retries {
            if attempt > 0 {
                let delay_secs = 1u64 << (attempt - 1); // 1s, 2s, 4s
                tracing::warn!(
                    attempt = attempt,
                    delay_secs = delay_secs,
                    "Retrying Anthropic API request after delay"
                );
                tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
            }

            let response = self
                .client
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await;

            match response {
                Err(e) => {
                    last_error = Some(anyhow!("Request failed: {}", e));
                    continue;
                }
                Ok(resp) => {
                    let status = resp.status();

                    if status.is_success() {
                        let resp_body = resp.text().await?;
                        let response: AnthropicResponse = serde_json::from_str(&resp_body)?;
                        return parse_anthropic_response(response);
                    }

                    // Rate limit or server error — retry
                    if status.as_u16() == 429 || status.is_server_error() {
                        let err_text = resp.text().await.unwrap_or_default();
                        last_error = Some(anyhow!(
                            "API error (status {}): {}",
                            status.as_u16(),
                            err_text
                        ));
                        continue;
                    }

                    // Other 4xx — do not retry
                    let err_text = resp.text().await.unwrap_or_default();
                    return Err(anyhow!(
                        "Anthropic API client error (status {}): {}",
                        status.as_u16(),
                        err_text
                    ));
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("Anthropic API request failed after retries")))
    }
}

// ── Vertex AI provider (Anthropic API on Google Cloud) ───────────────────────

/// Cached OAuth2 access token with expiry.
struct CachedToken {
    token: String,
    /// Epoch seconds when this token expires (with 60s safety margin).
    expires_at: u64,
}

pub struct VertexAnthropicProvider {
    client: reqwest::Client,
    region: String,
    project_id: String,
    token_cache: StdMutex<Option<CachedToken>>,
}

impl VertexAnthropicProvider {
    pub fn new(region: &str, project_id: &str) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_default(),
            region: region.into(),
            project_id: project_id.into(),
            token_cache: StdMutex::new(None),
        }
    }

    /// Build the Vertex AI rawPredict URL for a given model.
    fn build_url(&self, model: &str) -> String {
        format!(
            "https://{}-aiplatform.googleapis.com/v1/projects/{}/locations/{}/publishers/anthropic/models/{}:rawPredict",
            self.region, self.project_id, self.region, model
        )
    }

    /// Get access token, returning cached version if still valid.
    async fn get_access_token(&self) -> Result<String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Check cache
        if let Ok(cache) = self.token_cache.lock() {
            if let Some(ref cached) = *cache {
                if now < cached.expires_at {
                    return Ok(cached.token.clone());
                }
            }
        }

        // Refresh token
        let (token, expires_in) = Self::refresh_token(&self.client).await?;

        // Cache with 60s safety margin
        let expires_at = now + expires_in.saturating_sub(60);
        if let Ok(mut cache) = self.token_cache.lock() {
            *cache = Some(CachedToken {
                token: token.clone(),
                expires_at,
            });
        }

        Ok(token)
    }

    /// Refresh OAuth2 token from ADC credentials file.
    async fn refresh_token(client: &reqwest::Client) -> Result<(String, u64)> {
        let home = std::env::var("HOME").unwrap_or_default();
        let adc_path = format!("{home}/.config/gcloud/application_default_credentials.json");
        let adc_content = std::fs::read_to_string(&adc_path).map_err(|_| {
            anyhow!(
                "ADC credentials not found at {adc_path}. Run: gcloud auth application-default login"
            )
        })?;

        let creds: serde_json::Value = serde_json::from_str(&adc_content)?;
        let client_id = creds["client_id"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing client_id in ADC file"))?;
        let client_secret = creds["client_secret"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing client_secret in ADC file"))?;
        let refresh_token = creds["refresh_token"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing refresh_token in ADC file"))?;

        let resp = client
            .post("https://oauth2.googleapis.com/token")
            .form(&[
                ("client_id", client_id),
                ("client_secret", client_secret),
                ("refresh_token", refresh_token),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await?;

        let status = resp.status();
        let body: serde_json::Value = resp.json().await?;

        if !status.is_success() {
            let err_msg = body["error_description"]
                .as_str()
                .or(body["error"].as_str())
                .unwrap_or("Unknown error");
            anyhow::bail!(
                "Token refresh failed: {err_msg}. Run: gcloud auth application-default login"
            );
        }

        let token = body["access_token"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("No access_token in response"))?;
        let expires_in = body["expires_in"].as_u64().unwrap_or(3600);

        Ok((token, expires_in))
    }
}

#[async_trait::async_trait]
impl Provider for VertexAnthropicProvider {
    async fn chat(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        model: &str,
        temperature: f64,
    ) -> Result<ChatResponse> {
        let token = self.get_access_token().await?;
        let url = self.build_url(model);

        let tools_json = build_tools_json(tools);

        // Vertex: anthropic_version in body, model NOT in body (it's in the URL)
        let mut body = serde_json::json!({
            "anthropic_version": "vertex-2023-10-16",
            "max_tokens": 4096,
            "system": system,
            "messages": convert_messages(messages),
            "temperature": temperature,
        });
        if !tools_json.is_empty() {
            body["tools"] = serde_json::Value::Array(tools_json);
        }

        let max_retries = 3usize;
        let mut last_error: Option<anyhow::Error> = None;

        for attempt in 0..=max_retries {
            if attempt > 0 {
                let delay_secs = 1u64 << (attempt - 1);
                tracing::warn!(
                    attempt = attempt,
                    delay_secs = delay_secs,
                    "Retrying Vertex AI API request after delay"
                );
                tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
            }

            let response = self
                .client
                .post(&url)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {token}"))
                .json(&body)
                .send()
                .await;

            match response {
                Err(e) => {
                    last_error = Some(anyhow!("Request failed: {}", e));
                    continue;
                }
                Ok(resp) => {
                    let status = resp.status();

                    if status.is_success() {
                        let resp_body = resp.text().await?;
                        let response: AnthropicResponse = serde_json::from_str(&resp_body)?;
                        return parse_anthropic_response(response);
                    }

                    // Rate limit or server error — retry
                    if status.as_u16() == 429 || status.is_server_error() {
                        let err_text = resp.text().await.unwrap_or_default();
                        last_error = Some(anyhow!(
                            "Vertex AI error (status {}): {}",
                            status.as_u16(),
                            err_text
                        ));
                        continue;
                    }

                    // Other 4xx — do not retry
                    let err_text = resp.text().await.unwrap_or_default();
                    return Err(anyhow!(
                        "Vertex AI client error (status {}): {}",
                        status.as_u16(),
                        err_text
                    ));
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("Vertex AI request failed after retries")))
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_provider() -> AnthropicProvider {
        AnthropicProvider::new("test-key".to_string())
    }

    #[test]
    fn test_anthropic_provider_builds_request_body() {
        let provider = make_provider();

        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: MessageContent::Text("Hello".to_string()),
        }];

        let tools = vec![ToolSpec {
            name: "my_tool".to_string(),
            description: "A test tool".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "input": { "type": "string" }
                },
                "required": ["input"]
            }),
        }];

        let body =
            provider.build_request_body("You are helpful.", &messages, &tools, "claude-3-opus", 0.5);

        assert_eq!(body["model"], "claude-3-opus");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["system"], "You are helpful.");
        assert_eq!(body["temperature"], 0.5);

        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");

        let tools_json = body["tools"].as_array().unwrap();
        assert_eq!(tools_json.len(), 1);
        assert_eq!(tools_json[0]["name"], "my_tool");
        assert_eq!(tools_json[0]["description"], "A test tool");
        assert!(tools_json[0]["input_schema"].is_object());
    }

    #[test]
    fn test_parse_response_text_only() {
        let response = AnthropicResponse {
            content: vec![AnthropicContent::Text {
                text: "Hello, world!".to_string(),
            }],
            usage: AnthropicUsage {
                input_tokens: 10,
                output_tokens: 5,
            },
        };

        let result = parse_anthropic_response(response).unwrap();
        assert_eq!(result.text, Some("Hello, world!".to_string()));
        assert!(result.tool_calls.is_empty());
        assert_eq!(result.input_tokens, 10);
        assert_eq!(result.output_tokens, 5);
    }

    #[test]
    fn test_parse_response_with_tool_use() {
        let response_json = json!({
            "content": [
                {
                    "type": "text",
                    "text": "I will use a tool."
                },
                {
                    "type": "tool_use",
                    "id": "toolu_abc123",
                    "name": "get_weather",
                    "input": {
                        "location": "San Francisco",
                        "unit": "celsius"
                    }
                }
            ],
            "usage": {
                "input_tokens": 25,
                "output_tokens": 15
            }
        });

        let response: AnthropicResponse = serde_json::from_value(response_json).unwrap();
        let result = parse_anthropic_response(response).unwrap();
        assert_eq!(result.text, Some("I will use a tool.".to_string()));
        assert_eq!(result.tool_calls.len(), 1);

        let tc = &result.tool_calls[0];
        assert_eq!(tc.id, "toolu_abc123");
        assert_eq!(tc.name, "get_weather");

        let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap();
        assert_eq!(args["location"], "San Francisco");
        assert_eq!(args["unit"], "celsius");

        assert_eq!(result.input_tokens, 25);
        assert_eq!(result.output_tokens, 15);
    }

    #[test]
    fn test_vertex_builds_correct_url() {
        let provider = VertexAnthropicProvider::new("us-central1", "my-project");
        let url = provider.build_url("claude-sonnet-4-20250514");
        assert_eq!(
            url,
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/us-central1/publishers/anthropic/models/claude-sonnet-4-20250514:rawPredict"
        );
    }

    #[test]
    fn test_convert_messages_merges_same_role() {
        let messages = vec![
            ChatMessage {
                role: "user".to_string(),
                content: MessageContent::Text("Hello".to_string()),
            },
            ChatMessage {
                role: "user".to_string(),
                content: MessageContent::Text("World".to_string()),
            },
        ];

        let result = convert_messages(&messages);
        assert_eq!(result.len(), 1, "consecutive same-role messages should merge");
        assert_eq!(result[0]["role"], "user");

        let content = result[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["text"], "Hello");
        assert_eq!(content[1]["text"], "World");
    }

    #[test]
    fn test_convert_messages_filters_empty_text() {
        let messages = vec![
            ChatMessage {
                role: "user".to_string(),
                content: MessageContent::Text("".to_string()),
            },
            ChatMessage {
                role: "user".to_string(),
                content: MessageContent::Text("  ".to_string()),
            },
            ChatMessage {
                role: "user".to_string(),
                content: MessageContent::Text("actual content".to_string()),
            },
        ];

        let result = convert_messages(&messages);
        assert_eq!(result.len(), 1);
        let content = result[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], "actual content");
    }
}
