use crate::channel::EventReceiver;
use crate::crd::GuardrailSpec;
use crate::monitor::registry::MonitorRegistry;
use crate::pipeline::{self, PipelineConfig, PipelineResult};
use crate::skill::types::LoadedSkill;
use crate::types::{CircuitBreaker, ResourceEvent};
use crate::agent::provider::Provider;
use kube::Client;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::{info, warn, error};

/// Minimum seconds between pipeline runs after a success (prevents event storm)
const COOLDOWN_AFTER_SUCCESS_SECS: u64 = 30;

pub struct AgentInstance {
    pub resource_name: String,
    pub resource_namespace: String,
    pub skill: Arc<LoadedSkill>,
    pub provider: Arc<dyn Provider>,
    pub client: Client,
    pub circuit_breaker: CircuitBreaker,
    pub config: PipelineConfig,
    pub monitor_registry: Arc<Mutex<MonitorRegistry>>,
    pub guardrails: Option<GuardrailSpec>,
    /// Track when last successful pipeline completed (for cooldown)
    last_success: Option<Instant>,
    /// Track whether goal has been achieved (phase=Running)
    goal_achieved: bool,
}

impl AgentInstance {
    pub fn new(
        resource_name: String,
        resource_namespace: String,
        skill: Arc<LoadedSkill>,
        provider: Arc<dyn Provider>,
        client: Client,
        config: PipelineConfig,
        monitor_registry: Arc<Mutex<MonitorRegistry>>,
        guardrails: Option<GuardrailSpec>,
    ) -> Self {
        Self {
            resource_name,
            resource_namespace,
            skill,
            provider,
            client,
            circuit_breaker: CircuitBreaker::new(3, 2),
            config,
            monitor_registry,
            guardrails,
            last_success: None,
            goal_achieved: false,
        }
    }

    /// Main event loop — receives events, runs single-agent pipeline
    pub async fn run(mut self, mut rx: EventReceiver) {
        info!("AgentInstance started for {}/{}", self.resource_namespace, self.resource_name);

        let mut pending_event: Option<ResourceEvent> = None;

        while let Some(event) = rx.recv().await {
            match event {
                ResourceEvent::Shutdown => {
                    info!("AgentInstance {}/{} shutting down", self.resource_namespace, self.resource_name);
                    break;
                }
                event => {
                    if pending_event.is_some() {
                        info!("Event queued, skipping duplicate for {}/{}", self.resource_namespace, self.resource_name);
                        continue;
                    }

                    self.handle_event(event).await;

                    if let Some(queued) = pending_event.take() {
                        self.handle_event(queued).await;
                    }
                }
            }
        }

        info!("AgentInstance {}/{} stopped", self.resource_namespace, self.resource_name);
    }

    async fn handle_event(&mut self, event: ResourceEvent) {
        let snapshot = match &event {
            ResourceEvent::Bootstrap(s)
            | ResourceEvent::MonitorTrigger(s)
            | ResourceEvent::SpecChange(s) => s.clone(),
            ResourceEvent::DriftDetected(s, drift_info) => {
                let mut enriched = s.clone();
                enriched.trigger.reason = format!(
                    "Child resources missing: {}. Re-apply from desired state.",
                    drift_info.missing_resources.join(", ")
                );
                enriched
            }
            ResourceEvent::Shutdown => return,
        };

        // ── Event filtering ──────────────────────────────────────────────
        // Skip monitor events when goal is already achieved (phase=Running)
        // Only SpecChange, DriftDetected, and Bootstrap should re-trigger
        if self.goal_achieved {
            match &event {
                ResourceEvent::MonitorTrigger(_) => {
                    info!(
                        "Skipping monitor event for {}/{}: goal already achieved",
                        self.resource_namespace, self.resource_name
                    );
                    return;
                }
                ResourceEvent::SpecChange(_) => {
                    // Spec changed → goal may have changed, reset achieved flag
                    info!("Spec changed for {}/{}, resetting goal_achieved", self.resource_namespace, self.resource_name);
                    self.goal_achieved = false;
                }
                _ => {}
            }
        }

        // Cooldown: skip events too soon after last success
        if let Some(last) = self.last_success {
            let elapsed = last.elapsed().as_secs();
            if elapsed < COOLDOWN_AFTER_SUCCESS_SECS {
                match &event {
                    ResourceEvent::SpecChange(_) | ResourceEvent::DriftDetected(_, _) => {
                        // Always process spec changes and drift, even during cooldown
                    }
                    _ => {
                        info!(
                            "Cooldown active for {}/{} ({}s remaining), skipping {:?}",
                            self.resource_namespace, self.resource_name,
                            COOLDOWN_AFTER_SUCCESS_SECS - elapsed,
                            snapshot.trigger.source,
                        );
                        return;
                    }
                }
            }
        }

        // Check circuit breaker
        if self.circuit_breaker.is_open() {
            warn!("Circuit breaker OPEN for {}/{}, skipping", self.resource_namespace, self.resource_name);
            return;
        }

        info!(
            "Processing event for {}/{}: trigger={}",
            self.resource_namespace, self.resource_name,
            snapshot.trigger.source,
        );

        let result = pipeline::run_pipeline(
            &snapshot,
            &self.skill,
            self.provider.clone(),
            self.client.clone(),
            &self.config,
            self.monitor_registry.clone(),
            self.guardrails.clone(),
        ).await;

        match result {
            Ok(PipelineResult::Success { actions_taken }) => {
                info!("Pipeline succeeded for {}/{}: {:?}", self.resource_namespace, self.resource_name, actions_taken);
                self.circuit_breaker.record_success();
                self.last_success = Some(Instant::now());
                // If agent called update_status with Running, mark goal achieved
                if actions_taken.iter().any(|a| a.contains("update_status") && a.contains("Running")) {
                    self.goal_achieved = true;
                    info!("Goal achieved for {}/{}, suppressing future monitor events", self.resource_namespace, self.resource_name);
                }
            }
            Ok(PipelineResult::Timeout) => {
                warn!("Pipeline timed out for {}/{}", self.resource_namespace, self.resource_name);
                self.circuit_breaker.record_failure();
            }
            Ok(PipelineResult::Failed { reason, partial_actions }) => {
                error!("Pipeline failed for {}/{}: {} (partial: {:?})", self.resource_namespace, self.resource_name, reason, partial_actions);
                self.circuit_breaker.record_failure();
            }
            Err(e) => {
                error!("Pipeline error for {}/{}: {}", self.resource_namespace, self.resource_name, e);
                self.circuit_breaker.record_failure();
            }
        }
    }
}
