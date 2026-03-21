use crate::crd::AIResource;
use kube::api::{Api, Patch, PatchParams};
use kube::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use tracing::warn;

pub const ANNOTATION_KEY: &str = "krust.io/desired-state";
const ANNOTATION_SIZE_WARN_BYTES: usize = 200_000; // warn at 200KB

/// Build an ownerReference JSON value for an AIResource.
pub fn build_owner_ref(name: &str, uid: &str) -> Value {
    json!({
        "apiVersion": "krust.io/v1",
        "kind": "AIResource",
        "name": name,
        "uid": uid,
        "controller": true,
        "blockOwnerDeletion": true
    })
}

/// Read the desired-state annotation from an AIResource.
/// Returns a map of template_name -> rendered YAML string.
pub fn read_from_resource(resource: &AIResource) -> HashMap<String, String> {
    resource
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(ANNOTATION_KEY))
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or_default()
}

/// Store a rendered template YAML into the desired-state annotation on the AIResource.
/// Uses read-modify-write to avoid clobbering other templates.
pub async fn store_rendered(
    client: &Client,
    namespace: &str,
    name: &str,
    template_name: &str,
    rendered_yaml: &str,
) -> anyhow::Result<()> {
    let api: Api<AIResource> = Api::namespaced(client.clone(), namespace);

    // Read current annotation
    let mut map: HashMap<String, String> = match api.get(name).await {
        Ok(resource) => read_from_resource(&resource),
        Err(_) => HashMap::new(),
    };

    // Merge in the new template
    map.insert(template_name.to_string(), rendered_yaml.to_string());

    // Serialize and check size
    let annotation_value = serde_json::to_string(&map)?;
    if annotation_value.len() > ANNOTATION_SIZE_WARN_BYTES {
        warn!(
            "Desired-state annotation for {}/{} is {}KB — approaching K8s annotation size limit",
            namespace, name, annotation_value.len() / 1024
        );
    }

    let patch = json!({
        "metadata": {
            "annotations": {
                ANNOTATION_KEY: annotation_value
            }
        }
    });

    api.patch(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .map_err(|e| {
            warn!("Failed to store desired state for {}/{}: {}", namespace, name, e);
            anyhow::anyhow!("Failed to store desired state: {}", e)
        })?;

    Ok(())
}
