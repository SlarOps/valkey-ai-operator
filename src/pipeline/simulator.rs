use crate::agent::agent::AutonomousAgent;
use crate::agent::provider::Provider;
use crate::agent::types::AgentConfig;
use crate::crd::GuardrailSpec;
use crate::monitor::registry::MonitorRegistry;
use crate::skill::types::LoadedSkill;
use crate::tools;
use crate::types::StateSnapshot;
use super::PipelineConfig;
use super::planner::ActionPlan;
use anyhow::{anyhow, Result};
use kube::Client;
use std::sync::{Arc, Mutex};
use tracing::info;

pub struct SimulatorResult {
    pub approved: bool,
    pub reason: String,
}

pub async fn run(
    plan: &ActionPlan,
    snapshot: &StateSnapshot,
    skill: &LoadedSkill,
    provider: Arc<dyn Provider>,
    client: Client,
    config: &PipelineConfig,
    monitor_registry: Arc<Mutex<MonitorRegistry>>,
    guardrails: Option<GuardrailSpec>,
) -> Result<SimulatorResult> {
    info!("Simulator agent starting");

    let tools = tools::register_tools_for_role(
        "simulator", client, Arc::new(skill.clone()),
        &snapshot.resource.name, &snapshot.resource.namespace,
        &snapshot.resource.uid, snapshot.resource.image.as_deref().unwrap_or(""),
        &snapshot.resource.goal, monitor_registry, guardrails,
    );

    let system_prompt = skill.agent_prompts.get("simulator")
        .cloned()
        .unwrap_or_else(|| default_simulator_prompt());

    let agent_config = AgentConfig {
        model: config.model.clone(),
        max_iterations: config.max_iterations.min(10),
        temperature: 0.0,
    };

    let mut agent = AutonomousAgent::new(provider, tools, agent_config);

    let plan_json = serde_json::to_string_pretty(plan)?;
    let user_message = format!(
        "## Skill Knowledge\n{}\n\n## Current State\n{}\n\n## Action Plan to Validate\n{}\n\nIs this plan safe? Respond with APPROVED or REJECTED followed by reason.",
        skill.body, snapshot.to_agent_message(), plan_json
    );

    let timeout = tokio::time::Duration::from_secs(config.agent_timeout_secs);
    let result = tokio::time::timeout(timeout, agent.run(&user_message, &system_prompt)).await
        .map_err(|_| anyhow!("Simulator agent timed out"))??;

    let reason = result.text.unwrap_or_else(|| "No response".to_string());
    let approved = reason.to_uppercase().contains("APPROVED");

    Ok(SimulatorResult { approved, reason })
}

fn default_simulator_prompt() -> String {
    "You are a Simulator agent. Validate action plans for safety. \
     Check preconditions, look for data loss risks, verify the plan makes sense. \
     Respond with APPROVED or REJECTED followed by your reasoning.".to_string()
}
