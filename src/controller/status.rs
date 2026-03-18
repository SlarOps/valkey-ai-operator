use crate::crd::{ClusterPhase, ValkeyCluster};
use kube::api::{Api, Patch, PatchParams};
use serde_json::json;

/// Update the phase field on a ValkeyCluster status subresource.
pub async fn update_phase(
    api: &Api<ValkeyCluster>,
    name: &str,
    phase: ClusterPhase,
) -> anyhow::Result<()> {
    let patch = json!({
        "status": {
            "phase": phase
        }
    });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

/// Update the last agent action and timestamp on a ValkeyCluster status subresource.
pub async fn update_agent_action(
    api: &Api<ValkeyCluster>,
    name: &str,
    action: &str,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    let patch = json!({
        "status": {
            "last_agent_action": action,
            "last_agent_action_time": now
        }
    });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

/// Update or insert a condition on a ValkeyCluster status subresource.
pub async fn update_condition(
    api: &Api<ValkeyCluster>,
    name: &str,
    condition_type: &str,
    status_val: &str,
) -> anyhow::Result<()> {
    // Fetch the current resource to update conditions properly.
    let cluster = api.get(name).await?;
    let mut conditions = cluster
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default();

    // Update existing or add new condition.
    if let Some(cond) = conditions
        .iter_mut()
        .find(|c| c.condition_type == condition_type)
    {
        cond.status = status_val.to_string();
    } else {
        conditions.push(crate::crd::ClusterCondition {
            condition_type: condition_type.to_string(),
            status: status_val.to_string(),
        });
    }

    let patch = json!({
        "status": {
            "conditions": conditions
        }
    });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

/// Update the ready nodes and masters count on a ValkeyCluster status subresource.
pub async fn update_ready_nodes(
    api: &Api<ValkeyCluster>,
    name: &str,
    ready: u32,
    masters: u32,
) -> anyhow::Result<()> {
    let patch = json!({
        "status": {
            "readyNodes": ready,
            "masters": masters
        }
    });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}
