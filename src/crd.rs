use std::collections::HashMap;

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Reference to a key within a Kubernetes Secret.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct SecretKeyRef {
    /// Name of the secret.
    pub name: String,
    /// Key within the secret.
    pub key: String,
}

/// CPU/memory resource specification.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
pub struct ResourceSpec {
    /// Memory amount (e.g. "512Mi", "1Gi").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    /// CPU amount (e.g. "500m", "1").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<String>,
}

/// Kubernetes-style resource requirements with requests and limits.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
pub struct ResourceRequirements {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<ResourceSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<ResourceSpec>,
}

/// Configuration for the AI agent.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct AgentSpec {
    /// Whether the agent is enabled.
    #[serde(default = "default_agent_enabled")]
    pub enabled: bool,
    /// LLM provider (e.g. "anthropic", "vertex").
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Cloud region for the provider (e.g. "us-central1").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Cloud project ID (e.g. for Vertex AI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Model name/ID override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Maximum number of agent iterations per reconciliation loop.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    /// Total timeout for a single agent pipeline run (e.g. "300s").
    #[serde(default = "default_pipeline_timeout")]
    pub pipeline_timeout: String,
    /// Timeout for a single LLM call (e.g. "60s").
    #[serde(default = "default_llm_call_timeout")]
    pub llm_call_timeout: String,
    /// Reference to a secret containing the agent's API key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_secret_ref: Option<SecretKeyRef>,
}

fn default_agent_enabled() -> bool {
    true
}

fn default_provider() -> String {
    "anthropic".into()
}

fn default_max_iterations() -> u32 {
    30
}

fn default_pipeline_timeout() -> String {
    "300s".into()
}

fn default_llm_call_timeout() -> String {
    "60s".into()
}

impl Default for AgentSpec {
    fn default() -> Self {
        Self {
            enabled: default_agent_enabled(),
            provider: default_provider(),
            region: None,
            project_id: None,
            model: None,
            max_iterations: default_max_iterations(),
            pipeline_timeout: default_pipeline_timeout(),
            llm_call_timeout: default_llm_call_timeout(),
            api_key_secret_ref: None,
        }
    }
}

/// Guardrails that constrain what the AI agent is allowed to do.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
pub struct GuardrailSpec {
    /// Maximum number of replicas the agent may create.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_replicas: Option<u32>,
    /// Maximum memory the agent may allocate (e.g. "4Gi").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory: Option<String>,
    /// Shell commands the agent is not allowed to execute.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied_commands: Vec<String>,
}

/// Desired state of an AIResource.
#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[kube(
    group = "krust.io",
    version = "v1",
    kind = "AIResource",
    namespaced,
    status = "AIResourceStatus",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Skill","type":"string","jsonPath":".spec.skill"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
pub struct AIResourceSpec {
    /// The skill (capability) this resource exercises (e.g. "valkey-cluster").
    pub skill: String,
    /// High-level goal the agent should achieve.
    pub goal: String,
    /// Container image to run.
    pub image: String,
    /// CPU/memory resource requirements for the workload pod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
    /// AI agent configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentSpec>,
    /// Guardrails that constrain agent actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guardrails: Option<GuardrailSpec>,
}

/// Phase of the AIResource lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum ResourcePhase {
    Pending,
    Initializing,
    Running,
    Healing,
    Failed,
}

impl ResourcePhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            ResourcePhase::Pending => "Pending",
            ResourcePhase::Initializing => "Initializing",
            ResourcePhase::Running => "Running",
            ResourcePhase::Healing => "Healing",
            ResourcePhase::Failed => "Failed",
        }
    }
}

impl std::fmt::Display for ResourcePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single condition on the AIResource.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ResourceCondition {
    /// Type of the condition (e.g. "Ready", "Healing").
    #[serde(rename = "type")]
    pub condition_type: String,
    /// Status of the condition ("True", "False", "Unknown").
    pub status: String,
    /// Machine-readable reason for the condition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Human-readable message about the condition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Observed state of an AIResource.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
pub struct AIResourceStatus {
    /// High-level lifecycle phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<ResourcePhase>,
    /// Human-readable status message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Description of the last action taken by the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_agent_action: Option<String>,
    /// Timestamp of the last agent action (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_agent_action_time: Option<String>,
    /// Arbitrary key-value state reported by the agent's monitor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor_state: Option<HashMap<String, serde_json::Value>>,
    /// List of conditions on this resource.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<ResourceCondition>,
}
