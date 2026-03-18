use std::sync::Arc;

use kube::api::{Api, PostParams};
use kube::client::Client;
use tokio::sync::mpsc;

use super::agent::AutonomousAgent;
use super::provider::{AnthropicProvider, Provider, VertexAnthropicProvider};
use super::types::AgentConfig;
use crate::controller::status;
use crate::crd::{ClusterPhase, SecretKeyRef, ValkeyCluster};
use crate::tools::register_tools;
use crate::types::{CircuitBreaker, StateSnapshot};

const SYSTEM_PROMPT: &str = "\
You are an autonomous Kubernetes operator agent for Valkey clusters.

You receive a snapshot of desired state (CRD spec) vs actual state (K8s + Valkey).
Your job: make actual match desired using the tools available to you.

Constraints:
- ALWAYS call update_cluster_status as your FINAL action
- Set phase to Running only when cluster_state=ok and desired matches actual
- cluster_init creates a NEW cluster — never use it on a cluster that already has data
- cluster_add_node adds nodes to an EXISTING cluster — use for scaling
- Diagnose before fixing. Verify after fixing.
";

pub struct AgentWorker;

impl AgentWorker {
    pub async fn run(
        client: Client,
        mut rx: mpsc::Receiver<StateSnapshot>,
        cluster_name: String,
        namespace: String,
    ) {
        let mut circuit_breaker = CircuitBreaker::new(3);
        let cb_key = format!("{}/{}", namespace, cluster_name);

        tracing::info!(cluster = %cluster_name, "AgentWorker started");

        while let Some(snapshot) = rx.recv().await {
            // Skip if nothing needs attention
            if snapshot.trigger == "reconcile" {
                tracing::debug!(cluster = %cluster_name, "No changes detected, skipping agent call");
                continue;
            }

            tracing::info!(
                cluster = %cluster_name,
                trigger = %snapshot.trigger,
                "Processing state change"
            );

            if circuit_breaker.is_open(&cb_key) {
                tracing::warn!(cluster = %cluster_name, "Circuit breaker open, skipping");
                continue;
            }

            // Build provider
            let provider: Arc<dyn Provider> = match Self::build_provider(&client, &namespace, &snapshot.spec).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(cluster = %cluster_name, error = %e, "Failed to build provider");
                    continue;
                }
            };

            // Build tools
            let spec_arc = Arc::new(snapshot.spec.clone());
            let tools = register_tools(
                Arc::new(client.clone()),
                cluster_name.clone(),
                namespace.clone(),
                snapshot.spec.resources.limits.as_ref().and_then(|l| l.memory.clone()),
                snapshot.spec.agent.max_memory_scale_factor,
                snapshot.spec.masters,
                spec_arc,
            );

            let config = AgentConfig {
                model: "claude-haiku-4-5@20251001".into(),
                max_iterations: 30,
                temperature: 0.0,
            };

            // Agent message = snapshot context
            let user_message = snapshot.to_agent_message();
            tracing::info!(cluster = %cluster_name, "Agent message:\n{}", user_message);

            let mut agent = AutonomousAgent::new(provider, tools, config);

            match agent.run(&user_message, SYSTEM_PROMPT).await {
                Ok(result) => {
                    tracing::info!(
                        cluster = %cluster_name,
                        actions = result.actions_taken.len(),
                        "Agent completed"
                    );
                    circuit_breaker.record_success(&cb_key);

                    // Update agent action metadata
                    let cluster_api: Api<ValkeyCluster> = Api::namespaced(client.clone(), &namespace);
                    let summary = if result.actions_taken.is_empty() {
                        "No actions needed".into()
                    } else {
                        format!("{} actions", result.actions_taken.len())
                    };
                    let _ = status::update_agent_action(&cluster_api, &cluster_name, &summary).await;
                    let _ = status::update_condition(&cluster_api, &cluster_name, "AgentHealthy", "True").await;

                    Self::emit_event(&client, &namespace, &cluster_name, "Normal", "AgentCompleted", &summary).await;
                }
                Err(e) => {
                    tracing::error!(cluster = %cluster_name, error = %e, "Agent failed");
                    let count = circuit_breaker.record_failure(&cb_key);

                    let cluster_api: Api<ValkeyCluster> = Api::namespaced(client.clone(), &namespace);
                    let _ = status::update_condition(&cluster_api, &cluster_name, "AgentHealthy", "False").await;

                    if circuit_breaker.is_open(&cb_key) {
                        let _ = status::update_phase(&cluster_api, &cluster_name, ClusterPhase::Failed).await;
                        Self::emit_event(&client, &namespace, &cluster_name, "Warning", "CircuitBreakerOpen",
                            &format!("{} consecutive failures", count)).await;
                    }
                }
            }
        }
    }

    async fn build_provider(
        client: &Client,
        namespace: &str,
        spec: &crate::crd::ValkeyClusterSpec,
    ) -> anyhow::Result<Arc<dyn Provider>> {
        match spec.agent.provider.as_str() {
            "vertex" => {
                let region = spec.agent.region.as_deref().unwrap_or("us-central1");
                let project_id = spec.agent.project_id.as_deref()
                    .ok_or_else(|| anyhow::anyhow!("Vertex requires agent.project_id"))?;
                Ok(Arc::new(VertexAnthropicProvider::new(region, project_id)))
            }
            _ => {
                let secret_ref = spec.agent.api_key_secret_ref.as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Anthropic requires agent.api_key_secret_ref"))?;
                let secrets: Api<k8s_openapi::api::core::v1::Secret> = Api::namespaced(client.clone(), namespace);
                let secret = secrets.get(&secret_ref.name).await?;
                let data = secret.data.ok_or_else(|| anyhow::anyhow!("Secret has no data"))?;
                let key_bytes = data.get(&secret_ref.key)
                    .ok_or_else(|| anyhow::anyhow!("Key not found in secret"))?;
                let api_key = String::from_utf8(key_bytes.0.clone())?.trim().to_string();
                Ok(Arc::new(AnthropicProvider::new(api_key)))
            }
        }
    }

    async fn emit_event(client: &Client, namespace: &str, cluster_name: &str, event_type: &str, reason: &str, message: &str) {
        let events: Api<k8s_openapi::api::core::v1::Event> = Api::namespaced(client.clone(), namespace);
        let event = k8s_openapi::api::core::v1::Event {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                generate_name: Some(format!("{}-", cluster_name)),
                namespace: Some(namespace.to_string()),
                ..Default::default()
            },
            involved_object: k8s_openapi::api::core::v1::ObjectReference {
                api_version: Some("valkey.krust.io/v1alpha1".into()),
                kind: Some("ValkeyCluster".into()),
                name: Some(cluster_name.to_string()),
                namespace: Some(namespace.to_string()),
                ..Default::default()
            },
            reason: Some(reason.to_string()),
            message: Some(message.to_string()),
            type_: Some(event_type.to_string()),
            first_timestamp: Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(chrono::Utc::now())),
            last_timestamp: Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(chrono::Utc::now())),
            ..Default::default()
        };
        let _ = events.create(&PostParams::default(), &event).await;
    }
}
