//! When did a column last change? Saved columns carry their save time in the region file
//! header (epoch seconds per chunk); changes since then are tracked in memory. Both feed the
//! up-to-date answers, the dirty-column notifications and the region summaries.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pumpkin_util::math::position::BlockPos;

use super::{read_var_int, write_var_int};
use crate::world::World;

/// Changes older than this have long been saved and show up in the region header.
const CHANGE_MEMORY: Duration = Duration::from_secs(30 * 60);
const SUMMARY_VERSION: u8 = 1;
const MAX_SUMMARY_TILE_RADIUS: i32 = 65;
const MAX_SUMMARY_TILE_ABS: i64 = 2_000_000;
const WIRE_NEVER_CLEAN: i64 = -1;
const STAMP_NO_REGION: i64 = 0;
const HEADER_BYTES: usize = 8192;

type ColumnKey = (&'static str, i32, i32);

/// Last in-memory change per column (epoch seconds).
static CHANGES: LazyLock<Mutex<HashMap<ColumnKey, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Columns changed since the last dirty broadcast, per dimension.
static PENDING_DIRTY: LazyLock<Mutex<HashMap<&'static str, HashSet<(i32, i32)>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Region headers by file, re-read when the file's modification time changes.
static HEADERS: LazyLock<Mutex<HashMap<PathBuf, (SystemTime, Arc<[u32; 1024]>)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Records block changes; called from `World::flush_block_updates`.
pub fn record_changes<'a>(world: &World, positions: impl Iterator<Item = &'a BlockPos>) {
    let dimension = world.dimension.minecraft_name;
    let columns: HashSet<(i32, i32)> = positions.map(|p| (p.0.x >> 4, p.0.z >> 4)).collect();
    if columns.is_empty() {
        return;
    }
    let now = now_seconds();
    {
        let mut changes = CHANGES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for &(x, z) in &columns {
            changes.insert((dimension, x, z), now);
        }
    }
    PENDING_DIRTY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(dimension)
        .or_default()
        .extend(columns);
}

/// Takes the columns changed since the last call, and forgets old in-memory changes.
pub fn take_dirty() -> HashMap<&'static str, HashSet<(i32, i32)>> {
    let cutoff = now_seconds().saturating_sub(CHANGE_MEMORY.as_secs());
    CHANGES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|_, at| *at >= cutoff);
    std::mem::take(&mut *PENDING_DIRTY.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
}

fn change_stamp(dimension: &'static str, x: i32, z: i32) -> u64 {
    CHANGES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&(dimension, x, z))
        .copied()
        .unwrap_or(0)
}

fn region_path(region_folder: &Path, region_x: i32, region_z: i32) -> PathBuf {
    region_folder.join(format!("r.{region_x}.{region_z}.mca"))
}

/// The timestamp table of a region file, `None` when there is no such file. Blocking.
fn region_header(path: &Path) -> Option<Arc<[u32; 1024]>> {
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    if let Some((cached_at, header)) = HEADERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(path)
        && *cached_at == modified
    {
        return Some(header.clone());
    }
    let mut bytes = [0u8; HEADER_BYTES];
    std::fs::File::open(path).ok()?.read_exact(&mut bytes).ok()?;
    let mut stamps = [0u32; 1024];
    for (i, stamp) in stamps.iter_mut().enumerate() {
        let at = 4096 + i * 4;
        *stamp = u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    }
    let header = Arc::new(stamps);
    HEADERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(path.to_path_buf(), (modified, header.clone()));
    Some(header)
}

/// Save time of one chunk according to its region header, 0 when it was never saved.
/// Blocking.
pub fn saved_stamp(region_folder: &Path, x: i32, z: i32) -> u64 {
    region_header(&region_path(region_folder, x >> 5, z >> 5))
        .map_or(0, |header| u64::from(header[((z & 31) * 32 + (x & 31)) as usize]))
}

/// Last known change of a column: its save time or a later in-memory change. Blocking.
pub fn column_stamp(dimension: &'static str, region_folder: &Path, x: i32, z: i32) -> u64 {
    saved_stamp(region_folder, x, z).max(change_stamp(dimension, x, z))
}

/// Answers `lss:region_summary_req`: per 32x32 tile (one region file) the newest save time,
/// so a reconnecting client can trust every cached column at least that new.
/// Returns `None` for malformed requests or another dimension. Blocking.
pub fn region_summary(world: &World, request: &[u8]) -> Option<Vec<u8>> {
    let mut reader = request;
    let (&version, rest) = reader.split_first()?;
    reader = rest;
    if version != SUMMARY_VERSION {
        return None;
    }
    let dim_len = usize::try_from(read_var_int(&mut reader)?).ok()?;
    if reader.len() < dim_len {
        return None;
    }
    let dimension = std::str::from_utf8(&reader[..dim_len]).ok()?;
    reader = &reader[dim_len..];
    let center_x = read_zig_var_long(&mut reader)?;
    let center_z = read_zig_var_long(&mut reader)?;
    let radius = read_var_int(&mut reader)?;
    if !reader.is_empty()
        || dimension != world.dimension.minecraft_name
        || !(0..=MAX_SUMMARY_TILE_RADIUS).contains(&radius)
        || center_x.abs() > MAX_SUMMARY_TILE_ABS
        || center_z.abs() > MAX_SUMMARY_TILE_ABS
    {
        return None;
    }
    let (center_x, center_z) = (center_x as i32, center_z as i32);

    // Regions with unsaved changes cannot vouch for anything yet
    let world_dimension = world.dimension.minecraft_name;
    let mut unclean: HashSet<(i32, i32)> = CHANGES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .keys()
        .filter(|(dim, _, _)| *dim == world_dimension)
        .map(|&(_, x, z)| (x >> 5, z >> 5))
        .collect();
    for entry in world.level.loaded_chunks.iter() {
        if entry.value().dirty.load(std::sync::atomic::Ordering::Relaxed) {
            unclean.insert((entry.key().x >> 5, entry.key().y >> 5));
        }
    }

    let region_folder = &world.level.level_folder.region_folder;
    let mut out = Vec::with_capacity(32 + (2 * radius as usize + 1).pow(2) * 2);
    out.push(SUMMARY_VERSION);
    write_var_int(&mut out, dimension.len() as i32);
    out.extend_from_slice(dimension.as_bytes());
    write_zig_var_long(&mut out, i64::from(center_x));
    write_zig_var_long(&mut out, i64::from(center_z));
    write_var_int(&mut out, radius);
    let mut previous = 0i64;
    for tile_z in center_z - radius..=center_z + radius {
        for tile_x in center_x - radius..=center_x + radius {
            let stamp = if unclean.contains(&(tile_x, tile_z)) {
                WIRE_NEVER_CLEAN
            } else {
                match region_header(&region_path(region_folder, tile_x, tile_z)) {
                    None => STAMP_NO_REGION,
                    Some(header) => match header.iter().max() {
                        Some(&newest) if newest > 0 => i64::from(newest),
                        _ => WIRE_NEVER_CLEAN,
                    },
                }
            };
            write_zig_var_long(&mut out, stamp - previous);
            previous = stamp;
        }
    }
    Some(out)
}

pub fn write_zig_var_long(out: &mut Vec<u8>, value: i64) {
    let mut zig = ((value << 1) ^ (value >> 63)) as u64;
    while zig & !0x7F != 0 {
        out.push((zig & 0x7F) as u8 | 0x80);
        zig >>= 7;
    }
    out.push(zig as u8);
}

fn read_zig_var_long(input: &mut &[u8]) -> Option<i64> {
    let mut zig = 0u64;
    for shift in (0..70).step_by(7) {
        let (&byte, rest) = input.split_first()?;
        *input = rest;
        zig |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some((zig >> 1) as i64 ^ -((zig & 1) as i64));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zig_zag_round_trip() {
        for value in [0, 1, -1, 63, -64, 1_792_000_000, -2_000_000, i64::MAX, i64::MIN] {
            let mut bytes = Vec::new();
            write_zig_var_long(&mut bytes, value);
            assert_eq!(read_zig_var_long(&mut bytes.as_slice()), Some(value));
        }
        let mut bytes = Vec::new();
        write_zig_var_long(&mut bytes, -1);
        assert_eq!(bytes, [1]);
    }
}
