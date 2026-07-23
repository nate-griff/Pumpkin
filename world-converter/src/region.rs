//! Minimal reader for vanilla Anvil region files (`.mca`).
//!
//! We can't reuse Pumpkin's own `AnvilChunkFile` for the *source* side: it
//! decodes each chunk through `ChunkData::from_bytes`, which assumes
//! Pumpkin's own numeric block-state palette. Vanilla/Paper chunks use a
//! different palette shape (`Name`/`Properties` compounds), so decoding them
//! through Pumpkin's reader silently turns every block into air. This
//! reader only unpacks the region-file container - the sector table and
//! per-chunk compression - and hands back raw, untouched chunk NBT bytes for
//! the caller to translate.

use std::io::Read;
use std::path::Path;

use bytes::Buf;
use flate2::read::{GzDecoder, ZlibDecoder};

const SECTOR_BYTES: usize = 4096;
pub const CHUNK_COUNT: usize = 32 * 32;

#[derive(Debug, thiserror::Error)]
pub enum RegionReadError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("region file too short for header")]
    TooShort,
    #[error("unknown compression type {0}")]
    UnknownCompression(u8),
    #[error("chunk {0} claims bytes beyond end of file")]
    OutOfBounds(usize),
    #[error("chunk {0} claims a length beyond its own sectors")]
    BadLength(usize),
}

/// One chunk read from a region file: its index within the region
/// (`0..1024`, laid out as `z * 32 + x`) and its raw, decompressed NBT
/// bytes.
pub struct RawChunk {
    pub index: usize,
    pub bytes: Vec<u8>,
}

/// Reads every present chunk out of a vanilla `.mca` region file.
pub fn read_region(path: &Path) -> Result<Vec<RawChunk>, RegionReadError> {
    let data = std::fs::read(path)?;
    if data.len() < SECTOR_BYTES * 2 {
        return Err(RegionReadError::TooShort);
    }

    let mut chunks = Vec::new();
    for index in 0..CHUNK_COUNT {
        let mut location = &data[index * 4..index * 4 + 4];
        let location = location.get_u32();

        let sector_count = (location & 0xFF) as usize;
        let sector_offset = (location >> 8) as usize;
        if sector_offset == 0 || sector_count == 0 {
            continue; // Chunk was never generated.
        }

        let start = sector_offset * SECTOR_BYTES;
        let end = start + sector_count * SECTOR_BYTES;
        if end > data.len() {
            return Err(RegionReadError::OutOfBounds(index));
        }

        let sectors = &data[start..end];
        let mut length_bytes = &sectors[0..4];
        let length = length_bytes.get_u32() as usize;
        if length == 0 || length - 1 > sectors.len() - 5 {
            return Err(RegionReadError::BadLength(index));
        }
        let compression = sectors[4];
        let payload = &sectors[5..5 + (length - 1)];

        let bytes = decompress(compression, payload)?;
        chunks.push(RawChunk { index, bytes });
    }

    Ok(chunks)
}

fn decompress(compression: u8, data: &[u8]) -> Result<Vec<u8>, RegionReadError> {
    match compression {
        1 => {
            let mut out = Vec::new();
            GzDecoder::new(data).read_to_end(&mut out)?;
            Ok(out)
        }
        2 => {
            let mut out = Vec::new();
            ZlibDecoder::new(data).read_to_end(&mut out)?;
            Ok(out)
        }
        3 => Ok(data.to_vec()),
        4 => {
            let mut out = Vec::new();
            lz4_java_wrc::Lz4BlockInput::new(data).read_to_end(&mut out)?;
            Ok(out)
        }
        other => Err(RegionReadError::UnknownCompression(other)),
    }
}

/// Converts a chunk's index within a region (`0..1024`) plus the region's
/// own coordinates (parsed from its `r.{x}.{z}.mca` filename) into the
/// chunk's absolute chunk coordinates.
#[must_use]
pub const fn chunk_position(region_x: i32, region_z: i32, index: usize) -> (i32, i32) {
    let local_x = (index % 32) as i32;
    let local_z = (index / 32) as i32;
    (region_x * 32 + local_x, region_z * 32 + local_z)
}

/// Parses `region_x`/`region_z` out of a region filename like `r.-1.2.mca`.
#[must_use]
pub fn parse_region_coords(file_name: &str) -> Option<(i32, i32)> {
    let stem = file_name.strip_prefix("r.")?.strip_suffix(".mca")?;
    let (x, z) = stem.split_once('.')?;
    Some((x.parse().ok()?, z.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{chunk_position, parse_region_coords, read_region};

    #[test]
    fn reads_real_paper_region_file() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("paper-world/dimensions/minecraft/overworld/region/r.0.0.mca");
        let chunks = read_region(&path).expect("should read region file");
        assert!(
            !chunks.is_empty(),
            "expected at least one chunk in r.0.0.mca"
        );
        for chunk in &chunks {
            // Every chunk's NBT payload must start with a Compound tag (0x0a).
            assert_eq!(
                chunk.bytes[0], 0x0a,
                "chunk {} did not start with TAG_Compound",
                chunk.index
            );
        }
    }

    #[test]
    fn parses_region_coords() {
        assert_eq!(parse_region_coords("r.-1.2.mca"), Some((-1, 2)));
        assert_eq!(parse_region_coords("r.0.0.mca"), Some((0, 0)));
        assert_eq!(parse_region_coords("not-a-region-file"), None);
    }

    #[test]
    fn computes_chunk_position() {
        // Region (0, 0), index 0 -> local (0, 0) -> chunk (0, 0).
        assert_eq!(chunk_position(0, 0, 0), (0, 0));
        // Region (0, 0), index 33 -> local (1, 1) -> chunk (1, 1).
        assert_eq!(chunk_position(0, 0, 33), (1, 1));
        // Region (-1, 2), index 0 -> chunk (-32, 64).
        assert_eq!(chunk_position(-1, 2, 0), (-32, 64));
    }
}
