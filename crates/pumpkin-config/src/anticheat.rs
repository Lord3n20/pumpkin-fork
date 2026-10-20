use serde::{Deserialize, Serialize};

/// Light movement checks for players that are not allowed to fly.
#[derive(Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct AntiCheatConfig {
    /// Master switch for all checks below.
    pub enabled: bool,
    /// Sets players back when a single movement packet covers more than
    /// `max_move_per_packet` blocks (vanilla "moved too quickly").
    pub check_speed: bool,
    /// Maximum distance in blocks per movement packet (vanilla: 10, with elytra 3x that).
    pub max_move_per_packet: f64,
    /// Kicks players that hover in the air without anything around them
    /// (vanilla "Flying is not enabled on this server").
    pub check_flying: bool,
    /// Movement packets in the air without falling before the fly kick (vanilla: 80 ticks).
    pub max_floating_packets: u32,
}

impl Default for AntiCheatConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check_speed: true,
            max_move_per_packet: 10.0,
            check_flying: true,
            max_floating_packets: 80,
        }
    }
}

/// Hides ores that are not exposed to air or fluids from clients (like Paper's engine-mode 1).
#[derive(Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct AntiXrayConfig {
    /// Master switch.
    pub enabled: bool,
    /// Only blocks below this Y level are hidden in the Overworld (the Nether and End are
    /// hidden over their full height).
    pub max_block_height: i32,
    /// Blocks (without the `minecraft:` prefix) that are replaced by the surrounding stone
    /// type unless a neighbour is transparent. Blocks with block entities (chests, spawners)
    /// do not belong here: their block entity data would not match the replacement.
    pub hidden_blocks: Vec<String>,
}

impl Default for AntiXrayConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_block_height: 64,
            hidden_blocks: [
                "coal_ore",
                "deepslate_coal_ore",
                "copper_ore",
                "deepslate_copper_ore",
                "iron_ore",
                "deepslate_iron_ore",
                "gold_ore",
                "deepslate_gold_ore",
                "redstone_ore",
                "deepslate_redstone_ore",
                "lapis_ore",
                "deepslate_lapis_ore",
                "diamond_ore",
                "deepslate_diamond_ore",
                "emerald_ore",
                "deepslate_emerald_ore",
                "raw_iron_block",
                "raw_copper_block",
                "raw_gold_block",
                "nether_gold_ore",
                "nether_quartz_ore",
                "ancient_debris",
                "amethyst_cluster",
                "budding_amethyst",
            ]
            .map(String::from)
            .to_vec(),
        }
    }
}
