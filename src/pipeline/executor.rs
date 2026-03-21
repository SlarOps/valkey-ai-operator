use crate::agent::agent::AutonomousAgent;
use crate::agent::provider::Provider;
use crate::agent::types::AgentConfig;
use crate::crd::GuardrailSpec;
use crate::monitor::registry::MonitorRegistry;
use crate::skill::types::LoadedSkill;
use crate::tools;
use crate::types::StateSnapshot;
use super::{PipelineConfig, PipelineResult};
use anyhow::{anyhow, Result};
use kube::Client;
use std::sync::{Arc, Mutex};
use tracing::info;

/// Single autonomous agent: receives state + skill knowledge, reasons continuously
/// using tools until goal is achieved. No rigid plan — agent observes, decides, acts, adapts.
pub async fn run(
    snapshot: &StateSnapshot,
    skill: &LoadedSkill,
    provider: Arc<dyn Provider>,
    client: Client,
    config: &PipelineConfig,
    monitor_registry: Arc<Mutex<MonitorRegistry>>,
    guardrails: Option<GuardrailSpec>,
) -> Result<PipelineResult> {
    info!("Agent starting (single-agent continuous reasoning)");

    let tools = tools::register_tools(
        client, Arc::new(skill.clone()),
        &snapshot.resource.name, &snapshot.resource.namespace,
        &snapshot.resource.uid, snapshot.resource.image.as_deref().unwrap_or(""),
        &snapshot.resource.goal, monitor_registry, guardrails,
    );

    let system_prompt = skill.agent_prompts.get("agent")
        .cloned()
        .unwrap_or_else(|| default_agent_prompt());

    let agent_config = AgentConfig {
        model: config.model.clone(),
        max_iterations: config.max_iterations,
        temperature: 0.0,
    };

    let mut agent = AutonomousAgent::new(provider, tools, agent_config);

    let user_message = format!(
        "## Skill Knowledge\n{}\n\n## Current State\n{}\n\nAchieve the goal. Observe the current state, decide what needs to be done, execute, verify, and update status.",
        skill.body, snapshot.to_agent_message()
    );

    let timeout = tokio::time::Duration::from_secs(config.agent_timeout_secs);
    let result = tokio::time::timeout(timeout, agent.run(&user_message, &system_prompt)).await
        .map_err(|_| anyhow!("Agent timed out"))??;

    Ok(PipelineResult::Success { actions_taken: result.actions_taken })
}

fn default_agent_prompt() -> String {
    "You are an autonomous Kubernetes operator agent. You have full access to tools for managing resources.\n\n\
     ## How to work\n\
     1. **Observe**: Use get_state, helm_status, kubectl_get to understand the current situation\n\
     2. **Decide**: Based on the skill knowledge and current state, determine what actions are needed\n\
     3. **Act**: Execute the necessary operations using available tools\n\
     4. **Verify**: After each action, check that it succeeded (e.g., CLUSTER INFO, pod status)\n\
     5. **Adapt**: If something fails, investigate why and try a different approach\n\
     6. **Complete**: Call update_status as your final action to report the result\n\n\
     ## Key principles\n\
     - Always check current state before acting — don't assume\n\
     - If a tool call fails, read the error and adjust your approach\n\
     - Never retry the exact same failed operation — try something different\n\
     - Verify your work after each significant action\n\
     - update_status MUST be your final action (Running if healthy, Failed if unrecoverable)".to_string()
}
