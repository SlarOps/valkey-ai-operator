use crate::agent::agent::AutonomousAgent;
use crate::agent::provider::Provider;
use crate::agent::types::AgentConfig;
use crate::crd::GuardrailSpec;
use crate::monitor::registry::MonitorRegistry;
use crate::skill::types::LoadedSkill;
use crate::tools;
use crate::types::StateSnapshot;
use super::{PipelineConfig, PipelineResult};
use super::planner::ActionPlan;
use anyhow::{anyhow, Result};
use kube::Client;
use std::sync::{Arc, Mutex};
use tracing::info;

/// Autonomous mode: agent receives state + skill, decides what to do (low risk)
pub async fn run_autonomous(
    snapshot: &StateSnapshot,
    skill: &LoadedSkill,
    provider: Arc<dyn Provider>,
    client: Client,
    config: &PipelineConfig,
    monitor_registry: Arc<Mutex<MonitorRegistry>>,
    guardrails: Option<GuardrailSpec>,
) -> Result<PipelineResult> {
    info!("Executor agent starting (autonomous mode)");

    let tools = tools::register_tools_for_role(
        "executor", client, Arc::new(skill.clone()),
        &snapshot.resource.name, &snapshot.resource.namespace,
        &snapshot.resource.uid, &snapshot.resource.image,
        &snapshot.resource.goal, monitor_registry, guardrails,
    );

    let system_prompt = skill.agent_prompts.get("executor")
        .cloned()
        .unwrap_or_else(|| default_executor_prompt());

    let agent_config = AgentConfig {
        model: config.model.clone(),
        max_iterations: config.max_iterations,
        temperature: 0.0,
    };

    let mut agent = AutonomousAgent::new(provider, tools, agent_config);

    let user_message = format!(
        "## Skill Knowledge\n{}\n\n## Current State\n{}\n\nDecide what to do and execute.",
        skill.body, snapshot.to_agent_message()
    );

    let timeout = tokio::time::Duration::from_secs(config.agent_timeout_secs);
    let result = tokio::time::timeout(timeout, agent.run(&user_message, &system_prompt)).await
        .map_err(|_| anyhow!("Executor agent timed out"))??;

    Ok(PipelineResult::Success { actions_taken: result.actions_taken })
}

/// Plan mode: agent receives approved plan, executes step-by-step (medium/high risk)
pub async fn run_plan(
    plan: &ActionPlan,
    snapshot: &StateSnapshot,
    skill: &LoadedSkill,
    provider: Arc<dyn Provider>,
    client: Client,
    config: &PipelineConfig,
    monitor_registry: Arc<Mutex<MonitorRegistry>>,
    guardrails: Option<GuardrailSpec>,
) -> Result<PipelineResult> {
    info!("Executor agent starting (plan mode, {} steps)", plan.steps.len());

    let tools = tools::register_tools_for_role(
        "executor", client, Arc::new(skill.clone()),
        &snapshot.resource.name, &snapshot.resource.namespace,
        &snapshot.resource.uid, &snapshot.resource.image,
        &snapshot.resource.goal, monitor_registry, guardrails,
    );

    let system_prompt = format!(
        "{}\n\nIMPORTANT: Execute the provided plan step-by-step. Do NOT deviate from the plan.",
        skill.agent_prompts.get("executor").cloned().unwrap_or_else(|| default_executor_prompt())
    );

    let agent_config = AgentConfig {
        model: config.model.clone(),
        max_iterations: config.max_iterations,
        temperature: 0.0,
    };

    let mut agent = AutonomousAgent::new(provider, tools, agent_config);

    let plan_json = serde_json::to_string_pretty(plan)?;
    let user_message = format!(
        "## Skill Knowledge\n{}\n\n## Current State\n{}\n\n## Action Plan to Execute\n{}\n\nExecute this plan step by step.",
        skill.body, snapshot.to_agent_message(), plan_json
    );

    let timeout = tokio::time::Duration::from_secs(config.agent_timeout_secs);
    let result = tokio::time::timeout(timeout, agent.run(&user_message, &system_prompt)).await
        .map_err(|_| anyhow!("Executor agent timed out"))??;

    Ok(PipelineResult::Success { actions_taken: result.actions_taken })
}

fn default_executor_prompt() -> String {
    "You are an Executor agent for a Kubernetes operator. \
     Use the available tools to manage resources. \
     Always verify your actions succeeded before moving on.".to_string()
}
