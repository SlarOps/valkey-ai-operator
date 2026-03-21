use crate::agent::agent::AutonomousAgent;
use crate::agent::provider::Provider;
use crate::agent::types::AgentConfig;
use crate::crd::GuardrailSpec;
use crate::monitor::registry::MonitorRegistry;
use crate::skill::types::LoadedSkill;
use crate::tools;
use crate::types::StateSnapshot;
use super::PipelineConfig;
use anyhow::{anyhow, Result};
use kube::Client;
use std::sync::{Arc, Mutex};
use tracing::info;

pub async fn run(
    snapshot: &StateSnapshot,
    skill: &LoadedSkill,
    provider: Arc<dyn Provider>,
    client: Client,
    config: &PipelineConfig,
    monitor_registry: Arc<Mutex<MonitorRegistry>>,
    guardrails: Option<GuardrailSpec>,
) -> Result<bool> {
    info!("Verifier agent starting");

    let tools = tools::register_tools_for_role(
        "verifier", client, Arc::new(skill.clone()),
        &snapshot.resource.name, &snapshot.resource.namespace,
        &snapshot.resource.uid, &snapshot.resource.image,
        &snapshot.resource.goal, monitor_registry, guardrails,
    );

    let system_prompt = skill.agent_prompts.get("verifier")
        .cloned()
        .unwrap_or_else(|| default_verifier_prompt());

    let agent_config = AgentConfig {
        model: config.model.clone(),
        max_iterations: config.max_iterations.min(10),
        temperature: 0.0,
    };

    let mut agent = AutonomousAgent::new(provider, tools, agent_config);

    let user_message = format!(
        "## Skill Knowledge\n{}\n\n## Goal\n{}\n\nVerify the current state matches the goal. Use get_state to check. Call update_status as your final action.",
        skill.body, snapshot.resource.goal
    );

    let timeout = tokio::time::Duration::from_secs(config.agent_timeout_secs);
    let result = tokio::time::timeout(timeout, agent.run(&user_message, &system_prompt)).await
        .map_err(|_| anyhow!("Verifier agent timed out"))??;

    // Check if verifier called update_status
    let called_update = result.actions_taken.iter().any(|a| a.contains("update_status"));
    if !called_update {
        info!("Verifier did not call update_status — may need manual check");
    }

    Ok(true)
}

fn default_verifier_prompt() -> String {
    "You are a Verifier agent. Check if the actual state matches the declared goal. \
     Use get_state to read the current state. \
     Call update_status to set the appropriate phase (Running if healthy, Healing if issues found). \
     update_status MUST be your final action.".to_string()
}
