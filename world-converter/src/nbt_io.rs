//! Thin generic NBT read/write helpers shared by chunk and entity conversion.
//!
//! Vanilla region-file NBT is a root Compound tag with an empty-string name
//! (`0x0a 0x00 0x00 ...`, detected the same way Pumpkin's own
//! `ChunkData::internal_from_bytes` does it). We always write that same
//! form back out, so whatever we produce is unambiguous to re-parse.

use std::io::Cursor;

use pumpkin_nbt::{Nbt, compound::NbtCompound, deserializer::NbtReadHelperJava};

pub fn read_compound(bytes: &[u8]) -> Result<NbtCompound, pumpkin_nbt::Error> {
    let is_named = bytes.len() >= 3 && bytes[0] == 0x0a && bytes[1] == 0x00 && bytes[2] == 0x00;

    let mut cursor = Cursor::new(bytes);
    let mut reader = NbtReadHelperJava::new(&mut cursor);
    let nbt = if is_named {
        Nbt::read(&mut reader)
    } else {
        Nbt::read_unnamed(&mut reader)
    }?;

    Ok(nbt.root_tag)
}

pub fn write_compound(compound: NbtCompound) -> Vec<u8> {
    Nbt::new(String::new(), compound).write().to_vec()
}
