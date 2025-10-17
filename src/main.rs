#[tokio::main]
async fn main() {
    if let Err(err) = gwu_hpc_mcp::async_main().await {
        eprintln!("Error: {}", err);
        std::process::exit(1);
    }
}
