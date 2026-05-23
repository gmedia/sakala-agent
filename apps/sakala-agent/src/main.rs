mod app;
mod config;
mod telemetry;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = config::load()?;
    telemetry::init(&config.log_level)?;
    app::run(config).await
}
