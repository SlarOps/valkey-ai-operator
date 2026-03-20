pub mod status;

use crate::crd::AIResource;
use futures::StreamExt;
use kube::{
    Api, Client,
    runtime::controller::{Action, Controller},
    runtime::watcher::Config,
};
use std::sync::Arc;
use tracing::{info, error};

#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {
    #[error("Kube error: {0}")]
    KubeError(#[from] kube::Error),
    #[error("General error: {0}")]
    GeneralError(String),
}

struct Ctx {
    client: Client,
}

async fn reconcile(resource: Arc<AIResource>, ctx: Arc<Ctx>) -> Result<Action, ReconcileError> {
    let name = resource.metadata.name.as_deref().unwrap_or("unknown");
    let ns = resource.metadata.namespace.as_deref().unwrap_or("default");
    info!("Reconciling AIResource {}/{} (skill: {})", ns, name, resource.spec.skill);
    // Stub: requeue after 60s
    let _ = ctx;
    Ok(Action::requeue(std::time::Duration::from_secs(60)))
}

fn error_policy(resource: Arc<AIResource>, error: &ReconcileError, _ctx: Arc<Ctx>) -> Action {
    let name = resource.metadata.name.as_deref().unwrap_or("unknown");
    error!("Reconcile error for {}: {}", name, error);
    Action::requeue(std::time::Duration::from_secs(30))
}

pub async fn run(client: Client) {
    let resources: Api<AIResource> = Api::all(client.clone());
    let ctx = Arc::new(Ctx { client });

    info!("Starting Pilotis operator controller...");

    Controller::new(resources, Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok(o) => info!("Reconciled: {:?}", o),
                Err(e) => error!("Reconcile failed: {:?}", e),
            }
        })
        .await;
}
