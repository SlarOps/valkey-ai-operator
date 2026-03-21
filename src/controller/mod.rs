pub mod status;

use crate::agent::provider::{AnthropicProvider, Provider, VertexAnthropicProvider};
use crate::agent::worker::AgentInstance;
use crate::channel::EventChannelRegistry;
use crate::crd::{AIResource, AIResourceSpec, ResourcePhase};
use crate::monitor::registry::MonitorRegistry;
use crate::pipeline::PipelineConfig;
use crate::skill::loader::load_skill;
use crate::skill::types::LoadedSkill;
use crate::tools::desired_state;
use crate::types::{
    DriftInfo, K8sState, PodInfo, ResourceEvent, ResourceInfo, StateSnapshot, StatefulSetInfo,
    TriggerInfo,
};
use anyhow::Result;
use futures::StreamExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Service};
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
        .owns(Api::<ConfigMap>::all(client.clone()), watcher::Config::default())
        .owns(Api::<Service>::all(client.clone()), watcher::Config::default())
        .owns(Api::<StatefulSet>::all(client.clone()), watcher::Config::default())
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
        // Instance exists — detect drift then check spec changes

        // Drift detection: check desired state vs live resources, notify agent if missing
        if let Some(drift_info) = detect_drift(&ctx.client, &resource, ns, name).await {
            info!("Drift detected for {}: {:?}", key, drift_info.missing_resources);
            let snapshot = build_drift_snapshot(ns, name, spec, &resource, &ctx.client).await;
            let sent = ctx.event_channels.lock().unwrap()
                .try_send(ns, name, ResourceEvent::DriftDetected(snapshot, drift_info));
            if !sent {
                warn!("Failed to send DriftDetected event for {} (channel full or missing)", key);
            }
        }

        // Check for spec changes
        let spec_changed = {
            let specs = ctx.last_seen_specs.lock().unwrap();
            specs.get(&key).map(|old| old != spec).unwrap_or(true)
        };

        if spec_changed {
            info!("Spec changed for {}, sending SpecChange event", key);
            ctx.last_seen_specs.lock().unwrap().insert(key.clone(), spec.clone());

            let snapshot = build_spec_change_snapshot(ns, name, spec, &resource, &ctx.client).await;
            let sent = ctx.event_channels.lock().unwrap()
                .try_send(ns, name, ResourceEvent::SpecChange(snapshot));
            if !sent {
                warn!("Failed to send SpecChange event for {} (channel full or missing)", key);
            }
        }
    }

    // Adaptive requeue: fast when work is needed, slow when stable
    let requeue_secs = match resource.status.as_ref().and_then(|s| s.phase.as_ref()) {
        Some(ResourcePhase::Running) => 120,      // stable — check every 2 min
        Some(ResourcePhase::Initializing) => 15,   // active work — check frequently
        _ => 30,                                    // pending/failed — moderate
    };
    Ok(Action::requeue(Duration::from_secs(requeue_secs)))
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
            .or_else(|| std::env::var("ANTHROPIC_DEFAULT_MODEL").ok())
            .unwrap_or_else(|| "claude-sonnet-4-20250514".to_string()),
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
    let snapshot = build_bootstrap_snapshot(namespace, name, spec, _resource, &state.client).await;

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
    let provider_type_owned = agent.map(|a| a.provider.clone())
        .or_else(|| std::env::var("LLM_PROVIDER").ok())
        .unwrap_or_else(|| "anthropic".to_string());
    let provider_type = provider_type_owned.as_str();

    match provider_type {
        "vertex" => {
            let region = agent.and_then(|a| a.region.clone())
                .or_else(|| std::env::var("CLOUD_ML_REGION").ok())
                .unwrap_or_else(|| "us-east5".to_string());
            let project_id = agent.and_then(|a| a.project_id.clone())
                .or_else(|| std::env::var("ANTHROPIC_VERTEX_PROJECT_ID").ok())
                .ok_or_else(|| anyhow::anyhow!("Vertex AI requires project_id (set in spec or ANTHROPIC_VERTEX_PROJECT_ID env)"))?;
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

fn get_uid(resource: &AIResource) -> String {
    resource.metadata.uid.clone().unwrap_or_default()
}

async fn build_bootstrap_snapshot(
    namespace: &str,
    name: &str,
    spec: &AIResourceSpec,
    resource: &AIResource,
    client: &Client,
) -> StateSnapshot {
    let k8s = query_k8s_state(client, namespace, name).await.unwrap_or_else(|_| {
        K8sState { pods: vec![], statefulsets: vec![] }
    });

    StateSnapshot {
        resource: ResourceInfo {
            name: name.to_string(),
            namespace: namespace.to_string(),
            uid: get_uid(resource),
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
    resource: &AIResource,
    client: &Client,
) -> StateSnapshot {
    let k8s = query_k8s_state(client, namespace, name).await.unwrap_or_else(|_| {
        K8sState { pods: vec![], statefulsets: vec![] }
    });

    StateSnapshot {
        resource: ResourceInfo {
            name: name.to_string(),
            namespace: namespace.to_string(),
            uid: get_uid(resource),
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

/// Detect drift: check desired state vs live resources.
/// Returns DriftInfo if any resources are missing, None if everything is fine.
async fn detect_drift(
    client: &Client,
    resource: &AIResource,
    namespace: &str,
    _name: &str,
) -> Option<DriftInfo> {
    let desired = desired_state::read_from_resource(resource);
    if desired.is_empty() {
        return None;
    }

    let mut missing = Vec::new();

    for (_template_name, rendered_yaml) in &desired {
        let manifest: serde_json::Value = match serde_yaml::from_str(rendered_yaml) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let kind = manifest["kind"].as_str().unwrap_or("");
        let res_name = manifest["metadata"]["name"].as_str().unwrap_or("");
        if kind.is_empty() || res_name.is_empty() {
            continue;
        }

        let exists = match kind {
            "ConfigMap" => {
                let api: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
                matches!(api.get_opt(res_name).await, Ok(Some(_)))
            }
            "Service" => {
                let api: Api<Service> = Api::namespaced(client.clone(), namespace);
                matches!(api.get_opt(res_name).await, Ok(Some(_)))
            }
            "StatefulSet" => {
                let api: Api<StatefulSet> = Api::namespaced(client.clone(), namespace);
                matches!(api.get_opt(res_name).await, Ok(Some(_)))
            }
            _ => true,
        };

        if !exists {
            missing.push(format!("{}/{}", kind, res_name));
        }
    }

    if missing.is_empty() {
        None
    } else {
        Some(DriftInfo { missing_resources: missing })
    }
}

async fn build_drift_snapshot(
    namespace: &str,
    name: &str,
    spec: &AIResourceSpec,
    resource: &AIResource,
    client: &Client,
) -> StateSnapshot {
    let k8s = query_k8s_state(client, namespace, name).await.unwrap_or_else(|_| {
        K8sState { pods: vec![], statefulsets: vec![] }
    });

    StateSnapshot {
        resource: ResourceInfo {
            name: name.to_string(),
            namespace: namespace.to_string(),
            uid: get_uid(resource),
            skill: spec.skill.clone(),
            goal: spec.goal.clone(),
            image: spec.image.clone(),
        },
        monitors: HashMap::new(),
        k8s,
        trigger: TriggerInfo {
            source: "drift".to_string(),
            reason: "Child resource missing or drifted".to_string(),
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

/// Monitor loop: periodically runs health check scripts in pods, sends events on failure
async fn monitor_loop(state: Arc<OperatorState>) {
    info!("Monitor loop started");
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Collect due monitors
        let due: Vec<(String, String, String, String, String, String, std::path::PathBuf)> = {
            let mut registry = state.monitor_registry.lock().unwrap();
            registry.due_monitors().iter_mut().map(|m| {
                m.last_run = Some(std::time::Instant::now());
                (
                    m.resource_namespace.clone(),
                    m.resource_name.clone(),
                    m.monitor_def.name.clone(),
                    m.monitor_def.script.clone(),
                    m.monitor_def.parse.clone(),
                    m.monitor_def.trigger_when.clone(),
                    m.skill_dir.clone(),
                )
            }).collect()
        };

        for (ns, name, monitor_name, script, parse_type, trigger_when, skill_dir) in &due {
            // Find a ready pod to run the monitor script on
            // Try multiple label selectors: Helm-style first, then legacy
            let pod_api: Api<Pod> = Api::namespaced(state.client.clone(), ns);
            let label_selectors = vec![
                format!("app.kubernetes.io/instance={}", name),
                format!("app={}", name),
            ];

            let mut target_pod = None;
            for label_selector in &label_selectors {
                let lp = ListParams::default().labels(label_selector);
                if let Ok(pod_list) = pod_api.list(&lp).await {
                    target_pod = pod_list.items.into_iter().find(|pod| {
                        pod.status.as_ref()
                            .and_then(|s| s.conditions.as_ref())
                            .map(|conds| conds.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
                            .unwrap_or(false)
                    });
                    if target_pod.is_some() {
                        break;
                    }
                }
            }

            let target_pod_name = match target_pod {
                Some(pod) => pod.metadata.name.unwrap_or_default(),
                None => {
                    // No ready pod — trigger monitor event
                    tracing::debug!("Monitor {}: no ready pod for {}/{}", monitor_name, ns, name);
                    let output = serde_json::json!({"exit_code": 1, "error": "no ready pod found"});
                    if evaluate_trigger(trigger_when, &output) {
                        send_monitor_event(&state, ns, name, monitor_name, &output).await;
                    }
                    continue;
                }
            };

            // Read monitor script from skill directory
            let script_path = skill_dir.join(script);
            let script_content = match tokio::fs::read_to_string(&script_path).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("Monitor {}: failed to read {}: {}", monitor_name, script_path.display(), e);
                    continue;
                }
            };

            // Run script in pod via kube attach API
            let command = vec!["bash".to_string(), "-c".to_string(), script_content];
            let ap = kube::api::AttachParams {
                stdout: true,
                stderr: true,
                stdin: false,
                tty: false,
                ..Default::default()
            };

            let (exit_code, stdout_str) = match pod_api.exec(&target_pod_name, command, &ap).await {
                Ok(mut attached) => {
                    use tokio::io::AsyncReadExt;
                    let mut stdout_buf = String::new();
                    if let Some(mut stdout) = attached.stdout() {
                        let _ = stdout.read_to_string(&mut stdout_buf).await;
                    }
                    if let Some(mut stderr) = attached.stderr() {
                        let mut buf = String::new();
                        let _ = stderr.read_to_string(&mut buf).await;
                    }
                    let code = if let Some(status_future) = attached.take_status() {
                        match status_future.await {
                            Some(status) => status.code.unwrap_or(0),
                            None => 0,
                        }
                    } else {
                        0
                    };
                    (code, stdout_buf)
                }
                Err(e) => {
                    tracing::debug!("Monitor {} failed on {}: {}", monitor_name, target_pod_name, e);
                    (1, String::new())
                }
            };

            // Parse output
            let output = crate::monitor::runner::parse_monitor_output(parse_type, &stdout_str, exit_code);

            // Store result in registry
            {
                let mut registry = state.monitor_registry.lock().unwrap();
                if let Some(instances) = registry.monitors_mut(ns, name) {
                    for m in instances {
                        if m.monitor_def.name == *monitor_name {
                            m.last_output = Some(output.clone());
                        }
                    }
                }
            }

            // Evaluate trigger condition and send event if needed
            if evaluate_trigger(trigger_when, &output) {
                info!("Monitor {} triggered for {}/{}: {:?}", monitor_name, ns, name, output);
                send_monitor_event(&state, ns, name, monitor_name, &output).await;
            }
        }
    }
}

/// Evaluate a simple trigger expression like "exit_code != 0" or "cluster_state != ok"
fn evaluate_trigger(trigger_when: &str, output: &serde_json::Value) -> bool {
    let parts: Vec<&str> = trigger_when.split_whitespace().collect();
    if parts.len() != 3 {
        return false;
    }
    let (field, op, expected) = (parts[0], parts[1], parts[2]);

    let actual = match output.get(field) {
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => return false,
    };

    match op {
        "!=" => actual != expected,
        "==" => actual == expected,
        ">" => actual.parse::<f64>().unwrap_or(0.0) > expected.parse::<f64>().unwrap_or(0.0),
        "<" => actual.parse::<f64>().unwrap_or(0.0) < expected.parse::<f64>().unwrap_or(0.0),
        _ => false,
    }
}

/// Send a MonitorTrigger event to the agent instance
async fn send_monitor_event(
    state: &OperatorState,
    ns: &str,
    name: &str,
    monitor_name: &str,
    output: &serde_json::Value,
) {
    let resources: Api<AIResource> = Api::namespaced(state.client.clone(), ns);
    let resource = match resources.get(name).await {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to get AIResource {}/{} for monitor event: {}", ns, name, e);
            return;
        }
    };

    let spec = &resource.spec;
    let uid = get_uid(&resource);
    let k8s = query_k8s_state(&state.client, ns, name).await.unwrap_or_else(|_| {
        K8sState { pods: vec![], statefulsets: vec![] }
    });

    let snapshot = StateSnapshot {
        resource: ResourceInfo {
            name: name.to_string(),
            namespace: ns.to_string(),
            uid,
            skill: spec.skill.clone(),
            goal: spec.goal.clone(),
            image: spec.image.clone(),
        },
        monitors: HashMap::new(),
        k8s,
        trigger: TriggerInfo {
            source: format!("monitor:{}", monitor_name),
            reason: format!("Monitor '{}' triggered: {}", monitor_name, output),
            timestamp: chrono::Utc::now().to_rfc3339(),
        },
    };

    let sent = state.event_channels.lock().unwrap()
        .try_send(ns, name, ResourceEvent::MonitorTrigger(snapshot));
    if !sent {
        warn!("Failed to send MonitorTrigger for {}/{} (channel full or missing)", ns, name);
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
