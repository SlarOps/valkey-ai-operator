pub mod k8s;
pub mod valkey;

use std::sync::Arc;
use kube::Client;
use crate::agent::tool::Tool;
use crate::crd::ValkeyClusterSpec;

/// Register all K8s and Valkey tools.
pub fn register_tools(
    client: Arc<Client>,
    cluster_name: String,
    namespace: String,
    spec_memory_limit: Option<String>,
    max_memory_scale_factor: f64,
    min_masters: u32,
    spec: Arc<ValkeyClusterSpec>,
) -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();

    tools.push(Box::new(k8s::CreateStatefulSet::new(
        client.clone(),
        cluster_name.clone(),
        namespace.clone(),
        spec.image(),
    )));
    tools.push(Box::new(k8s::CreateService::new(
        client.clone(),
        cluster_name.clone(),
        namespace.clone(),
    )));
    tools.push(Box::new(k8s::CreateConfigMap::new(
        client.clone(),
        cluster_name.clone(),
        namespace.clone(),
    )));
    tools.push(Box::new(k8s::PatchResources::new(
        client.clone(),
        cluster_name.clone(),
        namespace.clone(),
        spec_memory_limit,
        max_memory_scale_factor,
    )));
    tools.push(Box::new(k8s::ScaleStatefulSet::new(
        client.clone(),
        cluster_name.clone(),
        namespace.clone(),
        min_masters,
    )));
    tools.push(Box::new(k8s::GetPodStatus::new(
        client.clone(),
        namespace.clone(),
    )));
    tools.push(Box::new(k8s::WaitForPods::new(
        client.clone(),
        cluster_name.clone(),
        namespace.clone(),
    )));
    tools.push(Box::new(k8s::GetEvents::new(
        client.clone(),
        namespace.clone(),
    )));
    tools.push(Box::new(k8s::GetPodLogs::new(
        client.clone(),
        namespace.clone(),
    )));
    tools.push(Box::new(k8s::RestartPod::new(
        client.clone(),
        namespace.clone(),
    )));

    tools.push(Box::new(k8s::PodExec::new(
        client.clone(),
        namespace.clone(),
    )));
    tools.push(Box::new(k8s::UpdateClusterStatus::new(
        client.clone(),
        cluster_name.clone(),
        namespace.clone(),
    )));

    // Valkey tools
    tools.push(Box::new(valkey::ValkeyCli::new(
        client.clone(),
        cluster_name.clone(),
        namespace.clone(),
    )));
    tools.push(Box::new(valkey::ClusterNodes::new(
        client.clone(),
        cluster_name.clone(),
        namespace.clone(),
    )));
    tools.push(Box::new(valkey::ClusterInfo::new(
        client.clone(),
        cluster_name.clone(),
        namespace.clone(),
    )));
    tools.push(Box::new(valkey::HealthCheck::new(
        client.clone(),
        cluster_name.clone(),
        namespace.clone(),
        spec.clone(),
    )));
    tools.push(Box::new(valkey::ClusterAddNode::new(
        client.clone(),
        cluster_name.clone(),
        namespace.clone(),
    )));
    tools.push(Box::new(valkey::ClusterRebalance::new(
        client.clone(),
        cluster_name.clone(),
        namespace.clone(),
    )));
    tools.push(Box::new(valkey::ClusterMeetAll::new(
        client.clone(),
        cluster_name.clone(),
        namespace.clone(),
        spec,
    )));

    tools
}
