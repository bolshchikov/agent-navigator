use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    agent_navigator::init_tracing();
    agent_navigator::cli::run().await
}
