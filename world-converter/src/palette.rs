//! Translates vanilla chunk NBT into Pumpkin's own palette shape, in place.
//!
//! Vanilla/Paper stores each chunk section's `block_states.palette` as a
//! list of `{Name, Properties}` compounds (e.g. `{Name: "minecraft:stone"}`)
//! and `biomes.palette` as a list of name strings (e.g. `"minecraft:plains"`).
//! Pumpkin's chunk reader (`ChunkData::internal_from_bytes`) instead expects
//! both palettes to already be raw numeric IDs - that mismatch is the whole
//! reason a copied-in Paper world renders as empty air. This module rewrites
//! just those two palettes and leaves everything else (`xPos`, section `Y`,
//! the packed `data` long arrays, `Status`, `Heightmaps`, block entities,
//! ticks...) untouched, so the result can be handed straight to Pumpkin's
//! own chunk parser.

use pumpkin_data::BlockStateId;
use pumpkin_data::biome::Biome;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_nbt::tag::NbtTag;
use pumpkin_world::generation::structure::template::{BlockStateResolver, PaletteEntry};

/// A genuinely malformed palette entry that we cannot recover from at all -
/// as opposed to a block/biome name that's simply unrecognized, which is
/// handled by substituting a placeholder (see `translate_chunk`'s return
/// value) rather than failing the whole chunk.
#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    #[error("block palette entry has no 'Name' tag")]
    MissingBlockName,
}

/// Rewrites every section's `block_states.palette` and `biomes.palette` in
/// `root` from vanilla's Name/Properties shape into Pumpkin's raw numeric
/// IDs.
///
/// A block or biome name that Pumpkin's registry doesn't recognize (e.g. one
/// renamed between versions) is substituted with air / biome 0 rather than
/// failing the whole chunk over a single unknown block - that would throw
/// away far more than it saves. Every such substitution is returned so the
/// caller can report a summary instead of one warning per occurrence.
pub fn translate_chunk(root: &mut NbtCompound) -> Result<Vec<String>, TranslateError> {
    let mut unresolved = Vec::new();

    let Some(NbtTag::List(sections)) = root.child_tags.get_mut("sections") else {
        return Ok(unresolved);
    };

    for section in sections.iter_mut() {
        let NbtTag::Compound(section) = section else {
            continue;
        };
        translate_block_states(section, &mut unresolved)?;
        translate_biomes(section, &mut unresolved);
    }

    Ok(unresolved)
}

fn translate_block_states(
    section: &mut NbtCompound,
    unresolved: &mut Vec<String>,
) -> Result<(), TranslateError> {
    let Some(NbtTag::Compound(block_states)) = section.child_tags.get_mut("block_states") else {
        return Ok(());
    };
    let Some(NbtTag::List(palette)) = block_states.child_tags.get_mut("palette") else {
        return Ok(());
    };

    for entry in palette.iter_mut() {
        let NbtTag::Compound(entry_compound) = entry else {
            // Already numeric - e.g. re-running the tool on Pumpkin's own output.
            continue;
        };

        let name = entry_compound
            .get_string("Name")
            .ok_or(TranslateError::MissingBlockName)?
            .to_string();

        let properties: Vec<(String, String)> = entry_compound
            .get_compound("Properties")
            .map(|props| {
                props
                    .child_tags
                    .iter()
                    .filter_map(|(key, value)| match value {
                        NbtTag::String(v) => Some((key.to_string(), v.to_string())),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();

        let palette_entry = if properties.is_empty() {
            PaletteEntry::new(name.clone())
        } else {
            PaletteEntry::with_properties(name.clone(), properties)
        };

        let state_id = BlockStateResolver::resolve_simple(&palette_entry).map_or_else(
            || {
                unresolved.push(format!("block {name}"));
                BlockStateId::AIR.as_u16()
            },
            |state| state.id.as_u16(),
        );

        *entry = NbtTag::Int(i32::from(state_id));
    }

    Ok(())
}

fn translate_biomes(section: &mut NbtCompound, unresolved: &mut Vec<String>) {
    let Some(NbtTag::Compound(biomes)) = section.child_tags.get_mut("biomes") else {
        return;
    };
    let Some(NbtTag::List(palette)) = biomes.child_tags.get_mut("palette") else {
        return;
    };

    for entry in palette.iter_mut() {
        let NbtTag::String(name) = entry else {
            continue; // Already numeric.
        };
        let bare_name = name.strip_prefix("minecraft:").unwrap_or(name);
        let id = Biome::from_name(bare_name).map_or_else(
            || {
                unresolved.push(format!("biome {name}"));
                0
            },
            |biome| biome.id,
        );

        *entry = NbtTag::Byte(id as i8);
    }
}

#[cfg(test)]
mod tests {
    use pumpkin_data::Block;
    use pumpkin_nbt::{compound::NbtCompound, tag::NbtTag};

    use super::translate_chunk;

    fn root_with_section(section: NbtCompound) -> NbtCompound {
        let mut root = NbtCompound::new();
        root.put_list("sections", vec![NbtTag::Compound(section)]);
        root
    }

    fn first_section_palette(root: &NbtCompound, kind: &str) -> Vec<NbtTag> {
        let sections = root.get_list("sections").unwrap();
        let NbtTag::Compound(section) = &sections[0] else {
            panic!("expected a compound section")
        };
        let inner = section.get_compound(kind).unwrap();
        inner.get_list("palette").unwrap().to_vec()
    }

    #[test]
    fn translates_simple_block_palette() {
        let mut name_compound = NbtCompound::new();
        name_compound.put("Name", "minecraft:stone");

        let mut block_states = NbtCompound::new();
        block_states.put_list("palette", vec![NbtTag::Compound(name_compound)]);
        let mut section = NbtCompound::new();
        section.put("block_states", NbtTag::Compound(block_states));

        let mut root = root_with_section(section);
        translate_chunk(&mut root).expect("translation should succeed");

        let palette = first_section_palette(&root, "block_states");
        let NbtTag::Int(id) = palette[0] else {
            panic!("expected a numeric palette entry")
        };
        assert_eq!(id as u16, Block::STONE.default_state.id.as_u16());
    }

    #[test]
    fn translates_block_palette_with_properties() {
        let mut props = NbtCompound::new();
        props.put("axis", "y");
        let mut name_compound = NbtCompound::new();
        name_compound.put("Name", "minecraft:deepslate");
        name_compound.put("Properties", NbtTag::Compound(props));

        let mut block_states = NbtCompound::new();
        block_states.put_list("palette", vec![NbtTag::Compound(name_compound)]);
        let mut section = NbtCompound::new();
        section.put("block_states", NbtTag::Compound(block_states));

        let mut root = root_with_section(section);
        translate_chunk(&mut root).expect("translation should succeed");

        let palette = first_section_palette(&root, "block_states");
        assert!(matches!(palette[0], NbtTag::Int(_)));
    }

    #[test]
    fn unknown_block_becomes_air_and_is_reported() {
        let mut name_compound = NbtCompound::new();
        name_compound.put("Name", "minecraft:not_a_real_block");

        let mut block_states = NbtCompound::new();
        block_states.put_list("palette", vec![NbtTag::Compound(name_compound)]);
        let mut section = NbtCompound::new();
        section.put("block_states", NbtTag::Compound(block_states));

        let mut root = root_with_section(section);
        let unresolved =
            translate_chunk(&mut root).expect("an unknown block should not fail the chunk");
        assert_eq!(unresolved, vec!["block minecraft:not_a_real_block"]);

        let palette = first_section_palette(&root, "block_states");
        let NbtTag::Int(id) = palette[0] else {
            panic!("expected a numeric palette entry")
        };
        assert_eq!(id as u16, pumpkin_data::BlockStateId::AIR.as_u16());
    }

    #[test]
    fn translates_biome_palette() {
        let mut biomes = NbtCompound::new();
        biomes.put_list("palette", vec![NbtTag::String("minecraft:plains".into())]);
        let mut section = NbtCompound::new();
        section.put("biomes", NbtTag::Compound(biomes));

        let mut root = root_with_section(section);
        translate_chunk(&mut root).expect("translation should succeed");

        let palette = first_section_palette(&root, "biomes");
        assert!(matches!(palette[0], NbtTag::Byte(_)));
    }

    /// The real proof: take an actual chunk out of the user's Paper world,
    /// translate it, and confirm Pumpkin's own chunk parser now reads real
    /// terrain out of it instead of collapsing every section to air.
    #[test]
    fn end_to_end_translates_real_chunk_to_real_blocks() {
        use pumpkin_util::math::vector2::Vector2;
        use pumpkin_world::chunk::ChunkData;

        use crate::{nbt_io, region};

        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("paper-world/dimensions/minecraft/overworld/region/r.0.0.mca");
        let (region_x, region_z) =
            region::parse_region_coords(path.file_name().unwrap().to_str().unwrap()).unwrap();

        let chunks = region::read_region(&path).expect("should read region file");
        let raw = chunks
            .iter()
            .find(|c| {
                let (x, z) = region::chunk_position(region_x, region_z, c.index);
                // A chunk near spawn is virtually guaranteed to be real
                // terrain rather than an edge chunk that never generated.
                (0..4).contains(&x) && (0..4).contains(&z)
            })
            .expect("expected at least one generated chunk near spawn");

        let (chunk_x, chunk_z) = region::chunk_position(region_x, region_z, raw.index);

        let mut compound = nbt_io::read_compound(&raw.bytes).expect("valid source NBT");
        translate_chunk(&mut compound).expect("translation should succeed");
        let translated_bytes = nbt_io::write_compound(compound);

        let chunk_data =
            ChunkData::internal_from_bytes(&translated_bytes, Vector2::new(chunk_x, chunk_z))
                .expect("Pumpkin's own parser should now accept the translated chunk");

        let height = chunk_data.section.count * 16;
        let non_air = (0..height)
            .flat_map(|y| (0..16).flat_map(move |x| (0..16).map(move |z| (x, y, z))))
            .filter_map(|(x, y, z)| chunk_data.get_relative_block(x, y, z))
            .filter(|id| !id.to_state().is_air())
            .count();
        assert!(
            non_air > 0,
            "translated chunk at ({chunk_x}, {chunk_z}) has no non-air blocks - the palette fix didn't take"
        );
    }
}
