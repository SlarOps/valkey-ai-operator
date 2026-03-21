use serde_json::{json, Value};
use tracing::info;

use crate::agent::tool::{Tool, ToolSafety};
use crate::agent::types::ToolResult;

// ---------------------------------------------------------------------------
// HelmInstall
// ---------------------------------------------------------------------------

pub struct HelmInstall {
    namespace: String,
}

impl HelmInstall {
    pub fn new(namespace: &str) -> Self {
        Self { namespace: namespace.to_string() }
    }
}

#[async_trait::async_trait]
impl Tool for HelmInstall {
    fn name(&self) -> &str { "helm_install" }

    fn description(&self) -> &str {
        "Install a Helm chart release."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "release_name": {
                    "type": "string",
                    "description": "Helm release name"
                },
                "chart": {
                    "type": "string",
                    "description": "Chart reference (e.g. oci://registry-1.docker.io/bitnamicharts/valkey-cluster)"
                },
                "values": {
                    "type": "object",
                    "description": "Helm values as key=value pairs (supports nested keys like cluster.nodes=6)"
                },
                "wait": {
                    "type": "boolean",
                    "description": "Wait for all resources to be ready (default: true)"
                },
                "timeout": {
                    "type": "string",
                    "description": "Timeout for --wait (default: 600s)"
                }
            },
            "required": ["release_name", "chart"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::Validated }

    async fn execute(&self, args: Value) -> ToolResult {
        let release = match args["release_name"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing release_name".into() },
        };
        let chart = match args["chart"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing chart".into() },
        };

        let mut cmd = tokio::process::Command::new("helm");
        cmd.arg("install").arg(release).arg(chart);
        cmd.arg("--namespace").arg(&self.namespace);
        cmd.arg("--create-namespace");

        apply_values(&mut cmd, &args);

        let wait = args["wait"].as_bool().unwrap_or(true);
        let timeout = args["timeout"].as_str().unwrap_or("600s");
        if wait {
            cmd.arg("--wait").arg("--timeout").arg(timeout);
        }

        info!("helm install {} {} -n {}", release, chart, self.namespace);
        run_helm_command(cmd).await
    }
}

// ---------------------------------------------------------------------------
// HelmUpgrade
// ---------------------------------------------------------------------------

pub struct HelmUpgrade {
    namespace: String,
}

impl HelmUpgrade {
    pub fn new(namespace: &str) -> Self {
        Self { namespace: namespace.to_string() }
    }
}

#[async_trait::async_trait]
impl Tool for HelmUpgrade {
    fn name(&self) -> &str { "helm_upgrade" }

    fn description(&self) -> &str {
        "Upgrade an existing Helm chart release with new values."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "release_name": {
                    "type": "string",
                    "description": "Helm release name"
                },
                "chart": {
                    "type": "string",
                    "description": "Chart reference (e.g. oci://registry-1.docker.io/bitnamicharts/valkey-cluster)"
                },
                "values": {
                    "type": "object",
                    "description": "Helm values to set/override (supports nested keys like cluster.nodes=6)"
                },
                "reuse_values": {
                    "type": "boolean",
                    "description": "Reuse existing values and merge with new ones (default: true)"
                },
                "wait": {
                    "type": "boolean",
                    "description": "Wait for all resources to be ready (default: true)"
                },
                "timeout": {
                    "type": "string",
                    "description": "Timeout for --wait (default: 600s)"
                }
            },
            "required": ["release_name", "chart"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::Validated }

    async fn execute(&self, args: Value) -> ToolResult {
        let release = match args["release_name"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing release_name".into() },
        };
        let chart = match args["chart"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing chart".into() },
        };

        let mut cmd = tokio::process::Command::new("helm");
        cmd.arg("upgrade").arg(release).arg(chart);
        cmd.arg("--namespace").arg(&self.namespace);

        let reuse = args["reuse_values"].as_bool().unwrap_or(true);
        if reuse {
            cmd.arg("--reuse-values");
        }

        apply_values(&mut cmd, &args);

        let wait = args["wait"].as_bool().unwrap_or(true);
        let timeout = args["timeout"].as_str().unwrap_or("600s");
        if wait {
            cmd.arg("--wait").arg("--timeout").arg(timeout);
        }

        info!("helm upgrade {} {} -n {}", release, chart, self.namespace);
        run_helm_command(cmd).await
    }
}

// ---------------------------------------------------------------------------
// HelmStatus
// ---------------------------------------------------------------------------

pub struct HelmStatus {
    namespace: String,
}

impl HelmStatus {
    pub fn new(namespace: &str) -> Self {
        Self { namespace: namespace.to_string() }
    }
}

#[async_trait::async_trait]
impl Tool for HelmStatus {
    fn name(&self) -> &str { "helm_status" }

    fn description(&self) -> &str {
        "Get the status of a Helm release."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "release_name": {
                    "type": "string",
                    "description": "Helm release name"
                }
            },
            "required": ["release_name"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, args: Value) -> ToolResult {
        let release = match args["release_name"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing release_name".into() },
        };

        let mut cmd = tokio::process::Command::new("helm");
        cmd.arg("status").arg(release);
        cmd.arg("--namespace").arg(&self.namespace);
        cmd.arg("-o").arg("json");

        run_helm_command(cmd).await
    }
}

// ---------------------------------------------------------------------------
// HelmGetValues
// ---------------------------------------------------------------------------

pub struct HelmGetValues {
    namespace: String,
}

impl HelmGetValues {
    pub fn new(namespace: &str) -> Self {
        Self { namespace: namespace.to_string() }
    }
}

#[async_trait::async_trait]
impl Tool for HelmGetValues {
    fn name(&self) -> &str { "helm_get_values" }

    fn description(&self) -> &str {
        "Get the current values of a Helm release."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "release_name": {
                    "type": "string",
                    "description": "Helm release name"
                },
                "all": {
                    "type": "boolean",
                    "description": "Show all values including defaults (default: false)"
                }
            },
            "required": ["release_name"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, args: Value) -> ToolResult {
        let release = match args["release_name"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing release_name".into() },
        };

        let mut cmd = tokio::process::Command::new("helm");
        cmd.arg("get").arg("values").arg(release);
        cmd.arg("--namespace").arg(&self.namespace);
        cmd.arg("-o").arg("json");

        if args["all"].as_bool().unwrap_or(false) {
            cmd.arg("--all");
        }

        run_helm_command(cmd).await
    }
}

// ---------------------------------------------------------------------------
// HelmShowValues — show default values of a chart (not a release)
// ---------------------------------------------------------------------------

pub struct HelmShowValues;

impl HelmShowValues {
    pub fn new() -> Self { Self }
}

#[async_trait::async_trait]
impl Tool for HelmShowValues {
    fn name(&self) -> &str { "helm_show_values" }

    fn description(&self) -> &str {
        "Show the default values of a Helm chart (before installing)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "chart": {
                    "type": "string",
                    "description": "Chart reference (e.g. oci://registry-1.docker.io/bitnamicharts/valkey-cluster)"
                }
            },
            "required": ["chart"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, args: Value) -> ToolResult {
        let chart = match args["chart"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing chart".into() },
        };

        let mut cmd = tokio::process::Command::new("helm");
        cmd.arg("show").arg("values").arg(chart);

        run_helm_command(cmd).await
    }
}

// ---------------------------------------------------------------------------
// Helper — flatten nested JSON objects into dot-notation for --set
// ---------------------------------------------------------------------------

fn flatten_values(prefix: &str, value: &Value, out: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{}.{}", prefix, k)
                };
                flatten_values(&key, v, out);
            }
        }
        Value::String(s) => out.push((prefix.to_string(), s.clone())),
        Value::Number(n) => out.push((prefix.to_string(), n.to_string())),
        Value::Bool(b) => out.push((prefix.to_string(), b.to_string())),
        _ => out.push((prefix.to_string(), value.to_string())),
    }
}

fn apply_values(cmd: &mut tokio::process::Command, args: &Value) {
    if let Some(vals) = args["values"].as_object() {
        let mut flat = Vec::new();
        for (k, v) in vals {
            flatten_values(k, v, &mut flat);
        }
        for (k, v) in &flat {
            cmd.arg("--set").arg(format!("{}={}", k, v));
        }
    }
}

async fn run_helm_command(mut cmd: tokio::process::Command) -> ToolResult {
    match cmd.output().await {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let combined = if stderr.is_empty() {
                stdout
            } else if stdout.is_empty() {
                stderr
            } else {
                format!("{}\n{}", stdout, stderr)
            };

            ToolResult {
                success: output.status.success(),
                output: combined,
            }
        }
        Err(e) => ToolResult {
            success: false,
            output: format!("failed to execute helm: {} (is helm installed?)", e),
        },
    }
}
