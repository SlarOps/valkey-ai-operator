pub mod executor;

use crate::skill::types::LoadedSkill;
use crate::types::StateSnapshot;
use crate::agent::provider::Provider;
use crate::crd::GuardrailSpec;
use crate::monitor::registry::MonitorRegistry;
use anyhow::Result;
use std::sync::{Arc, Mutex};
use kube::Client;
use tracing::{info, warn};

pub struct PipelineConfig {
    pub pipeline_timeout_secs: u64,
    pub agent_timeout_secs: u64,
    pub llm_call_timeout_secs: u64,
    pub max_iterations: u32,
    pub model: String,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            pipeline_timeout_secs: 900,
            agent_timeout_secs: 900,
            llm_call_timeout_secs: 120,
            max_iterations: 50,
            model: "claude-sonnet-4-20250514".to_string(),
        }
    }
}

pub enum PipelineResult {
    Success { actions_taken: Vec<String> },
    Failed { reason: String, partial_actions: Vec<String> },
    Timeout,
}

/// Run the single-agent pipeline: one autonomous agent that reasons continuously
pub async fn run_pipeline(
    snapshot: &StateSnapshot,
    skill: &LoadedSkill,
    provider: Arc<dyn Provider>,
    client: Client,
    config: &PipelineConfig,
    monitor_registry: Arc<Mutex<MonitorRegistry>>,
    guardrails: Option<GuardrailSpec>,
) -> Result<PipelineResult> {
    let timeout = tokio::time::Duration::from_secs(config.pipeline_timeout_secs);

    let result = tokio::time::timeout(timeout, async {
        info!("Pipeline: single agent → continuous reasoning");
        executor::run(
            snapshot, skill, provider.clone(), client.clone(), config,
            monitor_registry.clone(), guardrails.clone(),
        ).await
    }).await;

    match result {
        Ok(inner) => inner,
        Err(_) => {
            warn!("Pipeline timed out after {}s", config.pipeline_timeout_secs);
            Ok(PipelineResult::Timeout)
        }
    }
}
