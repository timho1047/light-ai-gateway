//! Binary entry point.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    light_ai_gateway::cli::run().await
}
