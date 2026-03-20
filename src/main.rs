use krust_operator::controller;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let client = kube::Client::try_default().await?;
    controller::run(client).await;
    Ok(())
}
