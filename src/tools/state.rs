use std::sync::{Arc, Mutex};

use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::Client;
use serde_json::{json, Value};
use tracing::info;

use crate::agent::tool::{Tool, ToolSafety};
use crate::agent::types::ToolResult;
use crate::crd::AIResource;
use crate::monitor::registry::MonitorRegistry;
use crate::types::{K8sState, PodInfo, ResourceInfo, StatefulSetInfo, StateSnapshot, TriggerInfo};

// ---------------------------------------------------------------------------
// GetState — get current state: monitor data + K8s resources
// ---------------------------------------------------------------------------

pub struct GetState {
    client: Client,
    namespace: String,
    resource_name: String,
    skill_name: String,
    goal: String,
    image: String,
    monitor_registry: Arc<Mutex<MonitorRegistry>>,
}

impl GetState {
    pub fn new(
        client: Client,
        namespace: &str,
        resource_name: &str,
        skill_name: &str,
        goal: &str,
        image: &str,
        monitor_registry: Arc<Mutex<MonitorRegistry>>,
    ) -> Self {
        Self {
            client,
            namespace: namespace.to_string(),
            resource_name: resource_name.to_string(),
            skill_name: skill_name.to_string(),
            goal: goal.to_string(),
            image: image.to_string(),
            monitor_registry,
        }
    }
}

#[async_trait::async_trait]
impl Tool for GetState {
    fn name(&self) -> &str { "get_state" }

    fn description(&self) -> &str {
        "Get current state: monitor data + K8s resources."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, _args: Value) -> ToolResult {
        // Query K8s for pods with label selector app={resource_name}
        let pod_api: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let label_selector = format!("app={}", self.resource_name);
        let lp = ListParams::default().labels(&label_selector);

        let pods = match pod_api.list(&lp).await {
            Ok(list) => list.items.into_iter().map(|pod| {
                let name = pod.metadata.name.clone().unwrap_or_default();
                let status = pod.status.as_ref();
                let phase = status
                    .and_then(|s| s.phase.as_deref())
                    .unwrap_or("Unknown")
                    .to_string();
                let ready = status
                    .and_then(|s| s.conditions.as_ref())
                    .map(|conds| conds.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
                    .unwrap_or(false);
                let restarts: u32 = status
                    .and_then(|s| s.container_statuses.as_ref())
                    .map(|cs| cs.iter().map(|c| c.restart_count as u32).sum())
                    .unwrap_or(0);
                PodInfo { name, phase, ready, restarts }
            }).collect::<Vec<_>>(),
            Err(e) => {
                return ToolResult {
                    success: false,
                    output: format!("error querying pods: {}", e),
                };
            }
        };

        // Query StatefulSets with label selector app={resource_name}
        let sts_api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let statefulsets = match sts_api.list(&lp).await {
            Ok(list) => list.items.into_iter().map(|sts| {
                let name = sts.metadata.name.clone().unwrap_or_default();
                let spec = sts.spec.as_ref();
                let status = sts.status.as_ref();
                let replicas = spec.and_then(|s| s.replicas).unwrap_or(0) as u32;
                let ready_replicas = status.and_then(|s| s.ready_replicas).unwrap_or(0) as u32;
                let memory_limit = spec
                    .and_then(|s| s.template.spec.as_ref())
                    .and_then(|ps| ps.containers.first())
                    .and_then(|c| c.resources.as_ref())
                    .and_then(|r| r.limits.as_ref())
                    .and_then(|l| l.get("memory"))
                    .map(|q| q.0.clone());
                let cpu_limit = spec
                    .and_then(|s| s.template.spec.as_ref())
                    .and_then(|ps| ps.containers.first())
                    .and_then(|c| c.resources.as_ref())
                    .and_then(|r| r.limits.as_ref())
                    .and_then(|l| l.get("cpu"))
                    .map(|q| q.0.clone());
                StatefulSetInfo { name, replicas, ready_replicas, memory_limit, cpu_limit }
            }).collect::<Vec<_>>(),
            Err(e) => {
                return ToolResult {
                    success: false,
                    output: format!("error querying statefulsets: {}", e),
                };
            }
        };

        // Get monitor state from registry
        let monitor_state = {
            let registry = self.monitor_registry.lock().unwrap();
            registry.get_state(&self.namespace, &self.resource_name)
        };

        let snapshot = StateSnapshot {
            resource: ResourceInfo {
                name: self.resource_name.clone(),
                namespace: self.namespace.clone(),
                skill: self.skill_name.clone(),
                goal: self.goal.clone(),
                image: self.image.clone(),
            },
            monitors: monitor_state,
            k8s: K8sState { pods, statefulsets },
            trigger: TriggerInfo {
                source: "agent".to_string(),
                reason: "get_state called".to_string(),
                timestamp: chrono::Utc::now().to_rfc3339(),
            },
        };

        match serde_json::to_string_pretty(&snapshot) {
            Ok(s) => ToolResult { success: true, output: s },
            Err(e) => ToolResult { success: false, output: format!("serialization error: {}", e) },
        }
    }
}

// ---------------------------------------------------------------------------
// UpdateStatus — update AIResource status (phase, message)
// ---------------------------------------------------------------------------

pub struct UpdateStatus {
    client: Client,
    namespace: String,
    resource_name: String,
}

impl UpdateStatus {
    pub fn new(client: Client, namespace: &str, resource_name: &str) -> Self {
        Self {
            client,
            namespace: namespace.to_string(),
            resource_name: resource_name.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for UpdateStatus {
    fn name(&self) -> &str { "update_status" }

    fn description(&self) -> &str {
        "Update AIResource status (phase, message)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "phase": {
                    "type": "string",
                    "enum": ["Pending", "Initializing", "Running", "Healing", "Failed"],
                    "description": "Lifecycle phase to set"
                },
                "message": {
                    "type": "string",
                    "description": "Optional human-readable status message"
                }
            },
            "required": ["phase"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::Validated }

    async fn execute(&self, args: Value) -> ToolResult {
        let phase = match args["phase"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing phase".into() },
        };

        // Validate phase
        match phase {
            "Pending" | "Initializing" | "Running" | "Healing" | "Failed" => {}
            other => return ToolResult {
                success: false,
                output: format!("invalid phase '{}': must be one of Pending/Initializing/Running/Healing/Failed", other),
            },
        }

        let message = args["message"].as_str();

        let api: Api<AIResource> = Api::namespaced(self.client.clone(), &self.namespace);

        let mut status_patch = json!({
            "status": {
                "phase": phase,
                "lastAgentActionTime": chrono::Utc::now().to_rfc3339(),
            }
        });

        if let Some(msg) = message {
            status_patch["status"]["message"] = json!(msg);
        }

        let pp = PatchParams::apply("valkey-ai-operator").force();
        let patch = Patch::Merge(&status_patch);

        match api.patch_status(&self.resource_name, &pp, &patch).await {
            Ok(_) => {
                info!("update_status: {}/{} -> phase={}", self.namespace, self.resource_name, phase);
                let output = json!({ "updated": true, "phase": phase });
                ToolResult { success: true, output: output.to_string() }
            }
            Err(e) => ToolResult { success: false, output: format!("error: {}", e) },
        }
    }
}
