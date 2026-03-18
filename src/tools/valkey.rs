use std::collections::HashMap;
use std::sync::Arc;

use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, AttachParams};
use kube::Client;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use crate::agent::tool::{Tool, ToolSafety};
use crate::agent::types::ToolResult;
use crate::crd::ValkeyClusterSpec;

// ---------------------------------------------------------------------------
// Denylist helpers
// ---------------------------------------------------------------------------

const DENIED_COMMANDS: &[&str] = &["FLUSHALL", "FLUSHDB", "DEBUG", "SHUTDOWN"];

pub fn is_command_denied(cmd: &str) -> bool {
    let upper = cmd.trim().to_uppercase();

    // CONFIG SET is allowed only for memory-related settings
    if upper.starts_with("CONFIG SET") {
        let rest = upper["CONFIG SET".len()..].trim_start().to_string();
        return !rest.starts_with("MAXMEMORY");
    }

    for denied in DENIED_COMMANDS {
        if upper.starts_with(denied) {
            return true;
        }
    }

    false
}

// ---------------------------------------------------------------------------
// parse_cluster_info
// ---------------------------------------------------------------------------

pub fn parse_cluster_info(raw: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in raw.lines() {
        let line = line.trim_end_matches('\r').trim();
        if let Some((k, v)) = line.split_once(':') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Pod name helper
// ---------------------------------------------------------------------------

fn pod_name(cluster_name: &str, pod_index: u32) -> String {
    format!("{cluster_name}-{pod_index}")
}

// ---------------------------------------------------------------------------
// Run valkey-cli inside pod via kube-rs exec API
// ---------------------------------------------------------------------------

async fn kube_exec_valkey_cli(
    client: &Client,
    namespace: &str,
    pod: &str,
    command: &str,
) -> anyhow::Result<String> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);

    // Build command: ["valkey-cli", ...args]
    let mut cmd_args: Vec<String> = vec!["valkey-cli".to_string()];
    for part in command.split_whitespace() {
        cmd_args.push(part.to_string());
    }

    let ap = AttachParams {
        container: Some("valkey".to_string()),
        stdout: true,
        stderr: true,
        stdin: false,
        tty: false,
        ..Default::default()
    };

    let mut attached = pods.exec(pod, cmd_args, &ap).await?;

    let mut stdout_str = String::new();
    if let Some(mut stdout) = attached.stdout() {
        stdout.read_to_string(&mut stdout_str).await?;
    }

    let mut stderr_str = String::new();
    if let Some(mut stderr) = attached.stderr() {
        stderr.read_to_string(&mut stderr_str).await?;
    }

    // Wait for process status
    let status = attached.take_status();
    if let Some(status) = status {
        if let Some(s) = status.await {
            if s.status.as_deref() != Some("Success") {
                if !stderr_str.is_empty() {
                    anyhow::bail!("valkey-cli failed: {}", stderr_str.trim());
                }
                let reason = s.reason.unwrap_or_default();
                anyhow::bail!("valkey-cli failed: {}", reason);
            }
        }
    }

    let output = stdout_str.trim().to_string();
    if output.is_empty() && !stderr_str.trim().is_empty() {
        return Ok(stderr_str.trim().to_string());
    }
    Ok(output)
}

// ---------------------------------------------------------------------------
// 1. ValkeyCli
// ---------------------------------------------------------------------------

pub struct ValkeyCli {
    client: Arc<Client>,
    cluster_name: String,
    namespace: String,
}

impl ValkeyCli {
    pub fn new(client: Arc<Client>, cluster_name: String, namespace: String) -> Self {
        Self { client, cluster_name, namespace }
    }
}

#[async_trait::async_trait]
impl Tool for ValkeyCli {
    fn name(&self) -> &str { "valkey_cli" }

    fn description(&self) -> &str {
        "Run a Valkey command on the specified pod via kubectl exec. Dangerous commands are blocked."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pod_index": { "type": "integer", "description": "Pod index (0-based)" },
                "command": { "type": "string", "description": "Valkey command to run" }
            },
            "required": ["pod_index", "command"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::Validated }

    async fn execute(&self, args: Value) -> ToolResult {
        let pod_index = match args["pod_index"].as_u64() {
            Some(v) => v as u32,
            None => return ToolResult { success: false, output: "missing pod_index".into() },
        };
        let command = match args["command"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing command".into() },
        };

        if is_command_denied(command) {
            return ToolResult {
                success: false,
                output: format!("command denied: {command}"),
            };
        }

        let pod = pod_name(&self.cluster_name, pod_index);
        match kube_exec_valkey_cli(&self.client, &self.namespace, &pod, command).await {
            Ok(output) => ToolResult { success: true, output },
            Err(e) => ToolResult { success: false, output: format!("error: {e}") },
        }
    }
}

// ---------------------------------------------------------------------------
// 2. ClusterNodes
// ---------------------------------------------------------------------------

pub struct ClusterNodes {
    client: Arc<Client>,
    cluster_name: String,
    namespace: String,
}

impl ClusterNodes {
    pub fn new(client: Arc<Client>, cluster_name: String, namespace: String) -> Self {
        Self { client, cluster_name, namespace }
    }
}

#[async_trait::async_trait]
impl Tool for ClusterNodes {
    fn name(&self) -> &str { "cluster_nodes" }

    fn description(&self) -> &str {
        "Run CLUSTER NODES on pod-0 and return the cluster topology."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, _args: Value) -> ToolResult {
        let pod = pod_name(&self.cluster_name, 0);
        match kube_exec_valkey_cli(&self.client, &self.namespace, &pod, "CLUSTER NODES").await {
            Ok(output) => ToolResult { success: true, output },
            Err(e) => ToolResult { success: false, output: format!("error: {e}") },
        }
    }
}

// ---------------------------------------------------------------------------
// 3. ClusterInfo
// ---------------------------------------------------------------------------

pub struct ClusterInfo {
    client: Arc<Client>,
    cluster_name: String,
    namespace: String,
}

impl ClusterInfo {
    pub fn new(client: Arc<Client>, cluster_name: String, namespace: String) -> Self {
        Self { client, cluster_name, namespace }
    }
}

#[async_trait::async_trait]
impl Tool for ClusterInfo {
    fn name(&self) -> &str { "cluster_info" }

    fn description(&self) -> &str {
        "Run CLUSTER INFO on pod-0 and return parsed cluster state."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, _args: Value) -> ToolResult {
        let pod = pod_name(&self.cluster_name, 0);
        match kube_exec_valkey_cli(&self.client, &self.namespace, &pod, "CLUSTER INFO").await {
            Ok(raw) => {
                let parsed = parse_cluster_info(&raw);
                match serde_json::to_string(&parsed) {
                    Ok(json_str) => ToolResult { success: true, output: json_str },
                    Err(e) => ToolResult { success: false, output: format!("json error: {e}") },
                }
            }
            Err(e) => ToolResult { success: false, output: format!("error: {e}") },
        }
    }
}

// ---------------------------------------------------------------------------
// 4. HealthCheck
// ---------------------------------------------------------------------------

pub struct HealthCheck {
    client: Arc<Client>,
    cluster_name: String,
    namespace: String,
    spec: Arc<ValkeyClusterSpec>,
}

impl HealthCheck {
    pub fn new(client: Arc<Client>, cluster_name: String, namespace: String, spec: Arc<ValkeyClusterSpec>) -> Self {
        Self { client, cluster_name, namespace, spec }
    }
}

#[async_trait::async_trait]
impl Tool for HealthCheck {
    fn name(&self) -> &str { "health_check" }

    fn description(&self) -> &str {
        "PING all Valkey nodes via kubectl exec and report latency and healthy node count."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, _args: Value) -> ToolResult {
        let total = self.spec.total_pods();
        let mut results = Vec::new();
        let mut healthy_count = 0u32;

        for pod_index in 0..total {
            let pod = pod_name(&self.cluster_name, pod_index);
            let start = std::time::Instant::now();
            match kube_exec_valkey_cli(&self.client, &self.namespace, &pod, "PING").await {
                Ok(response) => {
                    let latency_ms = start.elapsed().as_millis();
                    healthy_count += 1;
                    results.push(json!({
                        "pod_index": pod_index,
                        "pod": pod,
                        "status": "healthy",
                        "response": response,
                        "latency_ms": latency_ms,
                    }));
                }
                Err(e) => {
                    results.push(json!({
                        "pod_index": pod_index,
                        "pod": pod,
                        "status": "unhealthy",
                        "error": e.to_string(),
                    }));
                }
            }
        }

        let summary = json!({
            "healthy": healthy_count,
            "total": total,
            "nodes": results,
        });

        ToolResult {
            success: healthy_count == total,
            output: serde_json::to_string(&summary).unwrap_or_default(),
        }
    }
}

// ---------------------------------------------------------------------------
// 5. ClusterMeetAll — get all pod IPs and CLUSTER MEET them
// ---------------------------------------------------------------------------

pub struct ClusterMeetAll {
    client: Arc<Client>,
    cluster_name: String,
    namespace: String,
    spec: Arc<ValkeyClusterSpec>,
}

impl ClusterMeetAll {
    pub fn new(client: Arc<Client>, cluster_name: String, namespace: String, spec: Arc<ValkeyClusterSpec>) -> Self {
        Self { client, cluster_name, namespace, spec }
    }
}

async fn get_pod_ip(client: &Client, namespace: &str, pod: &str) -> anyhow::Result<String> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let ap = kube::api::AttachParams {
        container: Some("valkey".to_string()),
        stdout: true, stderr: true, stdin: false, tty: false,
        ..Default::default()
    };
    let mut attached = pods.exec(pod, vec!["hostname".to_string(), "-i".to_string()], &ap).await?;
    let mut stdout = String::new();
    if let Some(mut out) = attached.stdout() {
        out.read_to_string(&mut stdout).await?;
    }
    let _ = attached.take_status();
    let ip = stdout.trim().to_string();
    if ip.is_empty() {
        anyhow::bail!("empty IP for pod {}", pod);
    }
    Ok(ip)
}

/// Creates a NEW Valkey cluster from scratch. ONLY use when NO cluster exists yet (no slots assigned).
/// Do NOT use on a running cluster — use cluster_add_node + cluster_rebalance instead.
#[async_trait::async_trait]
impl Tool for ClusterMeetAll {
    fn name(&self) -> &str { "cluster_init" }
    fn description(&self) -> &str {
        "Create a NEW Valkey cluster from scratch using 'valkey-cli --cluster create'. \
         WARNING: ONLY use when cluster has NO slots assigned (new cluster). \
         Do NOT use on a running cluster with existing data — use cluster_add_node instead."
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }
    fn safety(&self) -> ToolSafety { ToolSafety::Validated }

    async fn execute(&self, _args: Value) -> ToolResult {
        let total = self.spec.total_pods();
        let masters = self.spec.masters;
        let replicas_per_master = self.spec.replicas_per_master;

        // Step 1: Get all pod IPs
        let mut endpoints: Vec<String> = Vec::new();
        for i in 0..total {
            let pod = pod_name(&self.cluster_name, i);
            match get_pod_ip(&self.client, &self.namespace, &pod).await {
                Ok(ip) => endpoints.push(format!("{}:6379", ip)),
                Err(e) => return ToolResult {
                    success: false,
                    output: format!("Failed to get IP for {}: {}", pod, e),
                },
            }
        }

        // Step 2: Build valkey-cli --cluster create command
        // valkey-cli --cluster create <ip1>:6379 <ip2>:6379 ... --cluster-replicas N --cluster-yes
        let mut cmd_args: Vec<String> = vec![
            "valkey-cli".to_string(),
            "--cluster".to_string(),
            "create".to_string(),
        ];
        cmd_args.extend(endpoints.clone());
        cmd_args.push("--cluster-replicas".to_string());
        cmd_args.push(replicas_per_master.to_string());
        cmd_args.push("--cluster-yes".to_string());

        // Run from pod-0
        let pod0 = pod_name(&self.cluster_name, 0);
        let pods: Api<Pod> = Api::namespaced(self.client.as_ref().clone(), &self.namespace);
        let ap = kube::api::AttachParams {
            container: Some("valkey".to_string()),
            stdout: true, stderr: true, stdin: false, tty: false,
            ..Default::default()
        };

        tracing::info!(
            cluster = %self.cluster_name,
            endpoints = ?endpoints,
            masters = masters,
            replicas_per_master = replicas_per_master,
            "Running valkey-cli --cluster create"
        );

        match pods.exec(&pod0, cmd_args, &ap).await {
            Ok(mut attached) => {
                let mut stdout_str = String::new();
                if let Some(mut stdout) = attached.stdout() {
                    stdout.read_to_string(&mut stdout_str).await.unwrap_or_default();
                }
                let mut stderr_str = String::new();
                if let Some(mut stderr) = attached.stderr() {
                    stderr.read_to_string(&mut stderr_str).await.unwrap_or_default();
                }
                let _ = attached.take_status();

                let output = if !stdout_str.trim().is_empty() {
                    stdout_str.trim().to_string()
                } else {
                    stderr_str.trim().to_string()
                };

                // Verify cluster state
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let verify = kube_exec_valkey_cli(&self.client, &self.namespace, &pod0, "CLUSTER INFO").await
                    .unwrap_or_default();
                let nodes = kube_exec_valkey_cli(&self.client, &self.namespace, &pod0, "CLUSTER NODES").await
                    .unwrap_or_default();

                let is_ok = verify.contains("cluster_state:ok");
                let summary = format!(
                    "cluster create output:\n{}\n\nCLUSTER INFO:\n{}\n\nCLUSTER NODES:\n{}",
                    output, verify, nodes
                );

                ToolResult { success: is_ok, output: summary }
            }
            Err(e) => ToolResult {
                success: false,
                output: format!("Failed to run cluster create: {}", e),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// 6. ClusterAddNode — add new nodes to an existing running cluster
// ---------------------------------------------------------------------------

pub struct ClusterAddNode {
    client: Arc<Client>,
    cluster_name: String,
    namespace: String,
}

impl ClusterAddNode {
    pub fn new(client: Arc<Client>, cluster_name: String, namespace: String) -> Self {
        Self { client, cluster_name, namespace }
    }
}

#[async_trait::async_trait]
impl Tool for ClusterAddNode {
    fn name(&self) -> &str { "cluster_add_node" }
    fn description(&self) -> &str {
        "Add a new pod to an EXISTING running cluster using 'valkey-cli --cluster add-node'. \
         Use this for scaling up. After adding nodes, call cluster_rebalance to distribute slots. \
         Args: {\"pod_index\": number} — the index of the new pod to add."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pod_index": { "type": "integer", "description": "Index of the new pod to add to the cluster" }
            },
            "required": ["pod_index"]
        })
    }
    fn safety(&self) -> ToolSafety { ToolSafety::Validated }

    async fn execute(&self, args: Value) -> ToolResult {
        let new_pod_index = match args["pod_index"].as_u64() {
            Some(v) => v as u32,
            None => return ToolResult { success: false, output: "missing pod_index".into() },
        };

        // Get IP of existing node (pod-0) and new node
        let pod0 = pod_name(&self.cluster_name, 0);
        let new_pod = pod_name(&self.cluster_name, new_pod_index);

        let existing_ip = match get_pod_ip(&self.client, &self.namespace, &pod0).await {
            Ok(ip) => ip,
            Err(e) => return ToolResult { success: false, output: format!("Failed to get pod-0 IP: {}", e) },
        };
        let new_ip = match get_pod_ip(&self.client, &self.namespace, &new_pod).await {
            Ok(ip) => ip,
            Err(e) => return ToolResult { success: false, output: format!("Failed to get {} IP: {}", new_pod, e) },
        };

        // valkey-cli --cluster add-node <new_host>:<port> <existing_host>:<port>
        let cmd_args = vec![
            "valkey-cli".to_string(),
            "--cluster".to_string(),
            "add-node".to_string(),
            format!("{}:6379", new_ip),
            format!("{}:6379", existing_ip),
        ];

        let pods: Api<Pod> = Api::namespaced(self.client.as_ref().clone(), &self.namespace);
        let ap = kube::api::AttachParams {
            container: Some("valkey".to_string()),
            stdout: true, stderr: true, stdin: false, tty: false,
            ..Default::default()
        };

        tracing::info!(cluster = %self.cluster_name, new_pod = %new_pod, "Adding node to cluster");

        match pods.exec(&pod0, cmd_args, &ap).await {
            Ok(mut attached) => {
                let mut stdout_str = String::new();
                if let Some(mut stdout) = attached.stdout() {
                    stdout.read_to_string(&mut stdout_str).await.unwrap_or_default();
                }
                let mut stderr_str = String::new();
                if let Some(mut stderr) = attached.stderr() {
                    stderr.read_to_string(&mut stderr_str).await.unwrap_or_default();
                }
                let _ = attached.take_status();

                let output = if !stdout_str.trim().is_empty() {
                    stdout_str.trim().to_string()
                } else {
                    stderr_str.trim().to_string()
                };

                let success = output.contains("[OK]") || output.contains("added");
                ToolResult { success, output }
            }
            Err(e) => ToolResult { success: false, output: format!("Failed: {}", e) },
        }
    }
}

// ---------------------------------------------------------------------------
// 7. ClusterRebalance — rebalance slots across all masters
// ---------------------------------------------------------------------------

pub struct ClusterRebalance {
    client: Arc<Client>,
    cluster_name: String,
    namespace: String,
}

impl ClusterRebalance {
    pub fn new(client: Arc<Client>, cluster_name: String, namespace: String) -> Self {
        Self { client, cluster_name, namespace }
    }
}

#[async_trait::async_trait]
impl Tool for ClusterRebalance {
    fn name(&self) -> &str { "cluster_rebalance" }
    fn description(&self) -> &str {
        "Rebalance hash slots evenly across all current master nodes using 'valkey-cli --cluster rebalance'. \
         Use after adding new masters to redistribute slots. No args needed."
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }
    fn safety(&self) -> ToolSafety { ToolSafety::Validated }

    async fn execute(&self, _args: Value) -> ToolResult {
        let pod0 = pod_name(&self.cluster_name, 0);

        // Get pod-0 IP for the rebalance command endpoint
        let ip = match get_pod_ip(&self.client, &self.namespace, &pod0).await {
            Ok(ip) => ip,
            Err(e) => return ToolResult { success: false, output: format!("Failed to get pod-0 IP: {}", e) },
        };

        // valkey-cli --cluster rebalance <host>:<port> --cluster-use-empty-masters
        let cmd_args = vec![
            "valkey-cli".to_string(),
            "--cluster".to_string(),
            "rebalance".to_string(),
            format!("{}:6379", ip),
            "--cluster-use-empty-masters".to_string(),
        ];

        let pods: Api<Pod> = Api::namespaced(self.client.as_ref().clone(), &self.namespace);
        let ap = kube::api::AttachParams {
            container: Some("valkey".to_string()),
            stdout: true, stderr: true, stdin: false, tty: false,
            ..Default::default()
        };

        tracing::info!(cluster = %self.cluster_name, "Running valkey-cli --cluster rebalance");

        match pods.exec(&pod0, cmd_args, &ap).await {
            Ok(mut attached) => {
                let mut stdout_str = String::new();
                if let Some(mut stdout) = attached.stdout() {
                    stdout.read_to_string(&mut stdout_str).await.unwrap_or_default();
                }
                let mut stderr_str = String::new();
                if let Some(mut stderr) = attached.stderr() {
                    stderr.read_to_string(&mut stderr_str).await.unwrap_or_default();
                }
                let _ = attached.take_status();

                let output = if !stdout_str.trim().is_empty() {
                    stdout_str.trim().to_string()
                } else {
                    stderr_str.trim().to_string()
                };

                // Verify after rebalance
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let info = kube_exec_valkey_cli(&self.client, &self.namespace, &pod0, "CLUSTER INFO").await
                    .unwrap_or_default();
                let nodes = kube_exec_valkey_cli(&self.client, &self.namespace, &pod0, "CLUSTER NODES").await
                    .unwrap_or_default();

                let summary = format!(
                    "Rebalance output:\n{}\n\nCLUSTER INFO:\n{}\n\nCLUSTER NODES:\n{}",
                    output, info, nodes
                );

                ToolResult { success: info.contains("cluster_state:ok"), output: summary }
            }
            Err(e) => ToolResult { success: false, output: format!("Failed: {}", e) },
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
    fn test_command_denylist() {
        assert!(is_command_denied("FLUSHALL"));
        assert!(is_command_denied("flushall"));
        assert!(!is_command_denied("CLUSTER RESET")); // allowed for cluster recovery
        assert!(is_command_denied("DEBUG SLEEP 10"));
        assert!(is_command_denied("SHUTDOWN"));
        assert!(is_command_denied("FLUSHDB"));
        assert!(is_command_denied("CONFIG SET bind-source-addr ''"));
        assert!(is_command_denied("CONFIG SET loglevel verbose"));
    }

    #[test]
    fn test_command_allowed() {
        assert!(!is_command_denied("PING"));
        assert!(!is_command_denied("CLUSTER INFO"));
        assert!(!is_command_denied("CLUSTER NODES"));
        assert!(!is_command_denied("CLUSTER MEET 127.0.0.1 6380"));
        assert!(!is_command_denied("CLUSTER ADDSLOTS 0 1 2"));
        assert!(!is_command_denied("CONFIG SET maxmemory 512mb"));
        assert!(!is_command_denied("CONFIG SET maxmemory-policy allkeys-lru"));
    }

    #[test]
    fn test_parse_cluster_info() {
        let raw = "cluster_enabled:1\r\ncluster_state:ok\r\ncluster_slots_assigned:16384\r\n";
        let map = parse_cluster_info(raw);
        assert_eq!(map.get("cluster_enabled"), Some(&"1".to_string()));
        assert_eq!(map.get("cluster_state"), Some(&"ok".to_string()));
        assert_eq!(map.get("cluster_slots_assigned"), Some(&"16384".to_string()));
    }
}
