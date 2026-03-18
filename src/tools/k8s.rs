use std::sync::Arc;

use anyhow::Context;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{ConfigMap, Event, Pod, Service};
use kube::api::{Api, ListParams, LogParams, Patch, PatchParams};
use kube::Client;
use serde_json::{json, Value};
use tracing::info;

use crate::agent::tool::{Tool, ToolSafety};
use crate::agent::types::ToolResult;

// ---------------------------------------------------------------------------
// Guardrail helpers
// ---------------------------------------------------------------------------

/// Parse a memory string like "3Gi", "512Mi", "1G", "1M", or plain bytes to bytes.
pub fn parse_memory_to_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.ends_with("Gi") {
        s.strip_suffix("Gi")?.parse::<u64>().ok().map(|v| v * 1024 * 1024 * 1024)
    } else if s.ends_with("Mi") {
        s.strip_suffix("Mi")?.parse::<u64>().ok().map(|v| v * 1024 * 1024)
    } else if s.ends_with("Ki") {
        s.strip_suffix("Ki")?.parse::<u64>().ok().map(|v| v * 1024)
    } else if s.ends_with('G') {
        s.strip_suffix('G')?.parse::<u64>().ok().map(|v| v * 1_000_000_000)
    } else if s.ends_with('M') {
        s.strip_suffix('M')?.parse::<u64>().ok().map(|v| v * 1_000_000)
    } else {
        s.parse::<u64>().ok()
    }
}

/// Reject if `requested` exceeds `spec_limit * scale_factor`.
pub fn validate_memory_guardrail(
    requested: &str,
    spec_limit: &str,
    scale_factor: f64,
) -> Result<(), String> {
    let req = parse_memory_to_bytes(requested)
        .ok_or_else(|| format!("cannot parse requested memory: {requested}"))?;
    let limit = parse_memory_to_bytes(spec_limit)
        .ok_or_else(|| format!("cannot parse spec memory limit: {spec_limit}"))?;
    let max_allowed = (limit as f64 * scale_factor) as u64;
    if req > max_allowed {
        Err(format!(
            "requested memory {requested} ({req} bytes) exceeds guardrail \
             ({spec_limit} * {scale_factor} = {max_allowed} bytes)"
        ))
    } else {
        Ok(())
    }
}

/// Reject if `requested_replicas` is less than `min_masters`.
pub fn validate_scale_guardrail(
    requested_replicas: u32,
    min_masters: u32,
) -> Result<(), String> {
    if requested_replicas < min_masters {
        Err(format!(
            "requested replicas {requested_replicas} is below minimum masters {min_masters}"
        ))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Server-side apply helper
// ---------------------------------------------------------------------------

async fn apply_server_side(
    client: &Client,
    namespace: &str,
    manifest: &Value,
) -> anyhow::Result<String> {
    let kind = manifest["kind"]
        .as_str()
        .context("manifest missing 'kind'")?;
    let name = manifest["metadata"]["name"]
        .as_str()
        .context("manifest missing 'metadata.name'")?;

    let pp = PatchParams::apply("valkey-ai-operator").force();
    let patch = Patch::Apply(manifest);

    match kind {
        "StatefulSet" => {
            let api: Api<StatefulSet> = Api::namespaced(client.clone(), namespace);
            api.patch(name, &pp, &patch).await?;
        }
        "Service" => {
            let api: Api<Service> = Api::namespaced(client.clone(), namespace);
            api.patch(name, &pp, &patch).await?;
        }
        "ConfigMap" => {
            let api: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
            api.patch(name, &pp, &patch).await?;
        }
        _ => anyhow::bail!("unsupported kind for server-side apply: {kind}"),
    }

    Ok(format!("{kind}/{name} applied"))
}

// ---------------------------------------------------------------------------
// 1. KubectlApply — generic server-side apply for any K8s resource
// ---------------------------------------------------------------------------

pub struct KubectlApply {
    client: Arc<Client>,
    namespace: String,
}

impl KubectlApply {
    pub fn new(client: Arc<Client>, namespace: String) -> Self {
        Self { client, namespace }
    }
}

#[async_trait::async_trait]
impl Tool for KubectlApply {
    fn name(&self) -> &str { "kubectl_apply" }

    fn description(&self) -> &str {
        "Apply a K8s resource manifest (JSON). Works like 'kubectl apply'. \
         Supports: StatefulSet, Service, ConfigMap. Uses server-side apply. \
         Agent composes the full manifest — you control every field. \
         Args: {\"manifest\": <JSON object with apiVersion, kind, metadata, spec>}"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "manifest": {
                    "type": "object",
                    "description": "Full K8s resource manifest with apiVersion, kind, metadata, spec"
                }
            },
            "required": ["manifest"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::Validated }

    async fn execute(&self, args: Value) -> ToolResult {
        let manifest = &args["manifest"];
        if manifest.is_null() {
            return ToolResult { success: false, output: "missing manifest".into() };
        }

        // Validate required fields
        let kind = match manifest["kind"].as_str() {
            Some(k) => k,
            None => return ToolResult { success: false, output: "manifest missing 'kind'".into() },
        };
        if manifest["metadata"]["name"].as_str().is_none() {
            return ToolResult { success: false, output: "manifest missing 'metadata.name'".into() };
        }

        // Use namespace from manifest or default
        let ns = manifest["metadata"]["namespace"]
            .as_str()
            .unwrap_or(&self.namespace);

        match apply_server_side(&self.client, ns, manifest).await {
            Ok(msg) => {
                info!("kubectl_apply: {msg}");
                ToolResult { success: true, output: msg }
            }
            Err(e) => ToolResult { success: false, output: format!("error: {e}") },
        }
    }
}

// ---------------------------------------------------------------------------
// 2. PatchResources
// ---------------------------------------------------------------------------

pub struct PatchResources {
    client: Arc<Client>,
    cluster_name: String,
    namespace: String,
    spec_memory_limit: Option<String>,
    max_memory_scale_factor: f64,
}

impl PatchResources {
    pub fn new(
        client: Arc<Client>,
        cluster_name: String,
        namespace: String,
        spec_memory_limit: Option<String>,
        max_memory_scale_factor: f64,
    ) -> Self {
        Self { client, cluster_name, namespace, spec_memory_limit, max_memory_scale_factor }
    }
}

#[async_trait::async_trait]
impl Tool for PatchResources {
    fn name(&self) -> &str { "patch_resources" }

    fn description(&self) -> &str {
        "Patch resource limits (memory, CPU) on the Valkey StatefulSet after validating memory guardrails."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "memory_limit": { "type": "string", "description": "Memory limit (e.g. '1Gi')" },
                "cpu_limit": { "type": "string", "description": "CPU limit (e.g. '1')" }
            },
            "required": ["memory_limit", "cpu_limit"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::Validated }

    async fn execute(&self, args: Value) -> ToolResult {
        let memory_limit = match args["memory_limit"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing memory_limit".into() },
        };
        let cpu_limit = match args["cpu_limit"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing cpu_limit".into() },
        };

        // Validate memory guardrail
        if let Some(ref spec_limit) = self.spec_memory_limit {
            if let Err(e) = validate_memory_guardrail(memory_limit, spec_limit, self.max_memory_scale_factor) {
                return ToolResult { success: false, output: format!("guardrail rejected: {e}") };
            }
        }

        // Use JSON Patch to surgically update resource limits without touching other fields
        let patch_ops: Vec<serde_json::Value> = vec![
            json!({"op": "replace", "path": "/spec/template/spec/containers/0/resources/limits/memory", "value": memory_limit}),
            json!({"op": "replace", "path": "/spec/template/spec/containers/0/resources/limits/cpu", "value": cpu_limit}),
            json!({"op": "replace", "path": "/spec/template/spec/containers/0/resources/requests/memory", "value": memory_limit}),
            json!({"op": "replace", "path": "/spec/template/spec/containers/0/resources/requests/cpu", "value": cpu_limit}),
        ];
        let patch = serde_json::Value::Array(patch_ops);

        let api: Api<StatefulSet> = Api::namespaced((*self.client).clone(), &self.namespace);
        let pp = PatchParams::default();
        match api.patch(&self.cluster_name, &pp, &Patch::Json::<()>(serde_json::from_value(patch).unwrap())).await {
            Ok(_) => {
                let msg = format!("patched resources: memory={memory_limit}, cpu={cpu_limit}");
                info!("PatchResources: {msg}");
                ToolResult { success: true, output: msg }
            }
            Err(e) => ToolResult { success: false, output: format!("error: {e}") },
        }
    }
}

// ---------------------------------------------------------------------------
// 3. GetPodStatus
// ---------------------------------------------------------------------------

pub struct GetPodStatus {
    client: Arc<Client>,
    namespace: String,
}

impl GetPodStatus {
    pub fn new(client: Arc<Client>, namespace: String) -> Self {
        Self { client, namespace }
    }
}

#[async_trait::async_trait]
impl Tool for GetPodStatus {
    fn name(&self) -> &str { "get_pod_status" }

    fn description(&self) -> &str {
        "Get the phase, conditions, and restart count of a specific pod."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pod_name": { "type": "string", "description": "Name of the pod" }
            },
            "required": ["pod_name"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, args: Value) -> ToolResult {
        let pod_name = match args["pod_name"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing pod_name".into() },
        };

        let api: Api<Pod> = Api::namespaced((*self.client).clone(), &self.namespace);
        match api.get(pod_name).await {
            Ok(pod) => {
                let status = pod.status.as_ref();
                let phase = status
                    .and_then(|s| s.phase.as_deref())
                    .unwrap_or("Unknown");

                let conditions: Vec<String> = status
                    .and_then(|s| s.conditions.as_ref())
                    .map(|conds| {
                        conds.iter().map(|c| format!("{}={}", c.type_, c.status)).collect()
                    })
                    .unwrap_or_default();

                let restart_count: i32 = status
                    .and_then(|s| s.container_statuses.as_ref())
                    .map(|cs| cs.iter().map(|c| c.restart_count).sum())
                    .unwrap_or(0);

                let output = json!({
                    "pod": pod_name,
                    "phase": phase,
                    "conditions": conditions,
                    "restart_count": restart_count,
                });
                ToolResult { success: true, output: output.to_string() }
            }
            Err(e) => ToolResult { success: false, output: format!("error: {e}") },
        }
    }
}

// ---------------------------------------------------------------------------
// 4. WaitForPods
// ---------------------------------------------------------------------------

pub struct WaitForPods {
    client: Arc<Client>,
    cluster_name: String,
    namespace: String,
}

impl WaitForPods {
    pub fn new(client: Arc<Client>, cluster_name: String, namespace: String) -> Self {
        Self { client, cluster_name, namespace }
    }
}

#[async_trait::async_trait]
impl Tool for WaitForPods {
    fn name(&self) -> &str { "wait_for_pods" }

    fn description(&self) -> &str {
        "Poll until pods matching a label selector are Ready, with a timeout."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "label_selector": { "type": "string", "description": "Label selector (e.g. 'app=valkey')" },
                "expected_count": { "type": "integer", "description": "Number of ready pods expected" },
                "timeout_secs": { "type": "integer", "description": "Timeout in seconds" }
            },
            "required": ["label_selector", "expected_count", "timeout_secs"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, args: Value) -> ToolResult {
        let default_label = format!("app={}", self.cluster_name);
        let label_selector = args["label_selector"].as_str()
            .unwrap_or(&default_label)
            .to_string();
        let expected_count = args["expected_count"].as_u64().unwrap_or(1) as usize;
        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(120);

        let api: Api<Pod> = Api::namespaced((*self.client).clone(), &self.namespace);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

        loop {
            let lp = ListParams::default().labels(&label_selector);
            match api.list(&lp).await {
                Ok(pod_list) => {
                    let ready_count = pod_list.items.iter().filter(|pod| {
                        pod.status.as_ref()
                            .and_then(|s| s.conditions.as_ref())
                            .map(|conds| conds.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
                            .unwrap_or(false)
                    }).count();

                    if ready_count >= expected_count {
                        return ToolResult {
                            success: true,
                            output: format!("{ready_count}/{expected_count} pods ready"),
                        };
                    }

                    if tokio::time::Instant::now() >= deadline {
                        return ToolResult {
                            success: false,
                            output: format!("timeout: only {ready_count}/{expected_count} pods ready after {timeout_secs}s"),
                        };
                    }
                }
                Err(e) => {
                    if tokio::time::Instant::now() >= deadline {
                        return ToolResult { success: false, output: format!("error: {e}") };
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }
}

// ---------------------------------------------------------------------------
// 5. GetEvents
// ---------------------------------------------------------------------------

pub struct GetEvents {
    client: Arc<Client>,
    namespace: String,
}

impl GetEvents {
    pub fn new(client: Arc<Client>, namespace: String) -> Self {
        Self { client, namespace }
    }
}

#[async_trait::async_trait]
impl Tool for GetEvents {
    fn name(&self) -> &str { "get_events" }

    fn description(&self) -> &str {
        "List Kubernetes events for a specific resource."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "resource_name": { "type": "string", "description": "Name of the resource to get events for" }
            },
            "required": ["resource_name"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, args: Value) -> ToolResult {
        let resource_name = match args["resource_name"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing resource_name".into() },
        };

        let api: Api<Event> = Api::namespaced((*self.client).clone(), &self.namespace);
        let field_selector = format!("involvedObject.name={resource_name}");
        let lp = ListParams::default().fields(&field_selector);

        match api.list(&lp).await {
            Ok(event_list) => {
                let events: Vec<Value> = event_list.items.iter().map(|e| {
                    json!({
                        "reason": e.reason,
                        "message": e.message,
                        "type": e.type_,
                        "count": e.count,
                        "last_timestamp": e.last_timestamp.as_ref().map(|t| t.0.to_rfc3339()),
                    })
                }).collect();
                ToolResult { success: true, output: serde_json::to_string(&events).unwrap_or_default() }
            }
            Err(e) => ToolResult { success: false, output: format!("error: {e}") },
        }
    }
}

// ---------------------------------------------------------------------------
// 6. GetPodLogs
// ---------------------------------------------------------------------------

pub struct GetPodLogs {
    client: Arc<Client>,
    namespace: String,
}

impl GetPodLogs {
    pub fn new(client: Arc<Client>, namespace: String) -> Self {
        Self { client, namespace }
    }
}

#[async_trait::async_trait]
impl Tool for GetPodLogs {
    fn name(&self) -> &str { "get_pod_logs" }

    fn description(&self) -> &str {
        "Get the last 100 lines of logs from a specific pod."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pod_name": { "type": "string", "description": "Name of the pod" }
            },
            "required": ["pod_name"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, args: Value) -> ToolResult {
        let pod_name = match args["pod_name"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing pod_name".into() },
        };

        let api: Api<Pod> = Api::namespaced((*self.client).clone(), &self.namespace);
        let lp = LogParams {
            tail_lines: Some(100),
            ..Default::default()
        };

        match api.logs(pod_name, &lp).await {
            Ok(logs) => ToolResult { success: true, output: logs },
            Err(e) => ToolResult { success: false, output: format!("error: {e}") },
        }
    }
}

// ---------------------------------------------------------------------------
// 7. RestartPod
// ---------------------------------------------------------------------------

pub struct RestartPod {
    client: Arc<Client>,
    namespace: String,
}

impl RestartPod {
    pub fn new(client: Arc<Client>, namespace: String) -> Self {
        Self { client, namespace }
    }
}

#[async_trait::async_trait]
impl Tool for RestartPod {
    fn name(&self) -> &str { "restart_pod" }

    fn description(&self) -> &str {
        "Delete a pod so that the StatefulSet controller recreates it."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pod_name": { "type": "string", "description": "Name of the pod to restart" }
            },
            "required": ["pod_name"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::Validated }

    async fn execute(&self, args: Value) -> ToolResult {
        let pod_name = match args["pod_name"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing pod_name".into() },
        };

        let api: Api<Pod> = Api::namespaced((*self.client).clone(), &self.namespace);
        match api.delete(pod_name, &Default::default()).await {
            Ok(_) => {
                let msg = format!("deleted pod {pod_name} for restart");
                info!("RestartPod: {msg}");
                ToolResult { success: true, output: msg }
            }
            Err(e) => ToolResult { success: false, output: format!("error: {e}") },
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// 8. UpdateClusterStatus
// ---------------------------------------------------------------------------
// Agent calls this to update the ValkeyCluster CRD status based on actual
// cluster health. This is the ONLY way to transition phases (e.g. to Running).

pub struct UpdateClusterStatus {
    client: Arc<Client>,
    cluster_name: String,
    namespace: String,
}

impl UpdateClusterStatus {
    pub fn new(client: Arc<Client>, cluster_name: String, namespace: String) -> Self {
        Self { client, cluster_name, namespace }
    }
}

#[async_trait::async_trait]
impl Tool for UpdateClusterStatus {
    fn name(&self) -> &str { "update_cluster_status" }

    fn description(&self) -> &str {
        "Update the ValkeyCluster CRD status based on actual cluster state. \
         Agent MUST call this after verifying cluster health. \
         Args: {\"phase\": \"Running\"|\"Healing\"|\"Failed\", \"cluster_state\": string, \"ready_nodes\": number, \"masters\": number}"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "phase": {
                    "type": "string",
                    "enum": ["Running", "Healing", "Failed", "Initializing"],
                    "description": "Cluster phase. Set to Running only after verifying cluster_state is ok."
                },
                "cluster_state": {
                    "type": "string",
                    "description": "Value from CLUSTER INFO cluster_state (e.g. 'ok' or 'fail')"
                },
                "ready_nodes": {
                    "type": "integer",
                    "description": "Number of healthy/responding nodes"
                },
                "masters": {
                    "type": "integer",
                    "description": "Number of master nodes"
                }
            },
            "required": ["phase", "cluster_state", "ready_nodes", "masters"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::Validated }

    async fn execute(&self, args: Value) -> ToolResult {
        let phase_str = args["phase"].as_str().unwrap_or("Initializing");
        let cluster_state = args["cluster_state"].as_str().unwrap_or("unknown");
        let ready_nodes = args["ready_nodes"].as_u64().unwrap_or(0) as u32;
        let masters = args["masters"].as_u64().unwrap_or(0) as u32;

        let api: Api<crate::crd::ValkeyCluster> = Api::namespaced((*self.client).clone(), &self.namespace);

        let status_patch = json!({
            "status": {
                "phase": phase_str,
                "cluster_state": cluster_state,
                "ready_nodes": ready_nodes,
                "masters": masters,
            }
        });

        let pp = PatchParams::default();
        match api.patch_status(&self.cluster_name, &pp, &Patch::Merge(&status_patch)).await {
            Ok(_) => {
                info!(
                    cluster = %self.cluster_name,
                    phase = phase_str,
                    cluster_state = cluster_state,
                    ready_nodes = ready_nodes,
                    masters = masters,
                    "Cluster status updated by agent"
                );
                ToolResult {
                    success: true,
                    output: format!("Status updated: phase={}, cluster_state={}, ready_nodes={}, masters={}",
                        phase_str, cluster_state, ready_nodes, masters),
                }
            }
            Err(e) => ToolResult {
                success: false,
                output: format!("Failed to update status: {}", e),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// 9. PodExec — run arbitrary command in a pod via kube attach API
// ---------------------------------------------------------------------------

pub struct PodExec {
    client: Arc<Client>,
    namespace: String,
}

impl PodExec {
    pub fn new(client: Arc<Client>, namespace: String) -> Self {
        Self { client, namespace }
    }
}

#[async_trait::async_trait]
impl Tool for PodExec {
    fn name(&self) -> &str { "pod_exec" }

    fn description(&self) -> &str {
        "Run a command inside a pod container. Use to get pod IP (hostname -i), check DNS, etc. \
         Args: {\"pod_name\": string, \"command\": [string]} e.g. [\"hostname\", \"-i\"]"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pod_name": { "type": "string", "description": "Pod name" },
                "command": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Command as array e.g. [\"hostname\", \"-i\"]"
                }
            },
            "required": ["pod_name", "command"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::Validated }

    async fn execute(&self, args: Value) -> ToolResult {
        let pod_name = match args["pod_name"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing pod_name".into() },
        };

        let command: Vec<String> = match args["command"].as_array() {
            Some(arr) => arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect(),
            None => return ToolResult { success: false, output: "missing command array".into() },
        };

        if command.is_empty() {
            return ToolResult { success: false, output: "empty command".into() };
        }

        let pods: Api<Pod> = Api::namespaced((*self.client).clone(), &self.namespace);
        let ap = kube::api::AttachParams {
            container: Some("valkey".to_string()),
            stdout: true,
            stderr: true,
            stdin: false,
            tty: false,
            ..Default::default()
        };

        match pods.exec(pod_name, command, &ap).await {
            Ok(mut attached) => {
                use tokio::io::AsyncReadExt;
                let mut stdout_str = String::new();
                if let Some(mut stdout) = attached.stdout() {
                    let _ = stdout.read_to_string(&mut stdout_str).await;
                }
                let mut stderr_str = String::new();
                if let Some(mut stderr) = attached.stderr() {
                    let _ = stderr.read_to_string(&mut stderr_str).await;
                }

                let _ = attached.take_status();

                let output = if !stdout_str.trim().is_empty() {
                    stdout_str.trim().to_string()
                } else {
                    stderr_str.trim().to_string()
                };

                ToolResult { success: true, output }
            }
            Err(e) => ToolResult { success: false, output: format!("error: {e}") },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_memory_gi() {
        assert_eq!(parse_memory_to_bytes("3Gi"), Some(3 * 1024 * 1024 * 1024));
        assert_eq!(parse_memory_to_bytes("512Mi"), Some(512 * 1024 * 1024));
        assert_eq!(parse_memory_to_bytes("1G"), Some(1_000_000_000));
        assert_eq!(parse_memory_to_bytes("1M"), Some(1_000_000));
        assert_eq!(parse_memory_to_bytes("1024"), Some(1024));
        assert_eq!(parse_memory_to_bytes("abc"), None);
    }

    #[test]
    fn test_validate_memory_limit_within_guardrail() {
        // 1Gi requested, spec limit 1Gi, scale factor 2.0 -> max 2Gi, should pass
        assert!(validate_memory_guardrail("1Gi", "1Gi", 2.0).is_ok());
        // 2Gi requested, spec limit 1Gi, scale factor 2.0 -> max 2Gi, should pass (equal)
        assert!(validate_memory_guardrail("2Gi", "1Gi", 2.0).is_ok());
    }

    #[test]
    fn test_validate_memory_limit_exceeds_guardrail() {
        // 3Gi requested, spec limit 1Gi, scale factor 2.0 -> max 2Gi, should fail
        let result = validate_memory_guardrail("3Gi", "1Gi", 2.0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("exceeds guardrail"));
    }

    #[test]
    fn test_validate_scale_rejects_below_masters() {
        assert!(validate_scale_guardrail(3, 3).is_ok());
        assert!(validate_scale_guardrail(6, 3).is_ok());
        let result = validate_scale_guardrail(2, 3);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("below minimum masters"));
    }
}
