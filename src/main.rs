//! Assistant Gateway — runs `sh` commands on the device host over RPC.
//!
//! Export its `doover_config.json` config schema without connecting to an
//! agent:
//!
//!   assistant-gateway export doover_config.json --app-name assistant_gateway

use assistant_gateway::app::AssistantGatewayApp;
use doover::error::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    doover::run::<AssistantGatewayApp>().await
}
