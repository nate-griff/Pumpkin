#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    tracing::info!("world-converter scaffold OK");
}
