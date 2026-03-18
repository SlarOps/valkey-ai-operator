pub mod status;

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams};
use kube::client::Client;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::ResourceExt;
use tokio::sync::{mpsc, Mutex};

use crate::agent::worker::AgentWorker;
use crate::crd::ValkeyCluster;
use crate::types::{PodInfo, StateSnapshot};

struct Ctx {
    client: Client,
    workers: Mutex<HashMap<String, mpsc::Sender<StateSnapshot>>>,
}

pub async fn run(client: Client) {
    let api: Api<ValkeyCluster> = Api::all(client.clone());
    let ctx = Arc::new(Ctx {
        client: client.clone(),
        workers: Mutex::new(HashMap::new()),
    });

    tracing::info!("Starting ValkeyCluster controller");

    Controller::new(api, Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|result| async move {
            if let Err(e) = result {
                tracing::error!(error = %e, "Reconcile failed");
            }
        })
        .await;
}

async fn reconcile(
    cluster: Arc<ValkeyCluster>,
    ctx: Arc<Ctx>,
) -> Result<Action, ReconcileError> {
    let name = cluster.name_any();
    let namespace = cluster.namespace().unwrap_or_else(|| "default".into());
    let spec = &cluster.spec;

    // Build state snapshot from actual K8s resources
    let snapshot = build_snapshot(&ctx.client, &name, &namespace, spec).await?;

    tracing::info!(
        cluster = %name,
        trigger = %snapshot.trigger,
        sts_exists = snapshot.sts_exists,
        pods = format!("{}/{}", snapshot.pods_ready, snapshot.pods_total),
        "Reconciling"
    );

    // Send snapshot to agent worker
    let key = format!("{}/{}", namespace, name);
    let mut workers = ctx.workers.lock().await;

    let tx = workers.entry(key.clone()).or_insert_with(|| {
        let (tx, rx) = mpsc::channel::<StateSnapshot>(4);
        let client = ctx.client.clone();
        let cluster_name = name.clone();
        let ns = namespace.clone();
        tokio::spawn(async move {
            AgentWorker::run(client, rx, cluster_name, ns).await;
        });
        tx
    });

    if let Err(e) = tx.try_send(snapshot) {
        tracing::debug!(cluster = %name, "Agent busy, skipping: {}", e);
    }

    Ok(Action::requeue(std::time::Duration::from_secs(60)))
}

async fn build_snapshot(
    client: &Client,
    name: &str,
    namespace: &str,
    spec: &crate::crd::ValkeyClusterSpec,
) -> Result<StateSnapshot, ReconcileError> {
    // Check StatefulSet
    let sts_api: Api<StatefulSet> = Api::namespaced(client.clone(), namespace);
    let (sts_exists, sts_replicas, sts_memory_limit, sts_cpu_limit) = match sts_api.get(name).await {
        Ok(sts) => {
            let replicas = sts.spec.as_ref().and_then(|s| s.replicas).map(|r| r as u32);
            let (mem, cpu) = sts.spec.as_ref()
                .and_then(|s| s.template.spec.as_ref())
                .and_then(|ps| ps.containers.first())
                .and_then(|c| c.resources.as_ref())
                .and_then(|r| r.limits.as_ref())
                .map(|limits| {
                    let mem = limits.get("memory").map(|q| q.0.clone());
                    let cpu = limits.get("cpu").map(|q| q.0.clone());
                    (mem, cpu)
                })
                .unwrap_or((None, None));
            (true, replicas, mem, cpu)
        }
        Err(_) => (false, None, None, None),
    };

    // List pods
    let pod_api: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let lp = ListParams::default().labels(&format!("app={}", name));
    let pod_list = pod_api.list(&lp).await.map_err(|e| ReconcileError(e.to_string()))?;

    let mut pods = Vec::new();
    let mut pods_ready = 0u32;
    for pod in &pod_list.items {
        let pod_name = pod.name_any();
        let status = pod.status.as_ref();
        let phase = status.and_then(|s| s.phase.as_deref()).unwrap_or("Unknown").to_string();
        let ready = status
            .and_then(|s| s.conditions.as_ref())
            .map(|conds| conds.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
            .unwrap_or(false);
        let restart_count: i32 = status
            .and_then(|s| s.container_statuses.as_ref())
            .map(|cs| cs.iter().map(|c| c.restart_count).sum())
            .unwrap_or(0);

        if ready { pods_ready += 1; }
        pods.push(PodInfo { name: pod_name, phase, ready, restart_count });
    }

    // Query Valkey cluster state via exec on pod-0 (if pods exist)
    let (valkey_cluster_state, valkey_cluster_slots_ok, valkey_cluster_known_nodes, valkey_cluster_size) =
        if pods_ready > 0 {
            get_valkey_state(client, name, namespace).await
        } else {
            (None, None, None, None)
        };

    // Determine trigger
    // Determine trigger — detect what changed
    let mut triggers = Vec::new();
    if !sts_exists {
        triggers.push("new_cluster".to_string());
    } else {
        if sts_replicas != Some(spec.total_pods()) {
            triggers.push(format!("replicas_mismatch: sts={} desired={}", sts_replicas.unwrap_or(0), spec.total_pods()));
        }
        // Resource mismatch
        if let Some(ref spec_limits) = spec.resources.limits {
            if let (Some(ref spec_mem), Some(ref sts_mem)) = (&spec_limits.memory, &sts_memory_limit) {
                if spec_mem != sts_mem {
                    triggers.push(format!("memory_mismatch: sts={} spec={}", sts_mem, spec_mem));
                }
            }
            if let (Some(ref spec_cpu), Some(ref sts_cpu)) = (&spec_limits.cpu, &sts_cpu_limit) {
                if spec_cpu != sts_cpu {
                    triggers.push(format!("cpu_mismatch: sts={} spec={}", sts_cpu, spec_cpu));
                }
            }
        }
        if pods_ready < pod_list.items.len() as u32 {
            triggers.push(format!("pods_not_ready: {}/{}", pods_ready, pod_list.items.len()));
        }
        // Valkey cluster issues
        if let Some(ref state) = valkey_cluster_state {
            if state != "ok" {
                triggers.push(format!("valkey_state: {}", state));
            }
        }
        if let Some(masters) = valkey_cluster_size {
            if masters != spec.masters {
                triggers.push(format!("masters_mismatch: actual={} desired={}", masters, spec.masters));
            }
        }
    }
    let trigger = if triggers.is_empty() { "reconcile".to_string() } else { triggers.join(", ") };

    Ok(StateSnapshot {
        cluster_name: name.to_string(),
        namespace: namespace.to_string(),
        spec: spec.clone(),
        sts_exists,
        sts_replicas,
        sts_memory_limit,
        sts_cpu_limit,
        pods_ready,
        pods_total: pod_list.items.len() as u32,
        pods,
        valkey_cluster_state,
        valkey_cluster_slots_ok,
        valkey_cluster_known_nodes,
        valkey_cluster_size,
        trigger,
    })
}

/// Query Valkey cluster state by running `valkey-cli CLUSTER INFO` on pod-0 via kube attach API.
async fn get_valkey_state(
    client: &Client,
    name: &str,
    namespace: &str,
) -> (Option<String>, Option<u32>, Option<u32>, Option<u32>) {
    let pod_name = format!("{}-0", name);
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let ap = kube::api::AttachParams {
        container: Some("valkey".to_string()),
        stdout: true, stderr: true, stdin: false, tty: false,
        ..Default::default()
    };

    let cmd = vec!["valkey-cli".to_string(), "CLUSTER".to_string(), "INFO".to_string()];
    match pods.exec(&pod_name, cmd, &ap).await {
        Ok(mut attached) => {
            use tokio::io::AsyncReadExt;
            let mut stdout = String::new();
            if let Some(mut out) = attached.stdout() {
                let _ = out.read_to_string(&mut stdout).await;
            }
            let _ = attached.take_status();

            let mut state = None;
            let mut slots_ok = None;
            let mut known_nodes = None;
            let mut size = None;
            for line in stdout.lines() {
                let line = line.trim();
                if let Some((k, v)) = line.split_once(':') {
                    match k.trim() {
                        "cluster_state" => state = Some(v.trim().to_string()),
                        "cluster_slots_ok" => slots_ok = v.trim().parse().ok(),
                        "cluster_known_nodes" => known_nodes = v.trim().parse().ok(),
                        "cluster_size" => size = v.trim().parse().ok(),
                        _ => {}
                    }
                }
            }
            (state, slots_ok, known_nodes, size)
        }
        Err(e) => {
            tracing::debug!(pod = %pod_name, error = %e, "Cannot reach Valkey on pod-0");
            (None, None, None, None)
        }
    }
}

fn error_policy(_cluster: Arc<ValkeyCluster>, _error: &ReconcileError, _ctx: Arc<Ctx>) -> Action {
    Action::requeue(std::time::Duration::from_secs(10))
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ReconcileError(pub String);

impl From<kube::Error> for ReconcileError {
    fn from(e: kube::Error) -> Self { ReconcileError(e.to_string()) }
}

impl From<anyhow::Error> for ReconcileError {
    fn from(e: anyhow::Error) -> Self { ReconcileError(e.to_string()) }
}
