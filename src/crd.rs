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

/// Configuration for the AI agent sidecar.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct AgentSpec {
    /// Whether the agent is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Whether the agent should perform self-healing actions.
    #[serde(default)]
    pub self_healing: bool,
    /// Interval in seconds between health checks.
    #[serde(default = "default_health_check_interval")]
    pub health_check_interval: u32,
    /// Factor by which the agent may scale memory limits.
    #[serde(default = "default_max_memory_scale_factor")]
    pub max_memory_scale_factor: f64,
    /// LLM provider: "vertex" (default) or "anthropic".
    #[serde(default = "default_provider")]
    pub provider: String,
    /// GCP region for Vertex AI (e.g. "us-central1").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// GCP project ID for Vertex AI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Reference to a secret containing the agent's API key (required for anthropic provider).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_secret_ref: Option<SecretKeyRef>,
}

fn default_provider() -> String {
    "vertex".into()
}

fn default_health_check_interval() -> u32 {
    30
}

fn default_max_memory_scale_factor() -> f64 {
    2.0
}

impl Default for AgentSpec {
    fn default() -> Self {
        Self {
            enabled: false,
            self_healing: false,
            health_check_interval: default_health_check_interval(),
            max_memory_scale_factor: default_max_memory_scale_factor(),
            provider: default_provider(),
            region: None,
            project_id: None,
            api_key_secret_ref: None,
        }
    }
}

/// Desired state of a ValkeyCluster.
#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[kube(
    group = "valkey.krust.io",
    version = "v1alpha1",
    kind = "ValkeyCluster",
    namespaced,
    status = "ValkeyClusterStatus",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Masters","type":"integer","jsonPath":".spec.masters"}"#,
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyNodes"}"#
)]
pub struct ValkeyClusterSpec {
    /// Valkey version to deploy (e.g. "7.2").
    pub version: String,
    /// Number of master nodes.
    pub masters: u32,
    /// Number of replicas per master.
    #[serde(default)]
    pub replicas_per_master: u32,
    /// Resource requirements for each Valkey pod.
    #[serde(default)]
    pub resources: ResourceRequirements,
    /// Override the default container image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// AI agent configuration.
    #[serde(default)]
    pub agent: AgentSpec,
}

impl ValkeyClusterSpec {
    /// Returns the total number of pods (masters + all replicas).
    pub fn total_pods(&self) -> u32 {
        self.masters * (1 + self.replicas_per_master)
    }

    /// Returns the container image, using the override if provided or the default valkey image.
    pub fn image(&self) -> String {
        match &self.image {
            Some(img) => img.clone(),
            None => format!("valkey/valkey:{}", self.version),
        }
    }
}

/// Phase of the ValkeyCluster lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum ClusterPhase {
    Pending,
    Initializing,
    Running,
    Healing,
    Failed,
}

impl ClusterPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            ClusterPhase::Pending => "Pending",
            ClusterPhase::Initializing => "Initializing",
            ClusterPhase::Running => "Running",
            ClusterPhase::Healing => "Healing",
            ClusterPhase::Failed => "Failed",
        }
    }
}

impl std::fmt::Display for ClusterPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single condition on the ValkeyCluster.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ClusterCondition {
    /// Type of the condition (e.g. "Ready", "Healing").
    pub condition_type: String,
    /// Status of the condition ("True", "False", "Unknown").
    pub status: String,
}

/// Observed state of a ValkeyCluster.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
pub struct ValkeyClusterStatus {
    /// High-level lifecycle phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<ClusterPhase>,
    /// Detailed cluster state string reported by Valkey.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_state: Option<String>,
    /// Number of master nodes currently recognised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub masters: Option<u32>,
    /// Number of pods that are ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_nodes: Option<u32>,
    /// Description of the last action taken by the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_agent_action: Option<String>,
    /// Timestamp of the last agent action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_agent_action_time: Option<String>,
    /// Timestamp of the last health check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_health_check: Option<String>,
    /// List of conditions on this cluster.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<ClusterCondition>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_spec(masters: u32, replicas_per_master: u32, version: &str) -> ValkeyClusterSpec {
        ValkeyClusterSpec {
            version: version.to_string(),
            masters,
            replicas_per_master,
            resources: ResourceRequirements::default(),
            image: None,
            agent: AgentSpec::default(),
        }
    }

    #[test]
    fn test_crd_default_values() {
        let spec = make_spec(3, 1, "7.2");
        assert_eq!(spec.total_pods(), 6, "3 masters * (1 + 1 replica) should equal 6");
    }

    #[test]
    fn test_crd_image_override() {
        let mut spec = make_spec(1, 0, "7.2");
        // Without override, should use the default image.
        assert_eq!(spec.image(), "valkey/valkey:7.2");

        // With override, should return the custom image.
        spec.image = Some("my-registry/valkey:custom".to_string());
        assert_eq!(spec.image(), "my-registry/valkey:custom");
    }

    #[test]
    fn test_phase_display() {
        assert_eq!(ClusterPhase::Pending.as_str(), "Pending");
        assert_eq!(ClusterPhase::Initializing.as_str(), "Initializing");
        assert_eq!(ClusterPhase::Running.as_str(), "Running");
        assert_eq!(ClusterPhase::Healing.as_str(), "Healing");
        assert_eq!(ClusterPhase::Failed.as_str(), "Failed");

        // Also verify Display impl delegates to as_str.
        assert_eq!(format!("{}", ClusterPhase::Running), "Running");
    }
}
