use anyhow::Result;
use cccc_core::HomeLayout;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match (args.next().as_deref(), args.next()) {
        (None, None) => cccc_mcp::run_stdio(HomeLayout::resolve()?).await,
        (Some("--gateway"), None) => cccc_mcp::run_stdio_gateway(HomeLayout::resolve()?).await,
        (Some("--help" | "-h"), None) => {
            println!(
                "Usage: cccc-mcp [--gateway]\n--gateway: route trusted ChatGPT calls by their bound conversation"
            );
            Ok(())
        }
        _ => anyhow::bail!("Usage: cccc-mcp [--gateway]"),
    }
}
