use tracing_subscriber::EnvFilter;
use valkey_ai_operator::controller;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("valkey_ai_operator=info".parse()?)
        )
        .init();

    tracing::info!("valkey-ai-operator starting");

    let client = kube::Client::try_default().await?;
    controller::run(client).await;

    Ok(())
}
