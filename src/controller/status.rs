use crate::crd::{AIResource, AIResourceStatus, ResourcePhase, ResourceCondition};
use kube::{Api, Client};
use kube::api::Patch;
use anyhow::Result;
use serde_json::json;

pub async fn update_phase(
    client: &Client,
    name: &str,
    namespace: &str,
    phase: ResourcePhase,
    message: Option<&str>,
) -> Result<()> {
    let api: Api<AIResource> = Api::namespaced(client.clone(), namespace);
    let status = json!({
        "status": {
            "phase": phase,
            "message": message,
        }
    });
    api.patch_status(name, &Default::default(), &Patch::Merge(&status)).await?;
    Ok(())
}

pub async fn update_agent_action(
    client: &Client,
    name: &str,
    namespace: &str,
    action: &str,
) -> Result<()> {
    let api: Api<AIResource> = Api::namespaced(client.clone(), namespace);
    let now = chrono::Utc::now().to_rfc3339();
    let status = json!({
        "status": {
            "last_agent_action": action,
            "last_agent_action_time": now,
        }
    });
    api.patch_status(name, &Default::default(), &Patch::Merge(&status)).await?;
    Ok(())
}

pub async fn update_condition(
    client: &Client,
    name: &str,
    namespace: &str,
    condition: ResourceCondition,
) -> Result<()> {
    let api: Api<AIResource> = Api::namespaced(client.clone(), namespace);
    let status = json!({
        "status": {
            "conditions": [condition],
        }
    });
    api.patch_status(name, &Default::default(), &Patch::Merge(&status)).await?;
    Ok(())
}

// Suppress unused import warning — AIResourceStatus kept for future use
const _: fn() = || {
    let _: Option<AIResourceStatus> = None;
};
