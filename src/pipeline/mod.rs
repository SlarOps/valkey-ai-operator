pub mod planner;
pub mod simulator;
pub mod executor;
pub mod verifier;

use crate::skill::types::{LoadedSkill, RiskLevel};
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
            pipeline_timeout_secs: 600,
            agent_timeout_secs: 300,
            llm_call_timeout_secs: 60,
            max_iterations: 50,
            model: "claude-sonnet-4-20250514".to_string(),
        }
    }
}

pub enum PipelineResult {
    Success { actions_taken: Vec<String> },
    Failed { reason: String, partial_actions: Vec<String> },
    SimulatorRejected { reason: String },
    Timeout,
}

/// Run the multi-agent pipeline for a given event
pub async fn run_pipeline(
    snapshot: &StateSnapshot,
    skill: &LoadedSkill,
    risk: RiskLevel,
    provider: Arc<dyn Provider>,
    client: Client,
    config: &PipelineConfig,
    monitor_registry: Arc<Mutex<MonitorRegistry>>,
    guardrails: Option<GuardrailSpec>,
) -> Result<PipelineResult> {
    let timeout = tokio::time::Duration::from_secs(config.pipeline_timeout_secs);

    let result = tokio::time::timeout(timeout, async {
        match risk {
            RiskLevel::Low => {
                info!("Pipeline: low risk → Executor (autonomous) → Verifier");
                let exec_result = executor::run_autonomous(
                    snapshot, skill, provider.clone(), client.clone(), config,
                    monitor_registry.clone(), guardrails.clone(),
                ).await?;

                let _verified = verifier::run(
                    snapshot, skill, provider.clone(), client.clone(), config,
                    monitor_registry.clone(), guardrails.clone(),
                ).await?;

                Ok(exec_result)
            }
            RiskLevel::Medium => {
                info!("Pipeline: medium risk → Planner → Executor (plan) → Verifier");
                let plan = planner::run(
                    snapshot, skill, provider.clone(), client.clone(), config,
                    monitor_registry.clone(), guardrails.clone(),
                ).await?;

                let exec_result = executor::run_plan(
                    &plan, snapshot, skill, provider.clone(), client.clone(), config,
                    monitor_registry.clone(), guardrails.clone(),
                ).await?;

                let _verified = verifier::run(
                    snapshot, skill, provider.clone(), client.clone(), config,
                    monitor_registry.clone(), guardrails.clone(),
                ).await?;

                Ok(exec_result)
            }
            RiskLevel::High => {
                info!("Pipeline: high risk → Planner → Simulator → Executor (plan) → Verifier");
                let max_retries = 2;
                let mut last_rejection = String::new();

                for attempt in 0..=max_retries {
                    let plan = if attempt == 0 {
                        planner::run(
                            snapshot, skill, provider.clone(), client.clone(), config,
                            monitor_registry.clone(), guardrails.clone(),
                        ).await?
                    } else {
                        info!("Pipeline: re-planning (attempt {}/{}) with simulator feedback", attempt + 1, max_retries + 1);
                        planner::run_with_feedback(
                            snapshot, skill, provider.clone(), client.clone(), config,
                            monitor_registry.clone(), guardrails.clone(),
                            &last_rejection,
                        ).await?
                    };

                    let sim_result = simulator::run(
                        &plan, snapshot, skill, provider.clone(), client.clone(), config,
                        monitor_registry.clone(), guardrails.clone(),
                    ).await?;

                    if sim_result.approved {
                        let exec_result = executor::run_plan(
                            &plan, snapshot, skill, provider.clone(), client.clone(), config,
                            monitor_registry.clone(), guardrails.clone(),
                        ).await?;

                        let _verified = verifier::run(
                            snapshot, skill, provider.clone(), client.clone(), config,
                            monitor_registry.clone(), guardrails.clone(),
                        ).await?;

                        return Ok(exec_result);
                    }

                    warn!("Simulator rejected plan (attempt {}): {}", attempt + 1, sim_result.reason);
                    last_rejection = sim_result.reason;
                }

                Ok(PipelineResult::SimulatorRejected { reason: last_rejection })
            }
        }
    }).await;

    match result {
        Ok(inner) => inner,
        Err(_) => {
            warn!("Pipeline timed out after {}s", config.pipeline_timeout_secs);
            Ok(PipelineResult::Timeout)
        }
    }
}
