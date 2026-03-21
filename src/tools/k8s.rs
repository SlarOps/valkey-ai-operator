use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::core::v1::{ConfigMap, Event, Pod, Service};
use kube::api::{Api, AttachParams, ListParams, LogParams, Patch, PatchParams};
use kube::Client;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tracing::info;

use crate::agent::tool::{Tool, ToolSafety};
use crate::agent::types::ToolResult;
use crate::crd::GuardrailSpec;

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
    owner_ref: Option<&Value>,
) -> anyhow::Result<String> {
    use anyhow::Context;

    let kind = manifest["kind"]
        .as_str()
        .context("manifest missing 'kind'")?;
    let name = manifest["metadata"]["name"]
        .as_str()
        .context("manifest missing 'metadata.name'")?;

    // Inject ownerReferences if provided
    let manifest = if let Some(oref) = owner_ref {
        let mut m = manifest.clone();
        m["metadata"]["ownerReferences"] = json!([oref]);
        m
    } else {
        manifest.clone()
    };

    let pp = PatchParams::apply("valkey-ai-operator").force();
    let patch = Patch::Apply(&manifest);

    match kind {
        "StatefulSet" => {
            let api: Api<StatefulSet> = Api::namespaced(client.clone(), namespace);
            let exists = api.get_opt(name).await?.is_some();
            if exists {
                // StatefulSet has many immutable fields (selector, serviceName, volumeClaimTemplates).
                // On update, use strategic merge patch with only mutable fields.
                let mut merge_patch = json!({
                    "spec": {}
                });
                let spec = manifest.get("spec");
                if let Some(replicas) = spec.and_then(|s| s.get("replicas")) {
                    merge_patch["spec"]["replicas"] = replicas.clone();
                }
                if let Some(template) = spec.and_then(|s| s.get("template")) {
                    merge_patch["spec"]["template"] = template.clone();
                }
                if let Some(update_strategy) = spec.and_then(|s| s.get("updateStrategy")) {
                    merge_patch["spec"]["updateStrategy"] = update_strategy.clone();
                }
                // Inject ownerReferences on update too
                if let Some(oref) = owner_ref {
                    merge_patch["metadata"] = json!({"ownerReferences": [oref]});
                }
                let pp = PatchParams::default();
                let patch = Patch::Strategic(&merge_patch);
                api.patch(name, &pp, &patch).await?;
            } else {
                api.patch(name, &pp, &patch).await?;
            }
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
        "Get logs from a pod. Supports previous container, time filtering, container selection, and tail lines."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pod_name": { "type": "string", "description": "Name of the pod" },
                "container": { "type": "string", "description": "Container name (optional, defaults to first container)" },
                "previous": { "type": "boolean", "description": "Get logs from previous terminated container (useful for crash analysis)" },
                "since_seconds": { "type": "integer", "description": "Only return logs newer than this many seconds" },
                "tail_lines": { "type": "integer", "description": "Number of lines from the end (default: 100)" }
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

        let tail_lines = args["tail_lines"].as_i64().unwrap_or(100);
        let previous = args["previous"].as_bool().unwrap_or(false);
        let since_seconds = args["since_seconds"].as_i64();
        let container = args["container"].as_str().map(String::from);

        let api: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let lp = LogParams {
            tail_lines: Some(tail_lines),
            previous,
            since_seconds,
            container,
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
                },
                "label_selector": {
                    "type": "string",
                    "description": "Custom label selector (default: app.kubernetes.io/instance=<resource_name>)"
                }
            },
            "required": ["expected_count"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, args: Value) -> ToolResult {
        let expected_count = args["expected_count"].as_u64().unwrap_or(1) as usize;
        let timeout_seconds = args["timeout_seconds"].as_u64().unwrap_or(120);

        let label_selector = args["label_selector"].as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("app.kubernetes.io/instance={}", self.resource_name));
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

// ===========================================================================
// READ-ONLY TOOLS
// ===========================================================================

// ---------------------------------------------------------------------------
// 5. KubectlDescribe — describe any resource by kind/name
// ---------------------------------------------------------------------------

pub struct KubectlDescribe {
    client: Client,
    namespace: String,
}

impl KubectlDescribe {
    pub fn new(client: Client, namespace: &str) -> Self {
        Self { client, namespace: namespace.to_string() }
    }
}

#[async_trait::async_trait]
impl Tool for KubectlDescribe {
    fn name(&self) -> &str { "kubectl_describe" }

    fn description(&self) -> &str {
        "Describe a Kubernetes resource in detail (like kubectl describe). Returns spec, status, conditions, events."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "description": "Resource kind: Pod, Service, StatefulSet, ConfigMap, Deployment, Event"
                },
                "name": {
                    "type": "string",
                    "description": "Name of the resource"
                }
            },
            "required": ["kind", "name"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, args: Value) -> ToolResult {
        let kind = match args["kind"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing kind".into() },
        };
        let name = match args["name"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing name".into() },
        };

        let result = match kind {
            "Pod" | "pod" => {
                let api: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
                api.get(name).await.map(|r| serde_json::to_value(r).unwrap_or_default())
            }
            "Service" | "service" | "svc" => {
                let api: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
                api.get(name).await.map(|r| serde_json::to_value(r).unwrap_or_default())
            }
            "StatefulSet" | "statefulset" | "sts" => {
                let api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
                api.get(name).await.map(|r| serde_json::to_value(r).unwrap_or_default())
            }
            "ConfigMap" | "configmap" | "cm" => {
                let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
                api.get(name).await.map(|r| serde_json::to_value(r).unwrap_or_default())
            }
            "Deployment" | "deployment" | "deploy" => {
                let api: Api<Deployment> = Api::namespaced(self.client.clone(), &self.namespace);
                api.get(name).await.map(|r| serde_json::to_value(r).unwrap_or_default())
            }
            _ => return ToolResult {
                success: false,
                output: format!("unsupported kind '{}'. Supported: Pod, Service, StatefulSet, ConfigMap, Deployment", kind),
            },
        };

        match result {
            Ok(v) => ToolResult { success: true, output: serde_json::to_string_pretty(&v).unwrap_or_default() },
            Err(e) => ToolResult { success: false, output: format!("error: {e}") },
        }
    }
}

// ---------------------------------------------------------------------------
// 6. KubectlGet — query resources with optional jsonpath
// ---------------------------------------------------------------------------

pub struct KubectlGet {
    client: Client,
    namespace: String,
}

impl KubectlGet {
    pub fn new(client: Client, namespace: &str) -> Self {
        Self { client, namespace: namespace.to_string() }
    }
}

/// Extract a value from a JSON object using a dot-separated path (e.g. "spec.replicas")
fn jsonpath_extract(value: &Value, path: &str) -> Value {
    let mut current = value;
    for segment in path.split('.') {
        // Try array index
        if let Ok(idx) = segment.parse::<usize>() {
            current = match current.get(idx) {
                Some(v) => v,
                None => return Value::Null,
            };
        } else {
            current = match current.get(segment) {
                Some(v) => v,
                None => return Value::Null,
            };
        }
    }
    current.clone()
}

#[async_trait::async_trait]
impl Tool for KubectlGet {
    fn name(&self) -> &str { "kubectl_get" }

    fn description(&self) -> &str {
        "Get a Kubernetes resource or list resources of a kind. Supports jsonpath to extract specific fields."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "description": "Resource kind: Pod, Service, StatefulSet, ConfigMap, Deployment"
                },
                "name": {
                    "type": "string",
                    "description": "Name of the resource (omit to list all of this kind)"
                },
                "jsonpath": {
                    "type": "string",
                    "description": "Dot-separated path to extract specific fields (e.g. 'spec.replicas', 'status.phase')"
                },
                "label_selector": {
                    "type": "string",
                    "description": "Label selector to filter resources (e.g. 'app=my-app')"
                }
            },
            "required": ["kind"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, args: Value) -> ToolResult {
        let kind = match args["kind"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing kind".into() },
        };
        let name = args["name"].as_str();
        let jsonpath = args["jsonpath"].as_str();
        let label_selector = args["label_selector"].as_str();

        macro_rules! get_or_list {
            ($api:expr) => {
                if let Some(name) = name {
                    $api.get(name).await
                        .map(|r| serde_json::to_value(r).unwrap_or_default())
                        .map(|v| if let Some(jp) = jsonpath { jsonpath_extract(&v, jp) } else { v })
                } else {
                    let lp = if let Some(ls) = label_selector {
                        ListParams::default().labels(ls)
                    } else {
                        ListParams::default()
                    };
                    $api.list(&lp).await
                        .map(|list| {
                            let items: Vec<Value> = list.items.into_iter()
                                .map(|item| {
                                    let v = serde_json::to_value(item).unwrap_or_default();
                                    if let Some(jp) = jsonpath { jsonpath_extract(&v, jp) } else { v }
                                })
                                .collect();
                            json!(items)
                        })
                }
            };
        }

        let result = match kind {
            "Pod" | "pod" => {
                let api: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
                get_or_list!(api)
            }
            "Service" | "service" | "svc" => {
                let api: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
                get_or_list!(api)
            }
            "StatefulSet" | "statefulset" | "sts" => {
                let api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
                get_or_list!(api)
            }
            "ConfigMap" | "configmap" | "cm" => {
                let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
                get_or_list!(api)
            }
            "Deployment" | "deployment" | "deploy" => {
                let api: Api<Deployment> = Api::namespaced(self.client.clone(), &self.namespace);
                get_or_list!(api)
            }
            _ => return ToolResult {
                success: false,
                output: format!("unsupported kind '{}'. Supported: Pod, Service, StatefulSet, ConfigMap, Deployment", kind),
            },
        };

        match result {
            Ok(v) => ToolResult { success: true, output: serde_json::to_string_pretty(&v).unwrap_or_default() },
            Err(e) => ToolResult { success: false, output: format!("error: {e}") },
        }
    }
}

// ===========================================================================
// MUTATION TOOLS
// ===========================================================================

// ---------------------------------------------------------------------------
// 7. KubectlScale — scale deployment/statefulset replicas
// ---------------------------------------------------------------------------

pub struct KubectlScale {
    client: Client,
    namespace: String,
    guardrails: Option<GuardrailSpec>,
}

impl KubectlScale {
    pub fn new(client: Client, namespace: &str, guardrails: Option<GuardrailSpec>) -> Self {
        Self { client, namespace: namespace.to_string(), guardrails }
    }
}

#[async_trait::async_trait]
impl Tool for KubectlScale {
    fn name(&self) -> &str { "kubectl_scale" }

    fn description(&self) -> &str {
        "Scale a StatefulSet or Deployment to the specified number of replicas."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "description": "Resource kind: StatefulSet or Deployment"
                },
                "name": {
                    "type": "string",
                    "description": "Name of the resource to scale"
                },
                "replicas": {
                    "type": "integer",
                    "description": "Desired number of replicas"
                }
            },
            "required": ["kind", "name", "replicas"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::Validated }

    async fn execute(&self, args: Value) -> ToolResult {
        let kind = match args["kind"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing kind".into() },
        };
        let name = match args["name"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing name".into() },
        };
        let replicas = match args["replicas"].as_u64() {
            Some(v) => v as u32,
            None => return ToolResult { success: false, output: "missing or invalid replicas".into() },
        };

        // Guardrail: check max_replicas
        if let Some(ref g) = self.guardrails {
            if let Some(max) = g.max_replicas {
                if replicas > max {
                    return ToolResult {
                        success: false,
                        output: format!("guardrail: requested {} replicas exceeds max_replicas {}", replicas, max),
                    };
                }
            }
        }

        let patch = json!({ "spec": { "replicas": replicas } });
        let pp = PatchParams::default();

        let result = match kind {
            "StatefulSet" | "statefulset" | "sts" => {
                let api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
                api.patch(name, &pp, &Patch::Merge(&patch)).await.map(|_| ())
            }
            "Deployment" | "deployment" | "deploy" => {
                let api: Api<Deployment> = Api::namespaced(self.client.clone(), &self.namespace);
                api.patch(name, &pp, &Patch::Merge(&patch)).await.map(|_| ())
            }
            _ => return ToolResult {
                success: false,
                output: format!("unsupported kind '{}' for scale. Use StatefulSet or Deployment", kind),
            },
        };

        match result {
            Ok(()) => {
                info!("kubectl_scale: {}/{} scaled to {} replicas", kind, name, replicas);
                ToolResult { success: true, output: format!("{}/{} scaled to {} replicas", kind, name, replicas) }
            }
            Err(e) => ToolResult { success: false, output: format!("error: {e}") },
        }
    }
}

// ---------------------------------------------------------------------------
// 8. KubectlPatch — patch resource fields
// ---------------------------------------------------------------------------

pub struct KubectlPatch {
    client: Client,
    namespace: String,
    guardrails: Option<GuardrailSpec>,
}

impl KubectlPatch {
    pub fn new(client: Client, namespace: &str, guardrails: Option<GuardrailSpec>) -> Self {
        Self { client, namespace: namespace.to_string(), guardrails }
    }
}

/// Fields that agents must never patch
const BLOCKED_PATCH_PATHS: &[&str] = &[
    "metadata.ownerReferences",
    "metadata.finalizers",
    "metadata.namespace",
    "metadata.uid",
    "metadata.deletionTimestamp",
];

fn validate_patch_fields(patch: &Value) -> Result<(), String> {
    for blocked in BLOCKED_PATCH_PATHS {
        let parts: Vec<&str> = blocked.split('.').collect();
        let mut current = patch;
        let mut found = true;
        for part in &parts {
            match current.get(part) {
                Some(v) => current = v,
                None => { found = false; break; }
            }
        }
        if found {
            return Err(format!("patching '{}' is not allowed", blocked));
        }
    }
    Ok(())
}

#[async_trait::async_trait]
impl Tool for KubectlPatch {
    fn name(&self) -> &str { "kubectl_patch" }

    fn description(&self) -> &str {
        "Patch a Kubernetes resource using JSON merge patch. Cannot modify ownerReferences, finalizers, or namespace."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "description": "Resource kind: Pod, Service, StatefulSet, ConfigMap, Deployment"
                },
                "name": {
                    "type": "string",
                    "description": "Name of the resource to patch"
                },
                "patch": {
                    "type": "object",
                    "description": "JSON merge patch object"
                }
            },
            "required": ["kind", "name", "patch"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::Validated }

    async fn execute(&self, args: Value) -> ToolResult {
        let kind = match args["kind"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing kind".into() },
        };
        let name = match args["name"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing name".into() },
        };
        let patch_value = match args.get("patch") {
            Some(v) if v.is_object() => v,
            _ => return ToolResult { success: false, output: "missing or invalid patch object".into() },
        };

        // Validate blocked fields
        if let Err(e) = validate_patch_fields(patch_value) {
            return ToolResult { success: false, output: format!("guardrail: {}", e) };
        }

        // Guardrail: check replicas in patch
        if let Some(ref g) = self.guardrails {
            if let Some(max) = g.max_replicas {
                if let Some(replicas) = patch_value.pointer("/spec/replicas").and_then(|v| v.as_u64()) {
                    if replicas as u32 > max {
                        return ToolResult {
                            success: false,
                            output: format!("guardrail: patch replicas {} exceeds max_replicas {}", replicas, max),
                        };
                    }
                }
            }
        }

        let pp = PatchParams::default();
        let patch = Patch::Merge(patch_value);

        let result = match kind {
            "Pod" | "pod" => {
                let api: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
                api.patch(name, &pp, &patch).await.map(|_| ())
            }
            "Service" | "service" | "svc" => {
                let api: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
                api.patch(name, &pp, &patch).await.map(|_| ())
            }
            "StatefulSet" | "statefulset" | "sts" => {
                let api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
                api.patch(name, &pp, &patch).await.map(|_| ())
            }
            "ConfigMap" | "configmap" | "cm" => {
                let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
                api.patch(name, &pp, &patch).await.map(|_| ())
            }
            "Deployment" | "deployment" | "deploy" => {
                let api: Api<Deployment> = Api::namespaced(self.client.clone(), &self.namespace);
                api.patch(name, &pp, &patch).await.map(|_| ())
            }
            _ => return ToolResult {
                success: false,
                output: format!("unsupported kind '{}'. Supported: Pod, Service, StatefulSet, ConfigMap, Deployment", kind),
            },
        };

        match result {
            Ok(()) => {
                info!("kubectl_patch: {}/{} patched", kind, name);
                ToolResult { success: true, output: format!("{}/{} patched successfully", kind, name) }
            }
            Err(e) => ToolResult { success: false, output: format!("error: {e}") },
        }
    }
}

// ---------------------------------------------------------------------------
// 9. KubectlExec — run diagnostic commands in pods
// ---------------------------------------------------------------------------

pub struct KubectlExec {
    client: Client,
    namespace: String,
    denied_commands: Vec<String>,
}

/// Commands that are always blocked regardless of guardrails
const ALWAYS_BLOCKED_COMMANDS: &[&str] = &[
    "rm -rf /",
    "dd if=",
    "mkfs",
    "shutdown",
    "reboot",
    "kubectl delete",
    "kill -9 1",
];

impl KubectlExec {
    pub fn new(client: Client, namespace: &str, denied_commands: Vec<String>) -> Self {
        Self { client, namespace: namespace.to_string(), denied_commands }
    }
}

#[async_trait::async_trait]
impl Tool for KubectlExec {
    fn name(&self) -> &str { "kubectl_exec" }

    fn description(&self) -> &str {
        "Run a command in a running pod for diagnostics. Cannot run destructive commands."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pod_name": {
                    "type": "string",
                    "description": "Name of the pod"
                },
                "command": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Command to run as array (e.g. ['pg_isready', '-h', 'localhost'])"
                },
                "container": {
                    "type": "string",
                    "description": "Container name (optional, defaults to first container)"
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 30)"
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
            Some(arr) => arr.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            None => return ToolResult { success: false, output: "missing or invalid command array".into() },
        };
        if command.is_empty() {
            return ToolResult { success: false, output: "command array is empty".into() };
        }
        let container = args["container"].as_str().map(String::from);
        let timeout_secs = args["timeout_seconds"].as_u64().unwrap_or(30);

        // Validate against blocklists
        let cmd_str = command.join(" ");
        for blocked in ALWAYS_BLOCKED_COMMANDS {
            if cmd_str.contains(blocked) {
                return ToolResult {
                    success: false,
                    output: format!("blocked: command contains '{}' which is never allowed", blocked),
                };
            }
        }
        for denied in &self.denied_commands {
            if cmd_str.contains(denied.as_str()) {
                return ToolResult {
                    success: false,
                    output: format!("guardrail: command contains denied command '{}'", denied),
                };
            }
        }

        let api: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let ap = AttachParams {
            container,
            stdout: true,
            stderr: true,
            stdin: false,
            tty: false,
            ..Default::default()
        };

        let exec_future = async {
            match api.exec(pod_name, command, &ap).await {
                Ok(mut attached) => {
                    let mut stdout_str = String::new();
                    if let Some(mut stdout) = attached.stdout() {
                        let _ = stdout.read_to_string(&mut stdout_str).await;
                    }
                    let mut stderr_str = String::new();
                    if let Some(mut stderr) = attached.stderr() {
                        let _ = stderr.read_to_string(&mut stderr_str).await;
                    }

                    let exit_code: i32 = if let Some(status_future) = attached.take_status() {
                        match status_future.await {
                            Some(status) => status.code.unwrap_or(0),
                            None => 0,
                        }
                    } else {
                        0
                    };

                    info!("kubectl_exec on '{}': exit_code={}", pod_name, exit_code);

                    let output = json!({
                        "exit_code": exit_code,
                        "stdout": stdout_str.trim(),
                        "stderr": stderr_str.trim(),
                    });
                    ToolResult { success: exit_code == 0, output: output.to_string() }
                }
                Err(e) => ToolResult { success: false, output: format!("error: {}", e) },
            }
        };

        match tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            exec_future,
        ).await {
            Ok(result) => result,
            Err(_) => ToolResult {
                success: false,
                output: format!("timeout: command did not complete within {}s", timeout_secs),
            },
        }
    }
}

// ===========================================================================
// BLOCKED TOOLS — NOT IMPLEMENTED BY DESIGN
// ===========================================================================
//
// The following tools are intentionally NOT provided to the agent:
// - kubectl_delete: Resource lifecycle is managed by the controller via ownerReferences
// - kubectl_drain: Node management is out of scope for workload agents
// - kubectl_cordon: Node management is out of scope for workload agents
//
// Deletion is handled by K8s garbage collection when the AIResource is deleted.

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

    #[test]
    fn test_jsonpath_extract() {
        let value = json!({
            "spec": {
                "replicas": 3,
                "template": {
                    "spec": {
                        "containers": [
                            {"name": "app", "image": "nginx"}
                        ]
                    }
                }
            },
            "status": {
                "phase": "Running"
            }
        });

        assert_eq!(jsonpath_extract(&value, "spec.replicas"), json!(3));
        assert_eq!(jsonpath_extract(&value, "status.phase"), json!("Running"));
        assert_eq!(jsonpath_extract(&value, "spec.template.spec.containers.0.name"), json!("app"));
        assert_eq!(jsonpath_extract(&value, "nonexistent.path"), Value::Null);
    }

    #[test]
    fn test_validate_patch_fields_blocks_owner_references() {
        let patch = json!({
            "metadata": {
                "ownerReferences": [{"name": "hacked"}]
            }
        });
        assert!(validate_patch_fields(&patch).is_err());
    }

    #[test]
    fn test_validate_patch_fields_blocks_finalizers() {
        let patch = json!({
            "metadata": {
                "finalizers": ["block-deletion"]
            }
        });
        assert!(validate_patch_fields(&patch).is_err());
    }

    #[test]
    fn test_validate_patch_fields_allows_spec() {
        let patch = json!({
            "spec": {
                "replicas": 5
            }
        });
        assert!(validate_patch_fields(&patch).is_ok());
    }

    #[test]
    fn test_validate_patch_fields_allows_labels() {
        let patch = json!({
            "metadata": {
                "labels": {"env": "prod"}
            }
        });
        assert!(validate_patch_fields(&patch).is_ok());
    }

    #[test]
    fn test_blocked_commands() {
        let cmd = "rm -rf / --no-preserve-root";
        assert!(ALWAYS_BLOCKED_COMMANDS.iter().any(|b| cmd.contains(b)));

        let safe_cmd = "pg_isready -h localhost";
        assert!(!ALWAYS_BLOCKED_COMMANDS.iter().any(|b| safe_cmd.contains(b)));
    }
}
