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
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tracing::info;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionPlan {
    #[serde(default)]
    pub plan_id: String,
    pub goal: String,
    pub steps: Vec<PlanStep>,
    #[serde(default = "default_rollback")]
    pub rollback_on_failure: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub order: u32,
    pub action: String,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub args: Option<serde_json::Value>,
    #[serde(default)]
    pub vars: Option<serde_json::Value>,
    #[serde(default)]
    pub risk: Option<String>,
    #[serde(default)]
    pub count: Option<u32>,
    #[serde(default)]
    pub timeout: Option<String>,
}

fn default_rollback() -> String { "stop_and_report".to_string() }

pub async fn run(
    snapshot: &StateSnapshot,
    skill: &LoadedSkill,
    provider: Arc<dyn Provider>,
    client: Client,
    config: &PipelineConfig,
    monitor_registry: Arc<Mutex<MonitorRegistry>>,
    guardrails: Option<GuardrailSpec>,
) -> Result<ActionPlan> {
    info!("Planner agent starting");

    let tools = tools::register_tools_for_role(
        "planner", client, Arc::new(skill.clone()),
        &snapshot.resource.name, &snapshot.resource.namespace,
        &snapshot.resource.image, &snapshot.resource.goal,
        monitor_registry, guardrails,
    );

    let system_prompt = skill.agent_prompts.get("planner")
        .cloned()
        .unwrap_or_else(|| default_planner_prompt());

    let agent_config = AgentConfig {
        model: config.model.clone(),
        max_iterations: config.max_iterations.min(10), // planner needs fewer iterations
        temperature: 0.0,
    };

    let mut agent = AutonomousAgent::new(provider, tools, agent_config);

    let user_message = format!(
        "## Skill Knowledge\n{}\n\n## Current State\n{}\n\nCreate an action plan as JSON.",
        skill.body, snapshot.to_agent_message()
    );

    let timeout = tokio::time::Duration::from_secs(config.agent_timeout_secs);
    let result = tokio::time::timeout(timeout, agent.run(&user_message, &system_prompt)).await
        .map_err(|_| anyhow!("Planner agent timed out"))??;

    // Parse action plan from agent response
    let text = result.text.unwrap_or_default();
    parse_action_plan(&text, &snapshot.resource.goal)
}

pub async fn run_with_feedback(
    snapshot: &StateSnapshot,
    skill: &LoadedSkill,
    provider: Arc<dyn Provider>,
    client: Client,
    config: &PipelineConfig,
    monitor_registry: Arc<Mutex<MonitorRegistry>>,
    guardrails: Option<GuardrailSpec>,
    simulator_feedback: &str,
) -> Result<ActionPlan> {
    info!("Planner agent re-planning with simulator feedback");

    let tools = tools::register_tools_for_role(
        "planner", client, Arc::new(skill.clone()),
        &snapshot.resource.name, &snapshot.resource.namespace,
        &snapshot.resource.image, &snapshot.resource.goal,
        monitor_registry, guardrails,
    );

    let system_prompt = skill.agent_prompts.get("planner")
        .cloned()
        .unwrap_or_else(|| default_planner_prompt());

    let agent_config = AgentConfig {
        model: config.model.clone(),
        max_iterations: config.max_iterations.min(10),
        temperature: 0.0,
    };

    let mut agent = AutonomousAgent::new(provider, tools, agent_config);

    let user_message = format!(
        "## Skill Knowledge\n{}\n\n## Current State\n{}\n\n## Previous Plan Rejected by Simulator\nThe simulator rejected your previous plan with this feedback:\n{}\n\nFix the issues and create a corrected action plan as JSON.",
        skill.body, snapshot.to_agent_message(), simulator_feedback
    );

    let timeout = tokio::time::Duration::from_secs(config.agent_timeout_secs);
    let result = tokio::time::timeout(timeout, agent.run(&user_message, &system_prompt)).await
        .map_err(|_| anyhow!("Planner agent timed out"))??;

    let text = result.text.unwrap_or_default();
    parse_action_plan(&text, &snapshot.resource.goal)
}

fn parse_action_plan(text: &str, goal: &str) -> Result<ActionPlan> {
    // Try to find JSON in the response
    if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            let json_str = &text[start..=end];
            if let Ok(plan) = serde_json::from_str::<ActionPlan>(json_str) {
                return Ok(plan);
            }
        }
    }

    // If no valid JSON found, create a simple plan from the text
    Ok(ActionPlan {
        plan_id: uuid_simple(),
        goal: goal.to_string(),
        steps: vec![],
        rollback_on_failure: "stop_and_report".to_string(),
    })
}

fn uuid_simple() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    format!("plan-{}", t.as_millis())
}

fn default_planner_prompt() -> String {
    "You are a Planner agent for a Kubernetes operator. \
     Given the current state and goal, create an action plan as JSON. \
     Use get_state to understand the current situation. \
     Output a JSON object with: plan_id, goal, steps (array of {order, action, ...}), rollback_on_failure.".to_string()
}
