use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    sunmao::run(sunmao::Cli::parse()).await
}
