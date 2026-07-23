//! Converts the world's top-level metadata: seed, spawn point, difficulty,
//! game rules, weather, and world clocks.
//!
//! Paper's `level.dat` has a completely different shape from Pumpkin's
//! (nested `spawn.pos`/`spawn.yaw` instead of flat `SpawnX`/`SpawnY`/`SpawnZ`,
//! a `difficulty_settings.difficulty` string instead of a numeric byte, no
//! top-level `WorldGenSettings` at all) - trying to read it through
//! Pumpkin's own typed `level.dat` reader is exactly what crashes in
//! <https://github.com/Pumpkin-MC/Pumpkin/issues/1554>. So spawn/difficulty
//! are pulled out generically here instead.
//!
//! Everything else - seed, game rules, weather, and per-dimension clocks -
//! Paper 26.2 already writes as the *same* per-dimension sidecar files
//! (`data/minecraft/{world_gen_settings,game_rules,weather,world_clocks}.dat`)
//! that Pumpkin itself reads and writes, so those are read with Pumpkin's own
//! `pumpkin_world::world_info::data_files` readers directly - no
//! reimplementation needed, and no risk of drifting from Pumpkin's own
//! format.

use std::io::Read;
use std::path::Path;

use flate2::read::GzDecoder;
use pumpkin_util::Difficulty;
use pumpkin_util::world_seed::Seed;
use pumpkin_world::world_info::anvil::AnvilLevelInfo;
use pumpkin_world::world_info::data_files::{
    read_game_rules, read_weather, read_world_clocks, read_world_gen_settings, write_weather,
    write_world_clocks,
};
use pumpkin_world::world_info::{LevelData, WorldInfoWriter};

use crate::nbt_io;

#[derive(Debug, thiserror::Error)]
pub enum LevelDatError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("nbt error: {0}")]
    Nbt(#[from] pumpkin_nbt::Error),
    #[error("level.dat is missing the '{0}' field")]
    MissingField(&'static str),
    #[error("world_info error: {0}")]
    WorldInfo(String),
}

struct SourceSpawn {
    data_version: i32,
    x: i32,
    y: i32,
    z: i32,
    yaw: f32,
    pitch: f32,
    difficulty: Difficulty,
}

/// Pulls spawn position/orientation, difficulty, and `DataVersion` out of
/// Paper's `level.dat` generically - see the module docs for why this can't
/// go through Pumpkin's own typed reader.
fn read_source_spawn(source_root: &Path) -> Result<SourceSpawn, LevelDatError> {
    let file = std::fs::File::open(source_root.join("level.dat"))?;
    let mut raw = Vec::new();
    GzDecoder::new(file).read_to_end(&mut raw)?;

    let root = nbt_io::read_compound(&raw)?;
    let data = root
        .get_compound("Data")
        .ok_or(LevelDatError::MissingField("Data"))?;

    let data_version = data
        .get_int("DataVersion")
        .ok_or(LevelDatError::MissingField("Data.DataVersion"))?;

    let spawn = data
        .get_compound("spawn")
        .ok_or(LevelDatError::MissingField("Data.spawn"))?;
    let pos = spawn
        .get_int_array("pos")
        .ok_or(LevelDatError::MissingField("Data.spawn.pos"))?;
    if pos.len() != 3 {
        return Err(LevelDatError::MissingField("Data.spawn.pos (not len 3)"));
    }
    let (x, y, z) = (pos[0], pos[1], pos[2]);
    let yaw = spawn.get_float("yaw").unwrap_or(0.0);
    let pitch = spawn.get_float("pitch").unwrap_or(0.0);

    let difficulty = data
        .get_compound("difficulty_settings")
        .and_then(|d| d.get_string("difficulty"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(Difficulty::Normal);

    Ok(SourceSpawn {
        data_version,
        x,
        y,
        z,
        yaw,
        pitch,
        difficulty,
    })
}

/// Writes `dest_root/level.dat` (and its `data/minecraft/*.dat` sidecar
/// files) using values carried over from the Paper world at `source_root`.
pub fn convert_level_dat(source_root: &Path, dest_root: &Path) -> Result<(), LevelDatError> {
    let spawn = read_source_spawn(source_root)?;

    // Paper already keeps these in the same per-dimension sidecar files
    // Pumpkin reads/writes, so pull them straight through Pumpkin's own
    // readers - the overworld's copy is authoritative for the whole world.
    let overworld_data_dir = source_root.join("dimensions/minecraft/overworld");
    let world_gen_settings = read_world_gen_settings(&overworld_data_dir)
        .ok_or(LevelDatError::MissingField("world_gen_settings.dat"))?;
    let game_rules = read_game_rules(&overworld_data_dir);
    let weather = read_weather(&overworld_data_dir);
    let clocks = read_world_clocks(&overworld_data_dir);
    let day_time = clocks
        .clocks
        .get("minecraft:overworld")
        .map_or(0, |c| c.total_ticks);

    let mut level_data = LevelData::default(Seed(world_gen_settings.seed as u64));
    level_data.data_version = spawn.data_version;
    level_data.spawn_x = spawn.x;
    level_data.spawn_y = spawn.y;
    level_data.spawn_z = spawn.z;
    level_data.spawn_yaw = spawn.yaw;
    level_data.spawn_pitch = spawn.pitch;
    level_data.difficulty = spawn.difficulty;
    level_data.world_gen_settings = world_gen_settings;
    level_data.game_rules = game_rules;
    level_data.day_time = day_time;
    level_data.clear_weather_time = weather.clear_weather_time;

    AnvilLevelInfo
        .write_world_info(&level_data, dest_root)
        .map_err(|err| LevelDatError::WorldInfo(err.to_string()))?;

    // `write_world_info` only carries the overworld's clock through; write
    // the full multi-dimension clocks and weather state back over it so
    // nether/end clocks and the rest of the weather state aren't lost.
    write_world_clocks(dest_root, &clocks)
        .map_err(|err| LevelDatError::WorldInfo(err.to_string()))?;
    write_weather(dest_root, &weather).map_err(|err| LevelDatError::WorldInfo(err.to_string()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use pumpkin_world::world_info::WorldInfoReader;
    use pumpkin_world::world_info::anvil::AnvilLevelInfo;

    use super::convert_level_dat;

    /// Converts the real Paper world's `level.dat` into a scratch directory,
    /// then reads it back through Pumpkin's own *strict* typed reader (the
    /// same one that crashes on Paper's raw file per issue #1554) to prove
    /// what we wrote is fully compatible - not just that our own writer
    /// didn't error.
    #[test]
    fn converted_level_dat_round_trips_through_pumpkins_own_reader() {
        let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../paper-world");
        let dest_root = std::env::temp_dir().join("world-converter-level-dat-test");
        std::fs::create_dir_all(&dest_root).unwrap();

        convert_level_dat(&source_root, &dest_root).expect("conversion should succeed");

        let info = AnvilLevelInfo
            .read_world_info(&dest_root)
            .expect("Pumpkin's own strict reader should accept what we wrote");

        // The real seed we confirmed earlier via direct inspection.
        assert_eq!(info.world_gen_settings.seed, -718_261_439_703_942_962);
        assert_eq!(info.spawn_x, 0);
        assert_eq!(info.spawn_y, 76);
        assert_eq!(info.spawn_z, 0);
        assert_eq!(info.difficulty, pumpkin_util::Difficulty::Normal);
        // Confirms game rules were actually carried over from the source
        // world rather than silently defaulted - this world has
        // `keepInventory=false` set, which isn't the engine default.
        assert!(!info.game_rules.keep_inventory);
        assert!(info.game_rules.mob_griefing);

        std::fs::remove_dir_all(&dest_root).ok();
    }
}
