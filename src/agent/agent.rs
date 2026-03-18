use std::sync::Arc;
use anyhow::Result;
use tracing::{info, debug, warn};

use super::provider::Provider;
use super::tool::Tool;
use super::types::{AgentConfig, ChatMessage, ContentPart, MessageContent, ToolSpec};

pub struct AgentRunResult {
    pub text: Option<String>,
    pub actions_taken: Vec<String>,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
}

pub struct AutonomousAgent {
    provider: Arc<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    config: AgentConfig,
    history: Vec<ChatMessage>,
}

impl AutonomousAgent {
    pub fn new(provider: Arc<dyn Provider>, tools: Vec<Box<dyn Tool>>, config: AgentConfig) -> Self {
        Self {
            provider,
            tools,
            config,
            history: Vec::new(),
        }
    }

    pub async fn run(&mut self, user_message: &str, system_prompt: &str) -> Result<AgentRunResult> {
        // Step 1: Clear history, push user message
        self.history.clear();
        self.history.push(ChatMessage {
            role: "user".to_string(),
            content: MessageContent::Text(user_message.to_string()),
        });

        // Step 2: Build tool specs from registered tools
        let tool_specs: Vec<ToolSpec> = self.tools.iter().map(|t| t.spec()).collect();

        let mut actions_taken: Vec<String> = Vec::new();
        let mut total_input_tokens: u32 = 0;
        let mut total_output_tokens: u32 = 0;
        let mut final_text: Option<String> = None;

        // Step 3: Loop up to config.max_iterations
        for iteration in 0..self.config.max_iterations {
            info!(iteration, "Agent loop iteration starting");
            // Step 3a: Call provider.chat()
            let response = self
                .provider
                .chat(
                    system_prompt,
                    &self.history,
                    &tool_specs,
                    &self.config.model,
                    self.config.temperature,
                )
                .await?;

            // Step 3b: Track tokens
            total_input_tokens += response.input_tokens;
            total_output_tokens += response.output_tokens;

            // Log agent reasoning text
            if let Some(ref text) = response.text {
                info!(iteration, "Agent reasoning: {}", text);
            }

            // Step 3c: If no tool_calls in response → agent is done, break
            if response.tool_calls.is_empty() {
                info!(iteration, "Agent completed - no more tool calls");
                final_text = response.text;
                break;
            }
            info!(iteration, tool_count = response.tool_calls.len(), "Agent requesting tool calls");

            // Step 3d: Build assistant message with ContentPart::Text + ContentPart::ToolUse
            let mut assistant_parts: Vec<ContentPart> = Vec::new();
            if let Some(ref text) = response.text {
                assistant_parts.push(ContentPart::Text { text: text.clone() });
            }
            for tc in &response.tool_calls {
                let input: serde_json::Value = serde_json::from_str(&tc.arguments)
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                assistant_parts.push(ContentPart::ToolUse {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    input,
                });
            }
            self.history.push(ChatMessage {
                role: "assistant".to_string(),
                content: MessageContent::Parts(assistant_parts),
            });

            // Step 3e: For each tool call: find tool by name, execute, log action
            // Step 3f: Build user message with ContentPart::ToolResult for each result
            let mut result_parts: Vec<ContentPart> = Vec::new();
            for tc in &response.tool_calls {
                let tool = self.tools.iter().find(|t| t.name() == tc.name);
                let tool_result = if let Some(tool) = tool {
                    let args: serde_json::Value = serde_json::from_str(&tc.arguments)
                        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                    info!(tool = tc.name, args = tc.arguments, "Executing tool");
                    let result = tool.execute(args).await;
                    info!(tool = tc.name, success = result.success, output_len = result.output.len(), "Tool result");
                    debug!(tool = tc.name, output = %result.output, "Tool output");
                    actions_taken.push(format!("{}({})", tc.name, tc.arguments));
                    result
                } else {
                    actions_taken.push(format!("{}({}) [tool not found]", tc.name, tc.arguments));
                    super::types::ToolResult {
                        success: false,
                        output: format!("Tool '{}' not found", tc.name),
                    }
                };

                result_parts.push(ContentPart::ToolResult {
                    tool_use_id: tc.id.clone(),
                    content: tool_result.output,
                });
            }

            // Step 3g: Push both messages to history
            self.history.push(ChatMessage {
                role: "user".to_string(),
                content: MessageContent::Parts(result_parts),
            });
        }

        // Step 4: Return AgentRunResult
        Ok(AgentRunResult {
            text: final_text,
            actions_taken,
            total_input_tokens,
            total_output_tokens,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::{ChatResponse, ToolCall, ToolResult, ToolSpec};
    use crate::agent::tool::{Tool, ToolSafety};
    use anyhow::Result;
    use serde_json::Value;

    struct MockProvider {
        responses: std::sync::Mutex<Vec<ChatResponse>>,
    }

    #[async_trait::async_trait]
    impl Provider for MockProvider {
        async fn chat(
            &self,
            _system: &str,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
            _model: &str,
            _temperature: f64,
        ) -> Result<ChatResponse> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Ok(ChatResponse {
                    text: Some("Done".to_string()),
                    tool_calls: vec![],
                    input_tokens: 0,
                    output_tokens: 0,
                })
            } else {
                Ok(responses.remove(0))
            }
        }
    }

    struct MockTool;

    #[async_trait::async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            "mock_tool"
        }

        fn description(&self) -> &str {
            "A mock tool for testing"
        }

        fn parameters_schema(&self) -> Value {
            serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            })
        }

        fn safety(&self) -> ToolSafety {
            ToolSafety::ReadOnly
        }

        async fn execute(&self, _args: Value) -> ToolResult {
            ToolResult {
                success: true,
                output: "mock result".to_string(),
            }
        }
    }

    #[tokio::test]
    async fn test_agent_runs_tool_and_completes() {
        // Provider returns 1 tool call then done
        let responses = vec![
            ChatResponse {
                text: Some("I will call a tool".to_string()),
                tool_calls: vec![ToolCall {
                    id: "tool_1".to_string(),
                    name: "mock_tool".to_string(),
                    arguments: "{}".to_string(),
                }],
                input_tokens: 10,
                output_tokens: 5,
            },
            // second call: no tool calls → done
            ChatResponse {
                text: Some("All done".to_string()),
                tool_calls: vec![],
                input_tokens: 8,
                output_tokens: 4,
            },
        ];

        let provider = Arc::new(MockProvider {
            responses: std::sync::Mutex::new(responses),
        });

        let tools: Vec<Box<dyn Tool>> = vec![Box::new(MockTool)];
        let config = AgentConfig {
            max_iterations: 10,
            ..Default::default()
        };

        let mut agent = AutonomousAgent::new(provider, tools, config);
        let result = agent.run("Do something", "You are helpful").await.unwrap();

        assert_eq!(result.actions_taken.len(), 1);
        assert!(result.actions_taken[0].contains("mock_tool"));
        assert_eq!(result.text, Some("All done".to_string()));
        assert_eq!(result.total_input_tokens, 18);
        assert_eq!(result.total_output_tokens, 9);
    }

    #[tokio::test]
    async fn test_agent_respects_max_iterations() {
        // Provider always returns a tool call
        let provider = Arc::new(MockProvider {
            responses: std::sync::Mutex::new(vec![]), // empty — falls through to always-tool-call logic
        });

        // Override with a provider that always returns tool calls
        struct AlwaysToolProvider;

        #[async_trait::async_trait]
        impl Provider for AlwaysToolProvider {
            async fn chat(
                &self,
                _system: &str,
                _messages: &[ChatMessage],
                _tools: &[ToolSpec],
                _model: &str,
                _temperature: f64,
            ) -> Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "tool_loop".to_string(),
                        name: "mock_tool".to_string(),
                        arguments: "{}".to_string(),
                    }],
                    input_tokens: 1,
                    output_tokens: 1,
                })
            }
        }

        let _ = provider; // unused, replaced by AlwaysToolProvider
        let always_provider = Arc::new(AlwaysToolProvider);
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(MockTool)];
        let config = AgentConfig {
            max_iterations: 3,
            ..Default::default()
        };

        let mut agent = AutonomousAgent::new(always_provider, tools, config);
        let result = agent.run("Loop forever", "You are helpful").await.unwrap();

        // Should stop at max_iterations (3 iterations, each with 1 tool call)
        assert_eq!(result.actions_taken.len(), 3);
        // text is None because we never got a response without tool calls
        assert_eq!(result.text, None);
    }
}
