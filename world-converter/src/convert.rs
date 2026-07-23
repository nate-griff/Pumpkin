//! Drives the whole conversion: walks a Paper world's dimension folders,
//! translates each chunk's block/biome palettes, and writes a Pumpkin-native
//! world out the other side using Pumpkin's own region-file writer
//! (`AnvilChunkFile`/`ChunkFileManager`) - the same code the live server
//! uses to save chunks, so the output is guaranteed to be something Pumpkin
//! can read back.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pumpkin_config::chunk::AnvilChunkConfig;
use pumpkin_util::math::vector2::Vector2;
use pumpkin_world::chunk::format::anvil::{AnvilChunkFile, SingleChunkDataSerializer};
use pumpkin_world::chunk::io::file_manager::ChunkFileManager;
use pumpkin_world::chunk::io::{Dirtiable, FileIO};
use pumpkin_world::chunk::{ChunkData, ChunkEntityData};
use pumpkin_world::level::LevelFolder;

use crate::{nbt_io, palette, region};

#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed writing region data: {0}")]
    Write(String),
}

/// Running totals across the whole conversion, reported once at the end
/// instead of logging every occurrence (a world can have thousands of
/// chunks referencing the same renamed/unknown block).
#[derive(Default)]
pub struct Stats {
    pub chunks_converted: usize,
    pub chunks_skipped: usize,
    pub entity_chunks_converted: usize,
    pub entity_chunks_skipped: usize,
    pub unresolved_names: BTreeSet<String>,
}

/// One Paper dimension folder mapped onto its Pumpkin output folder.
struct DimensionMapping {
    /// e.g. `<source>/dimensions/minecraft/overworld`.
    source_dim_dir: PathBuf,
    /// e.g. the world root itself, or `<dest>/DIM-1` / `<dest>/DIM1`.
    dest_dim_dir: PathBuf,
    label: &'static str,
}

/// Converts every built-in dimension (overworld, nether, end) found under
/// `source_root` into a Pumpkin-native world at `dest_root`. Dimensions with
/// no source data present are skipped rather than treated as an error, since
/// not every world has visited the nether or end.
pub async fn convert_world(source_root: &Path, dest_root: &Path) -> Result<Stats, ConvertError> {
    let mappings = [
        DimensionMapping {
            source_dim_dir: source_root.join("dimensions/minecraft/overworld"),
            dest_dim_dir: dest_root.to_path_buf(),
            label: "overworld",
        },
        DimensionMapping {
            source_dim_dir: source_root.join("dimensions/minecraft/the_nether"),
            dest_dim_dir: dest_root.join("DIM-1"),
            label: "the_nether",
        },
        DimensionMapping {
            source_dim_dir: source_root.join("dimensions/minecraft/the_end"),
            dest_dim_dir: dest_root.join("DIM1"),
            label: "the_end",
        },
    ];

    let mut stats = Stats::default();

    for mapping in &mappings {
        if !mapping.source_dim_dir.exists() {
            tracing::info!("Skipping {} - no source data found", mapping.label);
            continue;
        }
        convert_dimension(mapping, &mut stats).await?;
    }

    Ok(stats)
}

async fn convert_dimension(
    mapping: &DimensionMapping,
    stats: &mut Stats,
) -> Result<(), ConvertError> {
    let region_folder = mapping.dest_dim_dir.join("region");
    let entities_folder = mapping.dest_dim_dir.join("entities");
    let poi_folder = mapping.dest_dim_dir.join("poi");
    std::fs::create_dir_all(&region_folder)?;
    std::fs::create_dir_all(&entities_folder)?;
    std::fs::create_dir_all(&poi_folder)?;

    let level_folder = LevelFolder {
        root_folder: mapping.dest_dim_dir.clone(),
        dim_folder: mapping.dest_dim_dir.clone(),
        region_folder,
        entities_folder,
        poi_folder,
    };

    let chunk_manager =
        ChunkFileManager::<AnvilChunkFile<ChunkData>>::new(AnvilChunkConfig::default());
    let entity_manager =
        ChunkFileManager::<AnvilChunkFile<ChunkEntityData>>::new(AnvilChunkConfig::default());

    let region_count = convert_regions(
        &mapping.source_dim_dir.join("region"),
        &level_folder,
        &chunk_manager,
        stats,
    )
    .await?;

    let entity_count = convert_entities(
        &mapping.source_dim_dir.join("entities"),
        &level_folder,
        &entity_manager,
        stats,
    )
    .await?;

    tracing::info!(
        "{}: converted {region_count} region file(s), {entity_count} entity file(s)",
        mapping.label,
    );

    Ok(())
}

async fn convert_regions(
    source_region_dir: &Path,
    level_folder: &LevelFolder,
    chunk_manager: &ChunkFileManager<AnvilChunkFile<ChunkData>>,
    stats: &mut Stats,
) -> Result<usize, ConvertError> {
    if !source_region_dir.exists() {
        return Ok(0);
    }

    let mut region_files_converted = 0;
    for entry in std::fs::read_dir(source_region_dir)? {
        let entry = entry?;
        let os_file_name = entry.file_name();
        let Some(file_name) = os_file_name.to_str() else {
            continue;
        };
        let Some((region_x, region_z)) = region::parse_region_coords(file_name) else {
            continue;
        };

        let raw_chunks = match region::read_region(&entry.path()) {
            Ok(chunks) => chunks,
            Err(err) => {
                tracing::warn!("Skipping unreadable region {file_name}: {err}");
                continue;
            }
        };

        let mut converted = Vec::with_capacity(raw_chunks.len());
        for raw in raw_chunks {
            let (chunk_x, chunk_z) = region::chunk_position(region_x, region_z, raw.index);
            match translate_chunk_bytes(&raw.bytes, chunk_x, chunk_z) {
                Ok((chunk_data, unresolved)) => {
                    stats.unresolved_names.extend(unresolved);
                    chunk_data.mark_dirty(true);
                    converted.push((Vector2::new(chunk_x, chunk_z), Arc::new(chunk_data)));
                    stats.chunks_converted += 1;
                }
                Err(err) => {
                    tracing::warn!("Skipping chunk ({chunk_x}, {chunk_z}): {err}");
                    stats.chunks_skipped += 1;
                }
            }
        }

        chunk_manager
            .save_chunks(level_folder, converted)
            .await
            .map_err(|err| ConvertError::Write(err.to_string()))?;

        region_files_converted += 1;
    }

    Ok(region_files_converted)
}

fn translate_chunk_bytes(
    bytes: &[u8],
    chunk_x: i32,
    chunk_z: i32,
) -> Result<(ChunkData, Vec<String>), String> {
    let mut compound = nbt_io::read_compound(bytes).map_err(|err| err.to_string())?;
    let unresolved = palette::translate_chunk(&mut compound).map_err(|err| err.to_string())?;
    let translated_bytes = nbt_io::write_compound(compound);

    let chunk_data =
        ChunkData::internal_from_bytes(&translated_bytes, Vector2::new(chunk_x, chunk_z))
            .map_err(|err| err.to_string())?;

    Ok((chunk_data, unresolved))
}

async fn convert_entities(
    source_entities_dir: &Path,
    level_folder: &LevelFolder,
    entity_manager: &ChunkFileManager<AnvilChunkFile<ChunkEntityData>>,
    stats: &mut Stats,
) -> Result<usize, ConvertError> {
    if !source_entities_dir.exists() {
        return Ok(0);
    }

    let mut region_files_converted = 0;
    for entry in std::fs::read_dir(source_entities_dir)? {
        let entry = entry?;
        let os_file_name = entry.file_name();
        let Some(file_name) = os_file_name.to_str() else {
            continue;
        };
        let Some((region_x, region_z)) = region::parse_region_coords(file_name) else {
            continue;
        };

        let raw_chunks = match region::read_region(&entry.path()) {
            Ok(chunks) => chunks,
            Err(err) => {
                tracing::warn!("Skipping unreadable entity region {file_name}: {err}");
                continue;
            }
        };

        let mut converted = Vec::with_capacity(raw_chunks.len());
        for raw in raw_chunks {
            let (chunk_x, chunk_z) = region::chunk_position(region_x, region_z, raw.index);
            let position = Vector2::new(chunk_x, chunk_z);
            // Entities don't reference a numeric block palette, so no
            // translation is needed - just re-parse and re-write through
            // Pumpkin's own entity chunk format.
            let entity_bytes = bytes::Bytes::from(raw.bytes);
            match ChunkEntityData::from_bytes(&entity_bytes, position) {
                Ok(entity_data) => {
                    entity_data.mark_dirty(true);
                    converted.push((position, Arc::new(entity_data)));
                    stats.entity_chunks_converted += 1;
                }
                Err(err) => {
                    tracing::warn!("Skipping entity chunk ({chunk_x}, {chunk_z}): {err}");
                    stats.entity_chunks_skipped += 1;
                }
            }
        }

        entity_manager
            .save_chunks(level_folder, converted)
            .await
            .map_err(|err| ConvertError::Write(err.to_string()))?;

        region_files_converted += 1;
    }

    Ok(region_files_converted)
}
