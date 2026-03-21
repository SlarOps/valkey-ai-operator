pub mod desired_state;
pub mod fs;
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

/// Register all tools for the single autonomous agent
pub fn register_tools(
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

    let skill_dir = &skill.skill_dir;

    let mut tools: Vec<Box<dyn Tool>> = vec![
        // State & status
        Box::new(state::GetState::new(
            client.clone(), resource_namespace, resource_name,
            &skill.config.name, goal, image, monitor_registry.clone(),
        )),
        Box::new(state::UpdateStatus::new(client.clone(), resource_namespace, resource_name)),
        // K8s read tools
        Box::new(k8s::GetPodLogs::new(client.clone(), resource_namespace)),
        Box::new(k8s::WaitForReady::new(client.clone(), resource_namespace, resource_name)),
        Box::new(k8s::GetEvents::new(client.clone(), resource_namespace, resource_name)),
        Box::new(k8s::KubectlDescribe::new(client.clone(), resource_namespace)),
        Box::new(k8s::KubectlGet::new(client.clone(), resource_namespace)),
        // K8s mutation tools
        Box::new(k8s::KubectlScale::new(client.clone(), resource_namespace, guardrails.clone())),
        Box::new(k8s::KubectlPatch::new(client.clone(), resource_namespace, guardrails.clone())),
        Box::new(k8s::KubectlExec::new(client.clone(), resource_namespace, denied_commands.clone())),
        // Template & action tools
        Box::new(runtime::RunAction::new(
            client.clone(), skill.clone(), resource_namespace, resource_name, denied_commands.clone(),
        )),
        Box::new(runtime::ApplyTemplate::new(
            client.clone(), skill.clone(), resource_namespace, resource_name, resource_uid, image,
        )),
        // Helm tools
        Box::new(helm::HelmInstall::new(resource_namespace)),
        Box::new(helm::HelmUpgrade::new(resource_namespace)),
        Box::new(helm::HelmStatus::new(resource_namespace)),
        Box::new(helm::HelmGetValues::new(resource_namespace)),
        Box::new(helm::HelmShowValues::new()),
        // Filesystem tools (sandboxed to skill directory)
        Box::new(fs::FileRead::new(skill_dir)),
        Box::new(fs::Ls::new(skill_dir)),
        Box::new(fs::Glob::new(skill_dir)),
        Box::new(fs::Grep::new(skill_dir)),
        Box::new(fs::ContentSearch::new(skill_dir)),
        Box::new(fs::FileList::new(skill_dir)),
    ];

    // Filter by skill allowed-tools if specified
    if let Some(allowed) = &skill.config.allowed_tools {
        let allowed_set: HashSet<&str> = allowed.split(',').map(|s| s.trim()).collect();
        tools.retain(|t| allowed_set.contains(t.name()));
    }

    tools
}
