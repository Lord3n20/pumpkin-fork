//! Anti-xray (Stierzunge fork), like Paper's engine-mode 1: ores that touch no transparent
//! block are sent to clients as the surrounding stone type. When blocks change, hidden ores
//! within [`UPDATE_RADIUS`] are sent with their real state again.

use std::sync::OnceLock;

use pumpkin_data::{Block, BlockState, BlockStateId};
use pumpkin_util::math::position::BlockPos;
use tracing::{info, warn};

use crate::chunk::palette::BlockPalette;

/// Blocks around a change whose real state is revealed (Paper's default `update-radius`).
pub const UPDATE_RADIUS: i32 = 2;

pub struct AntiXray {
    /// Replacement state for every hidden state id; `None` for states that are not hidden.
    replacement: Box<[Option<BlockStateId>]>,
    /// Whether a hidden state only gets hidden below `max_height` (Overworld ores).
    height_limited: Box<[bool]>,
    max_height: i32,
}

static ANTI_XRAY: OnceLock<AntiXray> = OnceLock::new();

/// Sets up anti-xray once at startup. Does nothing when disabled.
pub fn configure(enabled: bool, max_height: i32, hidden_blocks: &[String]) {
    if !enabled {
        return;
    }
    let count = BlockStateId::COUNT as usize;
    let mut replacement = vec![None; count].into_boxed_slice();
    let mut height_limited = vec![false; count].into_boxed_slice();
    let mut hidden = 0;
    for name in hidden_blocks {
        let name = name.strip_prefix("minecraft:").unwrap_or(name);
        let Some(block) = Block::from_name(name) else {
            warn!("anti-xray: unknown block '{name}', ignored");
            continue;
        };
        let (stone, limited) = if name.starts_with("deepslate_") {
            (Block::DEEPSLATE, true)
        } else if name.starts_with("nether_") || name == "ancient_debris" {
            (Block::NETHERRACK, false)
        } else {
            (Block::STONE, true)
        };
        for state in block.states {
            let id = state.id.as_u16() as usize;
            replacement[id] = Some(stone.default_state.id);
            height_limited[id] = limited;
        }
        hidden += 1;
    }
    info!("anti-xray enabled for {hidden} block types");
    let _ = ANTI_XRAY.set(AntiXray {
        replacement,
        height_limited,
        max_height,
    });
}

/// The active anti-xray settings, if enabled.
#[must_use]
pub fn get() -> Option<&'static AntiXray> {
    ANTI_XRAY.get()
}

#[inline]
fn is_transparent(id: BlockStateId) -> bool {
    !BlockState::from_id(id).is_solid_render()
}

impl AntiXray {
    /// Whether this block type is one that anti-xray hides.
    #[inline]
    #[must_use]
    pub fn is_hidden_type(&self, id: BlockStateId) -> bool {
        self.replacement[id.as_u16() as usize].is_some()
    }

    #[inline]
    fn replacement_at(&self, id: BlockStateId, y: i32) -> Option<BlockStateId> {
        let index = id.as_u16() as usize;
        let replacement = self.replacement[index]?;
        if self.height_limited[index] && y >= self.max_height {
            return None;
        }
        Some(replacement)
    }

    /// Returns obfuscated copies of the sections, or `None` when nothing needs hiding.
    /// Neighbours outside this chunk count as solid, so ores at chunk borders stay hidden
    /// until a nearby block change reveals them.
    #[must_use]
    pub fn obfuscate(&self, sections: &[BlockPalette], min_y: i32) -> Option<Vec<BlockPalette>> {
        let any_hidden = sections.iter().any(|section| {
            section
                .palette_values()
                .iter()
                .any(|&id| self.is_hidden_type(id))
        });
        if !any_hidden {
            return None;
        }

        let height = sections.len() as i32 * 16;
        let get = |x: i32, y: i32, z: i32| -> Option<BlockStateId> {
            if !(0..16).contains(&x) || !(0..16).contains(&z) || !(0..height).contains(&y) {
                return None;
            }
            Some(sections[(y / 16) as usize].get(x as usize, (y % 16) as usize, z as usize))
        };
        let exposed = |x: i32, y: i32, z: i32| -> bool {
            [
                (1, 0, 0),
                (-1, 0, 0),
                (0, 1, 0),
                (0, -1, 0),
                (0, 0, 1),
                (0, 0, -1),
            ]
            .iter()
            .any(|&(dx, dy, dz)| get(x + dx, y + dy, z + dz).is_some_and(is_transparent))
        };

        let mut out = sections.to_vec();
        for (index, section) in sections.iter().enumerate() {
            if !section
                .palette_values()
                .iter()
                .any(|&id| self.is_hidden_type(id))
            {
                continue;
            }
            let base_y = index as i32 * 16;
            for ly in 0..16 {
                let y = base_y + ly as i32;
                for lz in 0..16 {
                    for lx in 0..16 {
                        let id = section.get(lx, ly, lz);
                        let Some(replacement) = self.replacement_at(id, min_y + y) else {
                            continue;
                        };
                        if !exposed(lx as i32, y, lz as i32) {
                            out[index].set(lx, ly, lz, replacement);
                        }
                    }
                }
            }
        }
        Some(out)
    }

    /// Positions around a changed block that may hold hidden blocks and have to be
    /// resent with their real state.
    pub fn reveal_positions(pos: BlockPos) -> impl Iterator<Item = BlockPos> {
        let r = UPDATE_RADIUS;
        (-r..=r).flat_map(move |dx| {
            (-r..=r).flat_map(move |dy| {
                (-r..=r).filter_map(move |dz| {
                    if dx == 0 && dy == 0 && dz == 0 {
                        None
                    } else {
                        Some(BlockPos::new(pos.0.x + dx, pos.0.y + dy, pos.0.z + dz))
                    }
                })
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anti_xray() -> AntiXray {
        let count = BlockStateId::COUNT as usize;
        let mut replacement = vec![None; count].into_boxed_slice();
        let mut height_limited = vec![false; count].into_boxed_slice();
        let ore = Block::DIAMOND_ORE.default_state.id.as_u16() as usize;
        replacement[ore] = Some(Block::STONE.default_state.id);
        height_limited[ore] = true;
        AntiXray {
            replacement,
            height_limited,
            max_height: 64,
        }
    }

    fn stone_section() -> BlockPalette {
        BlockPalette::Homogeneous(Block::STONE.default_state.id)
    }

    #[test]
    fn hides_enclosed_ore_and_keeps_exposed_ore() {
        let ax = anti_xray();
        let ore = Block::DIAMOND_ORE.default_state.id;
        let mut section = stone_section();
        section.set(5, 5, 5, ore); // enclosed
        section.set(8, 5, 8, ore); // exposed: air above
        section.set(8, 6, 8, Block::AIR.default_state.id);
        let sections = vec![section];

        let out = ax.obfuscate(&sections, 0).expect("chunk has ores");
        assert_eq!(out[0].get(5, 5, 5), Block::STONE.default_state.id);
        assert_eq!(out[0].get(8, 5, 8), ore);
        // Server-side data is untouched
        assert_eq!(sections[0].get(5, 5, 5), ore);
    }

    #[test]
    fn keeps_ores_above_max_height() {
        let ax = anti_xray();
        let ore = Block::DIAMOND_ORE.default_state.id;
        let mut section = stone_section();
        section.set(5, 5, 5, ore);
        // Section starts at y = 100, above max_height 64
        let out = ax.obfuscate(&[section], 100).expect("chunk has ores");
        assert_eq!(out[0].get(5, 5, 5), ore);
    }

    #[test]
    fn no_copy_without_ores() {
        let ax = anti_xray();
        assert!(ax.obfuscate(&[stone_section()], 0).is_none());
    }

    #[test]
    fn reveal_positions_cover_radius_without_center() {
        let positions: Vec<_> = AntiXray::reveal_positions(BlockPos::new(0, 0, 0)).collect();
        assert_eq!(positions.len(), 124);
        assert!(!positions.contains(&BlockPos::new(0, 0, 0)));
    }
}
