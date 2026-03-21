pub mod status;

use crate::agent::provider::{AnthropicProvider, Provider, VertexAnthropicProvider};
use crate::agent::worker::AgentInstance;
use crate::channel::EventChannelRegistry;
use crate::crd::{AIResource, AIResourceSpec, ResourcePhase};
use crate::monitor::registry::MonitorRegistry;
use crate::pipeline::PipelineConfig;
use crate::skill::loader::load_skill;
use crate::skill::types::LoadedSkill;
use crate::types::{
    K8sState, PodInfo, ResourceEvent, ResourceInfo, StateSnapshot, StatefulSetInfo, TriggerInfo,
};
use anyhow::Result;
use futures::StreamExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::Pod;
use kube::api::ListParams;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher;
use kube::{Api, Client};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {
    #[error("Kube error: {0}")]
    KubeError(#[from] kube::Error),
    #[error("General error: {0}")]
    GeneralError(String),
}

struct OperatorState {
    client: Client,
    skills_dir: PathBuf,
    monitor_registry: Arc<Mutex<MonitorRegistry>>,
    event_channels: Arc<Mutex<EventChannelRegistry>>,
    agent_handles: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
    loaded_skills: Arc<Mutex<HashMap<String, Arc<LoadedSkill>>>>,
    last_seen_specs: Arc<Mutex<HashMap<String, AIResourceSpec>>>,
}

pub async fn run(client: Client) {
    let skills_dir = PathBuf::from(
        std::env::var("SKILLS_DIR").unwrap_or_else(|_| "/skills".to_string()),
    );

    let monitor_registry = Arc::new(Mutex::new(MonitorRegistry::new()));
    let event_channels = Arc::new(Mutex::new(EventChannelRegistry::new()));

    let state = Arc::new(OperatorState {
        client: client.clone(),
        skills_dir,
        monitor_registry: monitor_registry.clone(),
        event_channels: event_channels.clone(),
        agent_handles: Arc::new(Mutex::new(HashMap::new())),
        loaded_skills: Arc::new(Mutex::new(HashMap::new())),
        last_seen_specs: Arc::new(Mutex::new(HashMap::new())),
    });

    // Spawn monitor loop
    let monitor_state = state.clone();
    tokio::spawn(async move {
        monitor_loop(monitor_state).await;
    });

    let resources: Api<AIResource> = Api::all(client.clone());

    info!("Starting Krust operator controller...");

    Controller::new(resources, watcher::Config::default())
        .run(
            |resource, ctx| reconcile(resource, ctx),
            |resource, error, ctx| error_policy(resource, error, ctx),
            state,
        )
        .for_each(|res| async move {
            match res {
                Ok(o) => info!("Reconciled: {:?}", o),
                Err(e) => error!("Reconcile failed: {:?}", e),
            }
        })
        .await;
}

async fn reconcile(
    resource: Arc<AIResource>,
    ctx: Arc<OperatorState>,
) -> Result<Action, ReconcileError> {
    let name = resource.metadata.name.as_deref().unwrap_or("unknown");
    let ns = resource.metadata.namespace.as_deref().unwrap_or("default");
    let key = format!("{}/{}", ns, name);
    let spec = &resource.spec;

    info!("Reconciling AIResource {}/{} (skill: {})", ns, name, spec.skill);

    // Check if agent instance already exists
    let has_instance = ctx.event_channels.lock().unwrap().has(ns, name);

    if !has_instance {
        // New resource: load skill, register monitors, spawn agent
        match setup_agent_instance(&ctx, ns, name, spec, &resource).await {
            Ok(_) => info!("Agent instance created for {}", key),
            Err(e) => {
                error!("Failed to setup agent instance for {}: {}", key, e);
                let _ = status::update_phase(
                    &ctx.client, name, ns,
                    ResourcePhase::Failed,
                    Some(&format!("Setup failed: {}", e)),
                ).await;
                return Ok(Action::requeue(Duration::from_secs(10)));
            }
        }
        // Store initial spec
        ctx.last_seen_specs.lock().unwrap().insert(key.clone(), spec.clone());
    } else {
        // Instance exists — check for spec changes
        let spec_changed = {
            let specs = ctx.last_seen_specs.lock().unwrap();
            specs.get(&key).map(|old| old != spec).unwrap_or(true)
        };

        if spec_changed {
            info!("Spec changed for {}, sending SpecChange event", key);
            ctx.last_seen_specs.lock().unwrap().insert(key.clone(), spec.clone());

            let snapshot = build_spec_change_snapshot(ns, name, spec, &ctx.client).await;
            let sent = ctx.event_channels.lock().unwrap()
                .try_send(ns, name, ResourceEvent::SpecChange(snapshot));
            if !sent {
                warn!("Failed to send SpecChange event for {} (channel full or missing)", key);
            }
        }
    }

    Ok(Action::requeue(Duration::from_secs(10)))
}

fn error_policy(
    resource: Arc<AIResource>,
    error: &ReconcileError,
    _ctx: Arc<OperatorState>,
) -> Action {
    let name = resource.metadata.name.as_deref().unwrap_or("unknown");
    error!("Reconcile error for {}: {}", name, error);
    Action::requeue(Duration::from_secs(30))
}

async fn setup_agent_instance(
    state: &OperatorState,
    namespace: &str,
    name: &str,
    spec: &AIResourceSpec,
    _resource: &AIResource,
) -> Result<()> {
    // Load skill
    let skill = load_skill(&state.skills_dir, &spec.skill)
        .map_err(|e| anyhow::anyhow!("Failed to load skill '{}': {}", spec.skill, e))?;
    let skill = Arc::new(skill);

    // Store loaded skill
    {
        let key = format!("{}/{}", namespace, name);
        state.loaded_skills.lock().unwrap().insert(key, skill.clone());
    }

    // Register monitors
    {
        let mut registry = state.monitor_registry.lock().unwrap();
        registry.register(namespace, name, &skill);
    }

    // Create event channel
    let rx = {
        let mut channels = state.event_channels.lock().unwrap();
        channels.register(namespace, name)
    };

    // Build provider
    let provider = build_provider(spec, &state.client).await?;

    // Build pipeline config
    let agent_spec = spec.agent.as_ref();
    let config = PipelineConfig {
        pipeline_timeout_secs: parse_duration_secs(
            agent_spec.map(|a| a.pipeline_timeout.as_str()).unwrap_or("300s")
        ),
        agent_timeout_secs: 300,
        llm_call_timeout_secs: parse_duration_secs(
            agent_spec.map(|a| a.llm_call_timeout.as_str()).unwrap_or("60s")
        ),
        max_iterations: agent_spec.map(|a| a.max_iterations).unwrap_or(30),
        model: agent_spec
            .and_then(|a| a.model.clone())
            .unwrap_or_else(|| "claude-haiku-4-5-20251001".to_string()),
    };

    // Spawn agent instance
    let instance = AgentInstance::new(
        name.to_string(),
        namespace.to_string(),
        skill,
        provider,
        state.client.clone(),
        config,
        state.monitor_registry.clone(),
        spec.guardrails.clone(),
    );

    // Build bootstrap snapshot before spawning (needs await)
    let snapshot = build_bootstrap_snapshot(namespace, name, spec, &state.client).await;

    let handle = tokio::spawn(async move {
        instance.run(rx).await;
    });

    // Store handle
    {
        let key = format!("{}/{}", namespace, name);
        state.agent_handles.lock().unwrap().insert(key, handle);
    }

    // Update phase to Pending
    let _ = status::update_phase(&state.client, name, namespace, ResourcePhase::Pending, None).await;

    // Send bootstrap event — use try_send (non-blocking) to avoid holding lock across await
    let sent = state.event_channels.lock().unwrap()
        .try_send(namespace, name, ResourceEvent::Bootstrap(snapshot));
    if !sent {
        warn!("Failed to send bootstrap event for {}/{} (channel full or missing)", namespace, name);
    }

    info!("Agent instance setup complete for {}/{}", namespace, name);
    Ok(())
}

async fn build_provider(spec: &AIResourceSpec, client: &Client) -> Result<Arc<dyn Provider>> {
    let agent = spec.agent.as_ref();
    let provider_type = agent.map(|a| a.provider.as_str()).unwrap_or("anthropic");

    match provider_type {
        "vertex" => {
            let region = agent.and_then(|a| a.region.clone()).unwrap_or_else(|| "us-east5".to_string());
            let project_id = agent.and_then(|a| a.project_id.clone())
                .ok_or_else(|| anyhow::anyhow!("Vertex AI requires project_id"))?;
            Ok(Arc::new(VertexAnthropicProvider::new(&region, &project_id)))
        }
        _ => {
            let api_key = if let Some(secret_ref) = agent.and_then(|a| a.api_key_secret_ref.as_ref()) {
                // Read from K8s secret
                let ns = "default"; // TODO: use resource namespace
                let secrets: Api<k8s_openapi::api::core::v1::Secret> = Api::namespaced(client.clone(), ns);
                match secrets.get(&secret_ref.name).await {
                    Ok(secret) => {
                        secret.data.as_ref()
                            .and_then(|d| d.get(&secret_ref.key))
                            .map(|v| String::from_utf8_lossy(&v.0).to_string())
                            .unwrap_or_default()
                    }
                    Err(e) => {
                        warn!("Failed to read secret {}: {}", secret_ref.name, e);
                        std::env::var("ANTHROPIC_API_KEY").unwrap_or_default()
                    }
                }
            } else {
                std::env::var("ANTHROPIC_API_KEY").unwrap_or_default()
            };
            Ok(Arc::new(AnthropicProvider::new(api_key)))
        }
    }
}

async fn build_bootstrap_snapshot(
    namespace: &str,
    name: &str,
    spec: &AIResourceSpec,
    client: &Client,
) -> StateSnapshot {
    // Query current K8s state
    let k8s = query_k8s_state(client, namespace, name).await.unwrap_or_else(|_| {
        K8sState { pods: vec![], statefulsets: vec![] }
    });

    StateSnapshot {
        resource: ResourceInfo {
            name: name.to_string(),
            namespace: namespace.to_string(),
            skill: spec.skill.clone(),
            goal: spec.goal.clone(),
            image: spec.image.clone(),
        },
        monitors: HashMap::new(),
        k8s,
        trigger: TriggerInfo {
            source: "bootstrap".to_string(),
            reason: "AIResource created".to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        },
    }
}

async fn build_spec_change_snapshot(
    namespace: &str,
    name: &str,
    spec: &AIResourceSpec,
    client: &Client,
) -> StateSnapshot {
    let k8s = query_k8s_state(client, namespace, name).await.unwrap_or_else(|_| {
        K8sState { pods: vec![], statefulsets: vec![] }
    });

    StateSnapshot {
        resource: ResourceInfo {
            name: name.to_string(),
            namespace: namespace.to_string(),
            skill: spec.skill.clone(),
            goal: spec.goal.clone(),
            image: spec.image.clone(),
        },
        monitors: HashMap::new(),
        k8s,
        trigger: TriggerInfo {
            source: "spec_change".to_string(),
            reason: "AIResource spec updated".to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        },
    }
}

async fn query_k8s_state(client: &Client, namespace: &str, name: &str) -> Result<K8sState> {
    let pods_api: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let sts_api: Api<StatefulSet> = Api::namespaced(client.clone(), namespace);

    let label = format!("app={}", name);
    let lp = ListParams::default().labels(&label);

    let pods = match pods_api.list(&lp).await {
        Ok(list) => list.items,
        Err(_) => vec![],
    };
    let pod_infos: Vec<PodInfo> = pods.iter().map(|p| {
        let status = p.status.as_ref();
        PodInfo {
            name: p.metadata.name.clone().unwrap_or_default(),
            phase: status.and_then(|s| s.phase.clone()).unwrap_or_else(|| "Unknown".to_string()),
            ready: status.and_then(|s| s.conditions.as_ref())
                .map(|c| c.iter().any(|cond| cond.type_ == "Ready" && cond.status == "True"))
                .unwrap_or(false),
            restarts: status.and_then(|s| s.container_statuses.as_ref())
                .and_then(|cs| cs.first())
                .map(|c| c.restart_count as u32)
                .unwrap_or(0),
            ip: status.and_then(|s| s.pod_ip.clone()),
        }
    }).collect();

    let stss = match sts_api.list(&lp).await {
        Ok(list) => list.items,
        Err(_) => vec![],
    };
    let sts_infos: Vec<StatefulSetInfo> = stss.iter().map(|s| {
        let spec = s.spec.as_ref();
        let status = s.status.as_ref();
        StatefulSetInfo {
            name: s.metadata.name.clone().unwrap_or_default(),
            replicas: spec.map(|s| s.replicas.unwrap_or(0) as u32).unwrap_or(0),
            ready_replicas: status.map(|s| s.ready_replicas.unwrap_or(0) as u32).unwrap_or(0),
            memory_limit: spec
                .and_then(|s| s.template.spec.as_ref())
                .and_then(|ps| ps.containers.first())
                .and_then(|c| c.resources.as_ref())
                .and_then(|r| r.limits.as_ref())
                .and_then(|l| l.get("memory"))
                .map(|m| m.0.clone()),
            cpu_limit: spec
                .and_then(|s| s.template.spec.as_ref())
                .and_then(|ps| ps.containers.first())
                .and_then(|c| c.resources.as_ref())
                .and_then(|r| r.limits.as_ref())
                .and_then(|l| l.get("cpu"))
                .map(|c| c.0.clone()),
        }
    }).collect();

    Ok(K8sState {
        pods: pod_infos,
        statefulsets: sts_infos,
    })
}

/// Monitor loop: runs every second, checks due monitors, sends events
async fn monitor_loop(state: Arc<OperatorState>) {
    info!("Monitor loop started");
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Get due monitors
        let due: Vec<(String, String, String, String, String)> = {
            let mut registry = state.monitor_registry.lock().unwrap();
            registry.due_monitors().iter_mut().map(|m| {
                // Mark as run
                m.last_run = Some(std::time::Instant::now());
                (
                    m.resource_namespace.clone(),
                    m.resource_name.clone(),
                    m.monitor_def.name.clone(),
                    m.monitor_def.script.clone(),
                    m.monitor_def.parse.clone(),
                )
            }).collect()
        };

        // For now, just log due monitors
        // Actual pod_exec will be implemented when deployed in-cluster
        for (ns, name, monitor_name, _script, _parse) in &due {
            tracing::trace!("Monitor due: {}/{} — {}", ns, name, monitor_name);
        }
    }
}

fn parse_duration_secs(s: &str) -> u64 {
    let s = s.trim();
    if let Some(secs) = s.strip_suffix('s') {
        secs.parse().unwrap_or(300)
    } else if let Some(mins) = s.strip_suffix('m') {
        mins.parse::<u64>().unwrap_or(5) * 60
    } else {
        s.parse().unwrap_or(300)
    }
}
