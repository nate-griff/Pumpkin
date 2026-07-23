mod nbt_io;
mod region;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    tracing::info!("world-converter scaffold OK");
}
