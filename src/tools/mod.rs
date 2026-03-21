pub mod desired_state;
pub mod helm;
pub mod k8s;
pub mod runtime;
pub mod state;
pub mod template;

use crate::agent::tool::Tool;
use crate::crd::GuardrailSpec;
use crate::monitor::registry::MonitorRegistry;
use crate::skill::types::LoadedSkill;
use kube::Client;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// Register tools for a specific agent role
pub fn register_tools_for_role(
    role: &str,
    client: Client,
    skill: Arc<LoadedSkill>,
    resource_name: &str,
    resource_namespace: &str,
    resource_uid: &str,
    image: &str,
    goal: &str,
    monitor_registry: Arc<Mutex<MonitorRegistry>>,
    guardrails: Option<GuardrailSpec>,
) -> Vec<Box<dyn Tool>> {
    let denied_commands = guardrails
        .as_ref()
        .map(|g| g.denied_commands.clone())
        .unwrap_or_default();

    let mut tools: Vec<Box<dyn Tool>> = match role {
        "planner" | "simulator" => vec![
            // Read-only tools for planning and simulation
            Box::new(state::GetState::new(
                client.clone(), resource_namespace, resource_name,
                &skill.config.name, goal, image, monitor_registry.clone(),
            )),
            Box::new(k8s::GetEvents::new(client.clone(), resource_namespace, resource_name)),
            Box::new(k8s::KubectlDescribe::new(client.clone(), resource_namespace)),
            Box::new(k8s::KubectlGet::new(client.clone(), resource_namespace)),
            // Helm read-only tools
            Box::new(helm::HelmStatus::new(resource_namespace)),
            Box::new(helm::HelmGetValues::new(resource_namespace)),
            Box::new(helm::HelmShowValues::new()),
        ],
        "executor" => vec![
            // Existing tools
            Box::new(runtime::RunAction::new(
                client.clone(), skill.clone(), resource_namespace, resource_name, denied_commands.clone(),
            )),
            Box::new(runtime::ApplyTemplate::new(
                client.clone(), skill.clone(), resource_namespace, resource_name, resource_uid, image,
            )),
            Box::new(state::GetState::new(
                client.clone(), resource_namespace, resource_name,
                &skill.config.name, goal, image, monitor_registry.clone(),
            )),
            Box::new(state::UpdateStatus::new(client.clone(), resource_namespace, resource_name)),
            Box::new(k8s::GetPodLogs::new(client.clone(), resource_namespace)),
            Box::new(k8s::WaitForReady::new(client.clone(), resource_namespace, resource_name)),
            Box::new(k8s::GetEvents::new(client.clone(), resource_namespace, resource_name)),
            // New read-only tools
            Box::new(k8s::KubectlDescribe::new(client.clone(), resource_namespace)),
            Box::new(k8s::KubectlGet::new(client.clone(), resource_namespace)),
            // New mutation tools
            Box::new(k8s::KubectlScale::new(client.clone(), resource_namespace, guardrails.clone())),
            Box::new(k8s::KubectlPatch::new(client.clone(), resource_namespace, guardrails.clone())),
            Box::new(k8s::KubectlExec::new(client.clone(), resource_namespace, denied_commands.clone())),
            // Helm tools
            Box::new(helm::HelmInstall::new(resource_namespace)),
            Box::new(helm::HelmUpgrade::new(resource_namespace)),
            Box::new(helm::HelmStatus::new(resource_namespace)),
            Box::new(helm::HelmGetValues::new(resource_namespace)),
            Box::new(helm::HelmShowValues::new()),
        ],
        "verifier" => vec![
            Box::new(state::GetState::new(
                client.clone(), resource_namespace, resource_name,
                &skill.config.name, goal, image, monitor_registry.clone(),
            )),
            Box::new(state::UpdateStatus::new(client.clone(), resource_namespace, resource_name)),
            Box::new(k8s::GetEvents::new(client.clone(), resource_namespace, resource_name)),
            // New read-only tools for verification
            Box::new(k8s::KubectlDescribe::new(client.clone(), resource_namespace)),
            Box::new(k8s::KubectlGet::new(client.clone(), resource_namespace)),
            Box::new(k8s::GetPodLogs::new(client.clone(), resource_namespace)),
            // Helm read-only tools
            Box::new(helm::HelmStatus::new(resource_namespace)),
            Box::new(helm::HelmGetValues::new(resource_namespace)),
        ],
        _ => vec![],
    };

    // Filter by skill allowed-tools if specified
    if let Some(allowed) = &skill.config.allowed_tools {
        let allowed_set: HashSet<&str> = allowed.split(',').map(|s| s.trim()).collect();
        tools.retain(|t| allowed_set.contains(t.name()));
    }

    tools
}
