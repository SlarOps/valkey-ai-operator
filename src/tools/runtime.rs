use std::sync::Arc;
use std::time::Instant;

use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, AttachParams};
use kube::Client;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tracing::info;

use crate::agent::tool::{Tool, ToolSafety};
use crate::agent::types::ToolResult;
use crate::skill::types::LoadedSkill;
use crate::tools::k8s::apply_server_side;
use crate::tools::template::render_template;

/// Shell-escape a value for safe use in `KEY=VALUE` env var assignment.
fn shell_escape(s: &str) -> String {
    if s.chars().all(|c| c.is_alphanumeric() || ".-_/:@,".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

// ---------------------------------------------------------------------------
// RunAction — execute a skill-defined action script in a pod
// ---------------------------------------------------------------------------

pub struct RunAction {
    client: Client,
    skill: Arc<LoadedSkill>,
    namespace: String,
    resource_name: String,
    denied_commands: Vec<String>,
}

impl RunAction {
    pub fn new(
        client: Client,
        skill: Arc<LoadedSkill>,
        namespace: &str,
        resource_name: &str,
        denied_commands: Vec<String>,
    ) -> Self {
        Self {
            client,
            skill,
            namespace: namespace.to_string(),
            resource_name: resource_name.to_string(),
            denied_commands,
        }
    }
}

#[async_trait::async_trait]
impl Tool for RunAction {
    fn name(&self) -> &str { "run_action" }

    fn description(&self) -> &str {
        "Execute a skill-defined action script in a pod."
    }

    fn parameters_schema(&self) -> Value {
        // Build per-action documentation from skill config
        let actions_desc: Vec<String> = self.skill.config.actions.iter().map(|a| {
            let params = if a.params.is_empty() {
                "none".to_string()
            } else {
                a.params.join(", ")
            };
            let desc = a.description.as_deref().unwrap_or("");
            format!("- {}: {} (params: {})", a.name, desc, params)
        }).collect();
        let args_description = format!(
            "Key-value arguments passed as env vars. Available actions:\n{}",
            actions_desc.join("\n")
        );

        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Action name as defined in the skill config"
                },
                "args": {
                    "type": "object",
                    "description": args_description
                },
                "pod_name": {
                    "type": "string",
                    "description": "Name of the pod in which to execute the action"
                }
            },
            "required": ["name", "pod_name"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::Validated }

    async fn execute(&self, args: Value) -> ToolResult {
        let action_name = match args["name"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing action name".into() },
        };
        let pod_name = match args["pod_name"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing pod_name".into() },
        };

        // Lookup action in skill config
        let action = match self.skill.config.actions.iter().find(|a| a.name == action_name) {
            Some(a) => a,
            None => return ToolResult {
                success: false,
                output: format!("action '{}' not found in skill '{}'", action_name, self.skill.config.name),
            },
        };

        // Check args against denied commands
        if let Some(args_obj) = args["args"].as_object() {
            for val in args_obj.values() {
                if let Some(s) = val.as_str() {
                    for denied in &self.denied_commands {
                        if s.contains(denied.as_str()) {
                            return ToolResult {
                                success: false,
                                output: format!("guardrail: argument contains denied command '{}'", denied),
                            };
                        }
                    }
                }
            }
        }

        // Read script from skill_dir
        let script_path = self.skill.skill_dir.join(&action.script);
        let script_content = match tokio::fs::read_to_string(&script_path).await {
            Ok(s) => s,
            Err(e) => return ToolResult {
                success: false,
                output: format!("failed to read script '{}': {}", script_path.display(), e),
            },
        };

        // Build script: export env vars then run script content
        let mut script_lines: Vec<String> = Vec::new();
        if let Some(args_obj) = args["args"].as_object() {
            for (k, v) in args_obj {
                let val = if let Some(s) = v.as_str() {
                    s.to_string()
                } else if let Some(n) = v.as_i64() {
                    n.to_string()
                } else if let Some(n) = v.as_f64() {
                    n.to_string()
                } else if let Some(b) = v.as_bool() {
                    b.to_string()
                } else {
                    continue;
                };
                script_lines.push(format!("export {}={}", k.to_uppercase(), shell_escape(&val)));
            }
        }
        script_lines.push(script_content);
        let full_script = script_lines.join("\n");

        let command = vec!["bash".to_string(), "-c".to_string(), full_script];

        // Exec in pod via kube attach API
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let ap = AttachParams {
            container: None,
            stdout: true,
            stderr: true,
            stdin: false,
            tty: false,
            ..Default::default()
        };

        let start = Instant::now();
        match pods.exec(pod_name, command, &ap).await {
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

                let duration_ms = start.elapsed().as_millis() as u64;
                info!("run_action '{}' on pod '{}': exit_code={}", action_name, pod_name, exit_code);

                let output = json!({
                    "exit_code": exit_code,
                    "stdout": stdout_str.trim(),
                    "stderr": stderr_str.trim(),
                    "duration_ms": duration_ms,
                });
                ToolResult { success: exit_code == 0, output: output.to_string() }
            }
            Err(e) => ToolResult { success: false, output: format!("error: {}", e) },
        }
    }
}

// ---------------------------------------------------------------------------
// ApplyTemplate — render and apply a skill template to Kubernetes
// ---------------------------------------------------------------------------

pub struct ApplyTemplate {
    client: Client,
    skill: Arc<LoadedSkill>,
    namespace: String,
    resource_name: String,
    image: String,
}

impl ApplyTemplate {
    pub fn new(
        client: Client,
        skill: Arc<LoadedSkill>,
        namespace: &str,
        resource_name: &str,
        image: &str,
    ) -> Self {
        Self {
            client,
            skill,
            namespace: namespace.to_string(),
            resource_name: resource_name.to_string(),
            image: image.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for ApplyTemplate {
    fn name(&self) -> &str { "apply_template" }

    fn description(&self) -> &str {
        "Render and apply a skill template to Kubernetes."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "template": {
                    "type": "string",
                    "description": "Template filename (without path) in the skill's templates/ directory"
                },
                "vars": {
                    "type": "object",
                    "description": "Additional template variables (name, namespace, image are auto-injected)"
                }
            },
            "required": ["template"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::Validated }

    async fn execute(&self, args: Value) -> ToolResult {
        let template_name = match args["template"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing template name".into() },
        };

        // Load template file from skill_dir/templates/
        let template_path = self.skill.skill_dir.join("templates").join(template_name);
        let template_content = match tokio::fs::read_to_string(&template_path).await {
            Ok(s) => s,
            Err(e) => return ToolResult {
                success: false,
                output: format!("failed to read template '{}': {}", template_path.display(), e),
            },
        };

        // Build vars: auto-inject name, namespace, image; merge with caller-provided vars
        let mut vars: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        vars.insert("name".to_string(), self.resource_name.clone());
        vars.insert("namespace".to_string(), self.namespace.clone());
        vars.insert("image".to_string(), self.image.clone());

        if let Some(extra) = args["vars"].as_object() {
            for (k, v) in extra {
                let val = if let Some(s) = v.as_str() {
                    s.to_string()
                } else if let Some(n) = v.as_i64() {
                    n.to_string()
                } else if let Some(n) = v.as_f64() {
                    n.to_string()
                } else if let Some(b) = v.as_bool() {
                    b.to_string()
                } else {
                    continue;
                };
                vars.insert(k.clone(), val);
            }
        }

        // Render template
        let rendered = match render_template(&template_content, &vars) {
            Ok(s) => s,
            Err(e) => return ToolResult {
                success: false,
                output: format!("template render error: {}", e),
            },
        };

        // Parse YAML into JSON Value
        let manifest: Value = match serde_yaml::from_str(&rendered) {
            Ok(v) => v,
            Err(e) => return ToolResult {
                success: false,
                output: format!("YAML parse error: {}", e),
            },
        };

        // kubectl apply server-side
        match apply_server_side(&self.client, &self.namespace, &manifest).await {
            Ok(msg) => {
                info!("apply_template '{}': {}", template_name, msg);
                let output = json!({ "applied": true, "message": msg });
                ToolResult { success: true, output: output.to_string() }
            }
            Err(e) => ToolResult { success: false, output: format!("apply error: {}", e) },
        }
    }
}
