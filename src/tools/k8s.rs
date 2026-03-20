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
// Server-side apply helper (pub — used by runtime::ApplyTemplate)
// ---------------------------------------------------------------------------

pub async fn apply_server_side(
    client: &Client,
    namespace: &str,
    manifest: &Value,
) -> anyhow::Result<String> {
    use anyhow::Context;

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
// 1. GetPodStatus
// ---------------------------------------------------------------------------

pub struct GetPodStatus {
    client: Client,
    namespace: String,
}

impl GetPodStatus {
    pub fn new(client: Client, namespace: &str) -> Self {
        Self { client, namespace: namespace.to_string() }
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

        let api: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
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
// 2. GetPodLogs
// ---------------------------------------------------------------------------

pub struct GetPodLogs {
    client: Client,
    namespace: String,
}

impl GetPodLogs {
    pub fn new(client: Client, namespace: &str) -> Self {
        Self { client, namespace: namespace.to_string() }
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

        let api: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
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
// 3. GetEvents
// ---------------------------------------------------------------------------

pub struct GetEvents {
    client: Client,
    namespace: String,
    resource_name: String,
}

impl GetEvents {
    pub fn new(client: Client, namespace: &str, resource_name: &str) -> Self {
        Self {
            client,
            namespace: namespace.to_string(),
            resource_name: resource_name.to_string(),
        }
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
                "resource_name": {
                    "type": "string",
                    "description": "Name of the resource to get events for (defaults to the current AIResource)"
                }
            }
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, args: Value) -> ToolResult {
        let resource_name = args["resource_name"]
            .as_str()
            .unwrap_or(&self.resource_name)
            .to_string();

        let api: Api<Event> = Api::namespaced(self.client.clone(), &self.namespace);
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
// 4. WaitForReady — poll until pods are ready, with configurable timeout
// ---------------------------------------------------------------------------

pub struct WaitForReady {
    client: Client,
    namespace: String,
    resource_name: String,
}

impl WaitForReady {
    pub fn new(client: Client, namespace: &str, resource_name: &str) -> Self {
        Self {
            client,
            namespace: namespace.to_string(),
            resource_name: resource_name.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for WaitForReady {
    fn name(&self) -> &str { "wait_for_ready" }

    fn description(&self) -> &str {
        "Poll until pods matching a label selector are Ready, with a timeout."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "expected_count": {
                    "type": "integer",
                    "description": "Number of ready pods expected"
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 120)"
                }
            },
            "required": ["expected_count"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, args: Value) -> ToolResult {
        let expected_count = args["expected_count"].as_u64().unwrap_or(1) as usize;
        let timeout_seconds = args["timeout_seconds"].as_u64().unwrap_or(120);

        let label_selector = format!("app={}", self.resource_name);
        let api: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_seconds);

        loop {
            let lp = ListParams::default().labels(&label_selector);
            match api.list(&lp).await {
                Ok(pod_list) => {
                    let actual = pod_list.items.iter().filter(|pod| {
                        pod.status.as_ref()
                            .and_then(|s| s.conditions.as_ref())
                            .map(|conds| conds.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
                            .unwrap_or(false)
                    }).count();

                    if actual >= expected_count {
                        info!("wait_for_ready: {}/{} pods ready", actual, expected_count);
                        let output = json!({
                            "ready": true,
                            "actual": actual,
                            "expected": expected_count,
                        });
                        return ToolResult { success: true, output: output.to_string() };
                    }

                    if tokio::time::Instant::now() >= deadline {
                        let output = json!({
                            "ready": false,
                            "actual": actual,
                            "expected": expected_count,
                        });
                        return ToolResult {
                            success: false,
                            output: output.to_string(),
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
// Tests
// ---------------------------------------------------------------------------

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
