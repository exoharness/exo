#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    exoharness::run_firecracker_bridge().await?;
    Ok(())
}
