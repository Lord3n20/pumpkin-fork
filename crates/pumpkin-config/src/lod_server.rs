use serde::{Deserialize, Serialize};

/// Serves far terrain to clients running the LOD Server Support mod (Voxy, Distant Horizons
/// via its adapter). Only chunks that already exist on disk are served; nothing is generated.
#[derive(Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct LodServerConfig {
    /// Master switch.
    pub enabled: bool,
    /// Radius in chunks that clients may request.
    pub lod_distance_chunks: u32,
    /// Upper bound for the uncompressed column bytes sent to one player per second.
    pub max_bytes_per_second_per_player: u64,
    /// Columns read and encoded at the same time for one player.
    pub concurrent_reads_per_player: usize,
    /// How often clients are told which columns changed, in seconds.
    pub dirty_broadcast_seconds: u64,
    /// Players up to this many blocks away are shown to LSS clients as far players (0 = off).
    pub far_players_max_distance_blocks: u32,
}

impl Default for LodServerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            lod_distance_chunks: 256,
            max_bytes_per_second_per_player: 8 * 1024 * 1024,
            concurrent_reads_per_player: 4,
            dirty_broadcast_seconds: 5,
            far_players_max_distance_blocks: 2048,
        }
    }
}
