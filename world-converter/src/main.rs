mod convert;
mod level_dat;
mod nbt_io;
mod palette;
mod region;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let mut args = std::env::args().skip(1);
    let (Some(source), Some(dest)) = (args.next(), args.next()) else {
        tracing::error!("usage: world-converter <source-world-dir> <dest-world-dir>");
        std::process::exit(1);
    };

    let source = std::path::Path::new(&source);
    let dest = std::path::Path::new(&dest);

    let stats = match convert::convert_world(source, dest).await {
        Ok(stats) => stats,
        Err(err) => {
            tracing::error!("conversion failed: {err}");
            std::process::exit(1);
        }
    };

    tracing::info!(
        "done: {} chunk(s) converted, {} skipped, {} entity chunk(s) converted, {} skipped",
        stats.chunks_converted,
        stats.chunks_skipped,
        stats.entity_chunks_converted,
        stats.entity_chunks_skipped,
    );

    if !stats.unresolved_names.is_empty() {
        tracing::warn!(
            "{} unknown block/biome name(s) were substituted with a placeholder:",
            stats.unresolved_names.len()
        );
        for name in &stats.unresolved_names {
            tracing::warn!("  {name}");
        }
    }

    match level_dat::convert_level_dat(source, dest) {
        Ok(()) => tracing::info!("level.dat: seed, spawn, difficulty, and game rules carried over"),
        Err(err) => tracing::error!("level.dat conversion failed: {err}"),
    }
}
