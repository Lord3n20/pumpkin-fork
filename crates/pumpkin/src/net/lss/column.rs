//! Column encoding: the v20 section array (identity dictionary instead of registry ids),
//! the `lss:voxel_column` frame around it, and the optional zstd codec.

use std::collections::HashMap;

use pumpkin_data::chunk::Biome;
use pumpkin_data::chunk_status::ChunkStatus;
use pumpkin_data::{Block, BlockStateId};
use pumpkin_world::chunk::ChunkData;
use pumpkin_world::chunk::format::LightContainer;
use pumpkin_world::chunk::palette::{BiomePalette, BlockPalette};

use super::write_var_int;

pub const COLUMN_SOURCE_IN_MEMORY: u8 = 0;
pub const COLUMN_SOURCE_DISK: u8 = 1;
const COLUMN_CODEC_RAW: u8 = 0;
const COLUMN_CODEC_ZSTD: u8 = 1;
/// Smaller bodies are never compressed (LSS `COLUMN_COMPRESS_MIN_BYTES`).
const COLUMN_COMPRESS_MIN_BYTES: usize = 512;
const MAX_SEND_SECTIONS_SIZE: usize = 2_000_000;
const LIGHT_BYTES: usize = 2048;

/// The raw v20 section bytes of a column plus where they came from.
pub struct EncodedColumn {
    pub source: u8,
    pub sections: Vec<u8>,
}

/// Encodes a full chunk as the v20 section array, or `None` when it is not fully generated.
pub fn encode_column(chunk: &ChunkData, source: u8) -> Option<EncodedColumn> {
    if chunk.status != ChunkStatus::Full {
        return None;
    }
    let min_section = chunk.section.min_y >> 4;
    let block_sections = chunk
        .section
        .block_sections
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let biome_sections = chunk
        .section
        .biome_sections
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let obfuscated = pumpkin_world::anti_xray::get()
        .and_then(|anti_xray| anti_xray.obfuscate(&block_sections, chunk.section.min_y));
    let blocks: &[BlockPalette] = obfuscated.as_deref().unwrap_or(&block_sections);
    let light = chunk
        .light_engine
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // Which sections to send, like LSS's SectionSerializer: everything with blocks or block
    // light, plus sky-lit air one section around the terrain (lights its faces at borders).
    let has_light = |layers: &[LightContainer], i: usize| match layers.get(i) {
        Some(LightContainer::Full(data)) => data.iter().any(|&b| b != 0),
        Some(LightContainer::Empty(level)) => *level != 0,
        None => false,
    };
    let mut content = (0..blocks.len()).filter(|&i| !blocks[i].has_only_air());
    let Some(lowest) = content.next() else {
        // All air: the clear column (empty dictionary, no sections)
        return Some(EncodedColumn {
            source,
            sections: vec![0, 0],
        });
    };
    let highest = content.last().unwrap_or(lowest);
    let included: Vec<usize> = (0..blocks.len())
        .filter(|&i| {
            !blocks[i].has_only_air()
                || has_light(&light.block_light, i)
                || (has_light(&light.sky_light, i) && i + 1 >= lowest && i <= highest + 1)
        })
        .collect();

    let mut dictionary = Dictionary::default();
    let mut body = Vec::with_capacity(included.len() * 4096);
    write_var_int(&mut body, included.len() as i32);
    for &i in &included {
        body.push((min_section + i as i32) as i8 as u8);
        body.extend_from_slice(&(blocks[i].non_air_block_count() as i16).to_be_bytes());
        body.extend_from_slice(&(blocks[i].liquid_block_count() as i16).to_be_bytes());
        let states: Vec<BlockStateId> = blocks[i].iter().collect();
        write_container(&mut body, &states, true, |state| {
            dictionary.index_of(block_identity(state))
        });
        let biomes: Vec<u8> = biome_section(&biome_sections, i).iter().collect();
        write_container(&mut body, &biomes, false, |id| {
            dictionary.index_of(biome_identity(id))
        });
        write_light(&mut body, light.block_light.get(i));
        write_light(&mut body, light.sky_light.get(i));
    }

    let mut sections = Vec::with_capacity(body.len() + dictionary.bytes.len() + 4);
    write_var_int(&mut sections, dictionary.entries.len() as i32);
    sections.extend_from_slice(&dictionary.bytes);
    sections.extend_from_slice(&body);
    (sections.len() <= MAX_SEND_SECTIONS_SIZE).then_some(EncodedColumn { source, sections })
}

fn biome_section(sections: &[BiomePalette], i: usize) -> &BiomePalette {
    &sections[i.min(sections.len().saturating_sub(1))]
}

/// `lss:voxel_column`: coords, dimension, timestamp, source, codec, length-prefixed sections.
/// Sections are zstd-compressed when the client declared it can decode them.
pub fn column_frame(
    x: i32,
    z: i32,
    dimension: &str,
    timestamp: i64,
    column: &EncodedColumn,
    zstd: bool,
) -> Vec<u8> {
    let compressed = (zstd && column.sections.len() >= COLUMN_COMPRESS_MIN_BYTES)
        .then(|| zstd_frame(&column.sections))
        .filter(|frame| frame.len() < column.sections.len());
    let (codec, shipped) = match &compressed {
        Some(frame) => (COLUMN_CODEC_ZSTD, frame.as_slice()),
        None => (COLUMN_CODEC_RAW, column.sections.as_slice()),
    };
    let mut frame = Vec::with_capacity(shipped.len() + dimension.len() + 32);
    frame.extend_from_slice(&x.to_be_bytes());
    frame.extend_from_slice(&z.to_be_bytes());
    write_var_int(&mut frame, dimension.len() as i32);
    frame.extend_from_slice(dimension.as_bytes());
    frame.extend_from_slice(&timestamp.to_be_bytes());
    frame.push(column.source);
    frame.push(codec);
    write_var_int(&mut frame, shipped.len() as i32);
    frame.extend_from_slice(shipped);
    frame
}

/// A zstd frame that declares its content size: the LSS client refuses frames without it
/// (decompression-bomb guard), and ruzstd does not write one, so it is patched in.
fn zstd_frame(data: &[u8]) -> Vec<u8> {
    let mut frame =
        ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest);
    // Magic (4 bytes), then the frame header descriptor. ruzstd writes no content size, no
    // single-segment flag and no dictionary, so a window descriptor byte follows.
    let descriptor = frame[4];
    debug_assert_eq!(descriptor & 0b1110_0011, 0);
    // Frame_Content_Size_flag = 2: four little-endian bytes after the window descriptor
    frame[4] = descriptor | 0b1000_0000;
    let size = (data.len() as u32).to_le_bytes();
    frame.splice(6..6, size);
    frame
}

#[derive(Default)]
struct Dictionary {
    entries: HashMap<String, i32>,
    bytes: Vec<u8>,
}

impl Dictionary {
    fn index_of(&mut self, identity: String) -> i32 {
        let next = self.entries.len() as i32;
        if let Some(&index) = self.entries.get(&identity) {
            return index;
        }
        write_var_int(&mut self.bytes, identity.len() as i32);
        self.bytes.extend_from_slice(identity.as_bytes());
        self.entries.insert(identity, next);
        next
    }
}

/// Canonical identity: `namespace:path[k1=v1,k2=v2]`, properties sorted by name.
pub fn block_identity(state: BlockStateId) -> String {
    let block = Block::from_state_id(state);
    let mut identity = if block.name.contains(':') {
        block.name.to_string()
    } else {
        format!("minecraft:{}", block.name)
    };
    if let Some(properties) = block.properties(state) {
        let mut properties = properties.to_props();
        if !properties.is_empty() {
            properties.sort_unstable_by_key(|(name, _)| *name);
            identity.push('[');
            for (n, (name, value)) in properties.iter().enumerate() {
                if n > 0 {
                    identity.push(',');
                }
                identity.push_str(name);
                identity.push('=');
                identity.push_str(value);
            }
            identity.push(']');
        }
    }
    identity
}

fn biome_identity(id: u8) -> String {
    Biome::from_id(id).map_or_else(
        || "minecraft:plains".to_string(),
        |biome| format!("minecraft:{}", biome.registry_id),
    )
}

/// v20 container: `bits`, palette of dictionary indices, packed palette indices.
/// Blocks use 0 or 4..=12 bits, biomes 0..=6; values never span two longs.
fn write_container<V: Copy + Eq + std::hash::Hash>(
    out: &mut Vec<u8>,
    values: &[V],
    is_blocks: bool,
    mut identity_index: impl FnMut(V) -> i32,
) {
    let mut palette: Vec<V> = Vec::new();
    let mut lookup: HashMap<V, u64> = HashMap::new();
    let indices: Vec<u64> = values
        .iter()
        .map(|&v| {
            *lookup.entry(v).or_insert_with(|| {
                palette.push(v);
                (palette.len() - 1) as u64
            })
        })
        .collect();

    if palette.len() == 1 {
        out.push(0);
        write_var_int(out, identity_index(palette[0]));
        return;
    }
    let ceil_log2 = usize::BITS - (palette.len() - 1).leading_zeros();
    let bits = if is_blocks { ceil_log2.clamp(4, 12) } else { ceil_log2 } as usize;
    out.push(bits as u8);
    write_var_int(out, palette.len() as i32);
    for &value in &palette {
        write_var_int(out, identity_index(value));
    }
    let per_long = 64 / bits;
    for chunk in indices.chunks(per_long) {
        let mut word = 0u64;
        for (slot, &index) in chunk.iter().enumerate() {
            word |= index << (slot * bits);
        }
        out.extend_from_slice(&word.to_be_bytes());
    }
}

fn write_light(out: &mut Vec<u8>, layer: Option<&LightContainer>) {
    match layer {
        Some(LightContainer::Full(data))
            if data.len() == LIGHT_BYTES && data.iter().any(|&b| b != 0) =>
        {
            out.push(1);
            out.extend_from_slice(data);
        }
        Some(LightContainer::Empty(level)) if *level != 0 => {
            out.push(1);
            out.extend(std::iter::repeat_n(*level | (*level << 4), LIGHT_BYTES));
        }
        _ => out.push(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_identities_are_canonical() {
        assert_eq!(block_identity(Block::STONE.default_state.id), "minecraft:stone");
        let identity = block_identity(Block::OAK_STAIRS.default_state.id);
        assert!(identity.starts_with("minecraft:oak_stairs[facing="), "{identity}");
        let properties: Vec<&str> = identity["minecraft:oak_stairs[".len()..identity.len() - 1]
            .split(',')
            .map(|p| p.split('=').next().unwrap_or_default())
            .collect();
        let mut sorted = properties.clone();
        sorted.sort_unstable();
        assert_eq!(properties, sorted);
    }

    #[test]
    fn container_packing() {
        let mut dict = Dictionary::default();
        let mut out = Vec::new();
        let values: Vec<u8> = (0..64).map(|i| (i % 3) as u8).collect();
        write_container(&mut out, &values, false, |v| dict.index_of(format!("b{v}")));
        // 3 entries -> 2 bits, 32 per long -> 2 longs
        assert_eq!(out[0], 2);
        assert_eq!(out[1], 3);
        assert_eq!(out.len(), 2 + 3 + 16);
        let first = u64::from_be_bytes(out[5..13].try_into().unwrap());
        assert_eq!(first & 0b11, 0);
        assert_eq!((first >> 2) & 0b11, 1);
        assert_eq!((first >> 4) & 0b11, 2);
    }

    #[test]
    fn zstd_frame_declares_size_and_round_trips() {
        let data: Vec<u8> = (0..20_000u32).map(|i| (i % 7) as u8).collect();
        let frame = zstd_frame(&data);
        assert_eq!(frame[4] >> 6, 2);
        assert_eq!(u32::from_le_bytes(frame[6..10].try_into().unwrap()), 20_000);
        let mut decoder = ruzstd::decoding::StreamingDecoder::new(frame.as_slice()).unwrap();
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut out).unwrap();
        assert_eq!(out, data);
    }
}
