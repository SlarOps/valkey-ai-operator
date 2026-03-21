use crate::channel::EventReceiver;
use crate::crd::GuardrailSpec;
use crate::monitor::registry::MonitorRegistry;
use crate::pipeline::{self, PipelineConfig, PipelineResult};
use crate::skill::types::{LoadedSkill, RiskLevel};
use crate::types::{CircuitBreaker, CircuitState, ResourceEvent};
use crate::agent::provider::Provider;
use kube::Client;
use std::sync::{Arc, Mutex};
use tracing::{info, warn, error};

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
        }
    }

    /// Main event loop — receives events, runs pipeline
    pub async fn run(mut self, mut rx: EventReceiver) {
        info!("AgentInstance started for {}/{}", self.resource_namespace, self.resource_name);

        // Queue for events that arrive while pipeline is running
        let mut pending_event: Option<ResourceEvent> = None;

        while let Some(event) = rx.recv().await {
            match event {
                ResourceEvent::Shutdown => {
                    info!("AgentInstance {}/{} shutting down", self.resource_namespace, self.resource_name);
                    break;
                }
                event => {
                    // Dedup: if we already have a pending event with same trigger source, skip
                    if pending_event.is_some() {
                        info!("Event queued, skipping duplicate for {}/{}", self.resource_namespace, self.resource_name);
                        continue;
                    }

                    self.handle_event(event).await;

                    // Process any pending event
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
                // Enrich snapshot with drift details so agent knows what's missing
                let mut enriched = s.clone();
                enriched.trigger.reason = format!(
                    "Child resources missing: {}. Re-apply from desired state.",
                    drift_info.missing_resources.join(", ")
                );
                enriched
            }
            ResourceEvent::Shutdown => return,
        };

        // Check circuit breaker
        if self.circuit_breaker.is_open() {
            warn!("Circuit breaker OPEN for {}/{}, skipping", self.resource_namespace, self.resource_name);
            return;
        }

        // Half-open: only allow low risk
        let risk = self.determine_risk(&event);
        if self.circuit_breaker.state == CircuitState::HalfOpen && risk != RiskLevel::Low {
            warn!("Circuit breaker HALF-OPEN for {}/{}, only low-risk allowed", self.resource_namespace, self.resource_name);
            return;
        }

        info!(
            "Processing event for {}/{}: trigger={}, risk={:?}",
            self.resource_namespace, self.resource_name,
            snapshot.trigger.source, risk
        );

        let result = pipeline::run_pipeline(
            &snapshot,
            &self.skill,
            risk,
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
            }
            Ok(PipelineResult::SimulatorRejected { reason }) => {
                warn!("Simulator rejected plan for {}/{}: {}", self.resource_namespace, self.resource_name, reason);
                // Don't count as failure — plan was safely rejected
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

    fn determine_risk(&self, event: &ResourceEvent) -> RiskLevel {
        match event {
            ResourceEvent::Bootstrap(_) => RiskLevel::High,
            ResourceEvent::SpecChange(_) => RiskLevel::Medium,
            ResourceEvent::DriftDetected(_, _) => RiskLevel::Low, // re-apply from desired state, no planning needed
            ResourceEvent::MonitorTrigger(snapshot) => {
                // Check which actions match the trigger, take max risk
                let source = &snapshot.trigger.source;
                let max_risk = self.skill.config.actions.iter()
                    .map(|a| &a.risk)
                    .max_by_key(|r| match r {
                        RiskLevel::Low => 0,
                        RiskLevel::Medium => 1,
                        RiskLevel::High => 2,
                    })
                    .cloned()
                    .unwrap_or(RiskLevel::Medium);

                // For monitor triggers, default to the highest risk action available
                // The planner/executor will decide what's actually needed
                if source.contains("bootstrap") {
                    RiskLevel::High
                } else {
                    max_risk
                }
            }
            ResourceEvent::Shutdown => RiskLevel::Low,
        }
    }
}
