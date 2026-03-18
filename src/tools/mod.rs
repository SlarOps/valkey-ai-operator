pub mod k8s;
pub mod valkey;

use std::sync::Arc;
use kube::Client;
use crate::agent::tool::Tool;
use crate::crd::ValkeyClusterSpec;

/// Register all tools available to the agent.
pub fn register_tools(
    client: Arc<Client>,
    cluster_name: String,
    namespace: String,
    spec_memory_limit: Option<String>,
    max_memory_scale_factor: f64,
    min_masters: u32,
    spec: Arc<ValkeyClusterSpec>,
) -> Vec<Box<dyn Tool>> {
    vec![
        // K8s — generic
        Box::new(k8s::KubectlApply::new(client.clone(), namespace.clone())),
        Box::new(k8s::PatchResources::new(client.clone(), cluster_name.clone(), namespace.clone(), spec_memory_limit, max_memory_scale_factor)),
        Box::new(k8s::RestartPod::new(client.clone(), namespace.clone())),
        Box::new(k8s::PodExec::new(client.clone(), namespace.clone())),
        Box::new(k8s::UpdateClusterStatus::new(client.clone(), cluster_name.clone(), namespace.clone())),

        // K8s — observe
        Box::new(k8s::GetPodStatus::new(client.clone(), namespace.clone())),
        Box::new(k8s::WaitForPods::new(client.clone(), cluster_name.clone(), namespace.clone())),
        Box::new(k8s::GetEvents::new(client.clone(), namespace.clone())),
        Box::new(k8s::GetPodLogs::new(client.clone(), namespace.clone())),

        // Valkey
        Box::new(valkey::ValkeyCli::new(client.clone(), cluster_name.clone(), namespace.clone())),
        Box::new(valkey::HealthCheck::new(client.clone(), cluster_name.clone(), namespace.clone(), spec.clone())),
        Box::new(valkey::ClusterMeetAll::new(client.clone(), cluster_name.clone(), namespace.clone(), spec)),
        Box::new(valkey::ClusterAddNode::new(client.clone(), cluster_name.clone(), namespace.clone())),
        Box::new(valkey::ClusterRebalance::new(client.clone(), cluster_name.clone(), namespace.clone())),
    ]
}
