use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use anyhow::Result;
use tracing::{info, debug, warn};

use super::provider::Provider;
use super::tool::Tool;
use super::types::{AgentConfig, ChatMessage, ContentPart, MessageContent, ToolSpec};

// ── History compaction constants ─────────────────────────────────────────────

/// Trigger compaction when message count exceeds this threshold.
const COMPACTION_MAX_MESSAGES: usize = 40;

/// Keep this many most-recent messages after compaction.
const COMPACTION_KEEP_RECENT: usize = 16;

/// Max chars of source transcript sent to the summarizer.
const COMPACTION_MAX_SOURCE_CHARS: usize = 12_000;

/// Max chars retained in the compaction summary.
const COMPACTION_MAX_SUMMARY_CHARS: usize = 2_000;

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
        self.history.clear();
        self.history.push(ChatMessage {
            role: "user".to_string(),
            content: MessageContent::Text(user_message.to_string()),
        });

        let tool_specs: Vec<ToolSpec> = self.tools.iter().map(|t| t.spec()).collect();

        let mut actions_taken: Vec<String> = Vec::new();
        let mut total_input_tokens: u32 = 0;
        let mut total_output_tokens: u32 = 0;
        let mut final_text: Option<String> = None;

        // Loop detection state
        let mut consecutive_identical_outputs: usize = 0;
        let mut last_tool_output_hash: Option<u64> = None;

        for iteration in 0..self.config.max_iterations {
            info!(iteration, history_len = self.history.len(), "Agent loop iteration starting");

            // History compaction: summarize older messages when history grows too large
            if self.history.len() > COMPACTION_MAX_MESSAGES {
                match self.compact_history(system_prompt).await {
                    Ok(true) => info!(iteration, new_len = self.history.len(), "History compacted"),
                    Ok(false) => {}
                    Err(e) => warn!(iteration, "History compaction failed (continuing): {}", e),
                }
            }

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

            total_input_tokens += response.input_tokens;
            total_output_tokens += response.output_tokens;

            if let Some(ref text) = response.text {
                info!(iteration, "Agent reasoning: {}", text);
            }

            if response.tool_calls.is_empty() {
                info!(iteration, "Agent completed - no more tool calls");
                final_text = response.text;
                break;
            }
            info!(iteration, tool_count = response.tool_calls.len(), "Agent requesting tool calls");

            // Build assistant message
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

            // Execute tool calls with per-turn deduplication
            let mut result_parts: Vec<ContentPart> = Vec::new();
            let mut seen_signatures: HashSet<u64> = HashSet::new();
            let mut combined_output = String::new();

            for tc in &response.tool_calls {
                // Dedup: skip identical tool+args in the same turn
                let mut sig_hasher = DefaultHasher::new();
                tc.name.hash(&mut sig_hasher);
                tc.arguments.hash(&mut sig_hasher);
                let sig = sig_hasher.finish();

                if !seen_signatures.insert(sig) {
                    warn!(tool = tc.name, "Skipping duplicate tool call in same turn");
                    result_parts.push(ContentPart::ToolResult {
                        tool_use_id: tc.id.clone(),
                        content: format!("Skipped: duplicate call to '{}' with identical arguments. Try a different approach.", tc.name),
                    });
                    continue;
                }

                let tool = self.tools.iter().find(|t| t.name() == tc.name);
                let tool_result = if let Some(tool) = tool {
                    let args: serde_json::Value = serde_json::from_str(&tc.arguments)
                        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                    info!(tool = tc.name, args = tc.arguments, "Executing tool");
                    let result = tool.execute(args).await;
                    info!(tool = tc.name, success = result.success, output_len = result.output.len(), "Tool result");
                    debug!(tool = tc.name, output = %result.output, "Tool output");
                    actions_taken.push(format!("{}({})", tc.name, tc.arguments));
                    combined_output.push_str(&result.output);
                    result
                } else {
                    actions_taken.push(format!("{}({}) [tool not found]", tc.name, tc.arguments));
                    super::types::ToolResult {
                        success: false,
                        output: format!("Tool '{}' not found. Available tools: {}",
                            tc.name,
                            self.tools.iter().map(|t| t.name()).collect::<Vec<_>>().join(", ")),
                    }
                };

                result_parts.push(ContentPart::ToolResult {
                    tool_use_id: tc.id.clone(),
                    content: tool_result.output,
                });
            }

            // Loop detection: hash combined tool outputs, abort after 3 identical rounds
            if !combined_output.is_empty() {
                let mut hasher = DefaultHasher::new();
                combined_output.hash(&mut hasher);
                let current_hash = hasher.finish();

                if last_tool_output_hash == Some(current_hash) {
                    consecutive_identical_outputs += 1;
                    warn!(
                        iteration,
                        consecutive = consecutive_identical_outputs,
                        "Identical tool output detected"
                    );
                } else {
                    consecutive_identical_outputs = 0;
                    last_tool_output_hash = Some(current_hash);
                }

                if consecutive_identical_outputs >= 3 {
                    warn!("Agent loop aborted: identical tool output 3 consecutive times — agent is stuck");
                    final_text = Some("Agent detected it was stuck in a loop producing identical results. Aborting to prevent wasted resources.".to_string());
                    break;
                }
            }

            self.history.push(ChatMessage {
                role: "user".to_string(),
                content: MessageContent::Parts(result_parts),
            });
        }

        Ok(AgentRunResult {
            text: final_text,
            actions_taken,
            total_input_tokens,
            total_output_tokens,
        })
    }

    // ── History compaction ────────────────────────────────────────────────────
    // When conversation history grows too long, summarize older messages into
    // a compact summary and replace them. Preserves the first user message
    // (original goal/state) and keeps recent messages intact.

    async fn compact_history(&mut self, _system_prompt: &str) -> Result<bool> {
        let total = self.history.len();
        if total <= COMPACTION_KEEP_RECENT + 1 {
            return Ok(false); // not enough to compact
        }

        // Always preserve the first message (original goal/state)
        // Compact messages between [1..compact_end], keep [compact_end..] as recent
        let compact_end = total.saturating_sub(COMPACTION_KEEP_RECENT);
        if compact_end <= 1 {
            return Ok(false);
        }

        // Snap to user-turn boundary so we don't split mid-conversation
        let mut end = compact_end;
        while end > 1 && self.history[end].role != "user" {
            end -= 1;
        }
        if end <= 1 {
            return Ok(false);
        }

        // Build transcript from messages to compact
        let transcript = build_compaction_transcript(&self.history[1..end]);
        info!(
            messages_to_compact = end - 1,
            transcript_chars = transcript.len(),
            "Compacting history"
        );

        // Use LLM to summarize
        let summarizer_prompt = "You are a conversation compaction engine for a Kubernetes operator agent. \
            Summarize the older conversation history into concise context. \
            Preserve: decisions made, actions taken (tool calls and results), errors encountered, current cluster state, passwords/secrets discovered. \
            Omit: verbose tool output logs, repeated status checks, redundant information. \
            Output plain text bullet points only. Be concise but preserve all critical operational context.";

        let summarizer_message = format!(
            "Summarize this agent conversation history (max 15 bullet points):\n\n{}",
            transcript
        );

        let summary_messages = vec![
            ChatMessage {
                role: "user".to_string(),
                content: MessageContent::Text(summarizer_message),
            },
        ];

        let summary = match self.provider.chat(
            summarizer_prompt,
            &summary_messages,
            &[], // no tools for summarization
            &self.config.model,
            0.0,
        ).await {
            Ok(resp) => {
                let text = resp.text.unwrap_or_default();
                truncate_str(&text, COMPACTION_MAX_SUMMARY_CHARS)
            }
            Err(e) => {
                // Fallback: deterministic truncation if LLM summarization fails
                warn!("LLM summarization failed, using truncation fallback: {}", e);
                truncate_str(&transcript, COMPACTION_MAX_SUMMARY_CHARS)
            }
        };

        // Replace compacted messages with a single summary message
        let summary_msg = ChatMessage {
            role: "user".to_string(),
            content: MessageContent::Text(format!(
                "[Compaction summary of previous {} messages]\n{}",
                end - 1,
                summary.trim()
            )),
        };

        // Keep first message + replace [1..end] with summary + keep [end..]
        self.history.splice(1..end, std::iter::once(summary_msg));

        info!(new_history_len = self.history.len(), "History compaction complete");
        Ok(true)
    }
}

// ── Helper functions ─────────────────────────────────────────────────────────

/// Build a text transcript from chat messages for summarization.
fn build_compaction_transcript(messages: &[ChatMessage]) -> String {
    let mut transcript = String::new();
    for msg in messages {
        let role = msg.role.to_uppercase();
        let content = match &msg.content {
            MessageContent::Text(t) => t.clone(),
            MessageContent::Parts(parts) => {
                parts.iter().map(|p| match p {
                    ContentPart::Text { text } => text.clone(),
                    ContentPart::ToolUse { name, input, .. } => {
                        format!("[Tool call: {}({})]", name, input)
                    }
                    ContentPart::ToolResult { content, .. } => {
                        // Truncate verbose tool outputs
                        if content.len() > 500 {
                            format!("[Tool result: {}...]", &content[..500])
                        } else {
                            format!("[Tool result: {}]", content)
                        }
                    }
                }).collect::<Vec<_>>().join("\n")
            }
        };
        transcript.push_str(&format!("{}: {}\n", role, content));
    }

    if transcript.len() > COMPACTION_MAX_SOURCE_CHARS {
        truncate_str(&transcript, COMPACTION_MAX_SOURCE_CHARS)
    } else {
        transcript
    }
}

/// Truncate a string to max_chars, appending "..." if truncated.
fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        s.to_string()
    } else {
        format!("{}...", &s[..max_chars.saturating_sub(3)])
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
