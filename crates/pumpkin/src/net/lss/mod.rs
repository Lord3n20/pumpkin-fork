//! Server side of the LOD Server Support protocol (Stierzunge fork), version 20.
//!
//! Clients with the LSS mod (Voxy) announce themselves on `lss:handshake_c2s` and then keep
//! declaring the far columns they still want on `lss:batch_chunk_req`. Every declared column is
//! answered once: with a `lss:voxel_column` frame when the chunk exists, or with a
//! `lss:batch_response` entry (not generated / up to date). Nothing is ever generated here;
//! columns come from loaded chunks or straight from the region files. Changed columns are
//! announced on `lss:dirty_columns`, reconnecting clients can validate their cache per region
//! (`lss:region_summary`), and players beyond entity range are sent as far players.
//!
//! Wire reference: github.com/VoX/lod-server-support v0.19.0 (`LSSConstants`,
//! `WireSectionCursor`, `SessionConfigS2CPayload`, `VoxelColumnS2CPayload`,
//! `RegionSummaryWire`, `FarPlayerWire`). Protocols 16/18/19 are not served: they only exist
//! in LSS releases for older Minecraft versions.

mod column;
mod far_players;
mod stamps;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

use futures::StreamExt;
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::math::vector2::Vector2;
use pumpkin_world::chunk::io::{FileIO, LoadedData};
use tokio::sync::Notify;
use tracing::{debug, info};
use uuid::Uuid;

use crate::entity::EntityBase;
use crate::entity::player::{JavaPlayer, Player};
use crate::server::Server;
use crate::world::World;
use column::EncodedColumn;

const PROTOCOL_VERSION: i32 = 20;
const CAPABILITY_ZSTD_COLUMNS: i32 = 2;
const CAPABILITY_FAR_PLAYERS: i32 = 4;
const CAPABILITY_MAP_POLICY: i32 = 8;
const RESPONSE_UP_TO_DATE: u8 = 1;
const RESPONSE_NOT_GENERATED: u8 = 2;
const MAX_BATCH_CHUNK_REQUESTS: usize = 1024;
const MAX_DIRTY_COLUMN_POSITIONS: usize = 10240;
const MAX_STAMP_ENTRIES: usize = 1024;
/// Extra ring the client may ask for beyond the LOD distance (`LOD_DISTANCE_BUFFER`).
const LOD_DISTANCE_BUFFER: i32 = 32;
/// A column re-declared this soon after we sent it is still on its way to the client.
const RESEND_GRACE: Duration = Duration::from_secs(2);

const CHANNELS: [&str; 13] = [
    "lss:handshake_c2s",
    "lss:batch_chunk_req",
    "lss:client_info",
    "lss:region_summary_req",
    far_players::CHANNEL_PREFS,
    "lss:session_config",
    "lss:voxel_column",
    "lss:batch_response",
    "lss:dirty_columns",
    "lss:region_summary",
    "lss:far_player_roster",
    "lss:far_player_updates",
    "lss:col_stamps",
];

static SESSIONS: LazyLock<Mutex<HashMap<Uuid, Arc<Session>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Set with the first session; until then block changes are not tracked at all.
static ACTIVE: AtomicBool = AtomicBool::new(false);

struct Session {
    player: Weak<Player>,
    capabilities: i32,
    /// Dimension the client was last told about.
    dimension: Mutex<&'static str>,
    /// The latest declared want-set (positions + client timestamps), newest replaces older.
    wants: Mutex<VecDeque<(i32, i32, i64)>>,
    /// Positions being read or recently sent, with the time they were taken.
    taken: Mutex<HashMap<(i32, i32), Instant>>,
    notify: Notify,
    far: Arc<Mutex<far_players::Viewer>>,
    /// A client that asked for region summaries also wants verification stamps.
    summaries: AtomicBool,
}

/// Entry point from the play packet handler for every `lss:*` custom payload.
pub fn handle_payload(server: &Arc<Server>, player: &Arc<Player>, channel: &str, data: &[u8]) {
    if !server.advanced_config.lod_server.enabled {
        return;
    }
    match channel {
        "lss:handshake_c2s" => handle_handshake(server, player, data),
        "lss:batch_chunk_req" => handle_request(player, data),
        "lss:region_summary_req" => {
            let Some(session) = session_of(player) else {
                return;
            };
            session.summaries.store(true, Ordering::Relaxed);
            let world = player.world();
            let request = data.to_vec();
            let player = player.clone();
            server.spawn_task(async move {
                let summary = tokio::task::spawn_blocking(move || {
                    stamps::region_summary(&world, &request)
                })
                .await
                .ok()
                .flatten();
                if let Some(summary) = summary {
                    JavaPlayer(&player)
                        .send_custom_payload("lss:region_summary", &summary)
                        .await;
                }
            });
        }
        far_players::CHANNEL_PREFS => {
            let session = session_of(player);
            far_players::on_prefs(player, session.as_ref().map(|s| s.far.as_ref()), data);
        }
        _ => {} // lss:client_info: diagnostics only
    }
}

/// Called from `World::flush_block_updates` with the positions that changed this tick.
pub fn record_block_changes<'a>(world: &World, positions: impl Iterator<Item = &'a BlockPos>) {
    if ACTIVE.load(Ordering::Relaxed) {
        stamps::record_changes(world, positions);
    }
}

fn session_of(player: &Player) -> Option<Arc<Session>> {
    SESSIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&player.gameprofile.id)
        .cloned()
}

fn handle_handshake(server: &Arc<Server>, player: &Arc<Player>, data: &[u8]) {
    let mut reader = data;
    let (Some(version), Some(capabilities)) = (read_var_int(&mut reader), read_var_int(&mut reader))
    else {
        return;
    };
    if version != PROTOCOL_VERSION {
        debug!(
            "LSS: {} announced protocol {version}, only {PROTOCOL_VERSION} is served",
            player.gameprofile.name
        );
        return;
    }

    let dimension = player.world().dimension.minecraft_name;
    let session = Arc::new(Session {
        player: Arc::downgrade(player),
        capabilities,
        dimension: Mutex::new(dimension),
        wants: Mutex::new(VecDeque::new()),
        taken: Mutex::new(HashMap::new()),
        notify: Notify::new(),
        far: Arc::new(Mutex::new(far_players::Viewer::new())),
        summaries: AtomicBool::new(false),
    });
    {
        let mut sessions = SESSIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions.retain(|_, s| s.player.strong_count() > 0);
        if let Some(old) = sessions.insert(player.gameprofile.id, session.clone()) {
            old.notify.notify_one(); // lets the old worker see it was replaced and stop
        }
    }
    if !ACTIVE.swap(true, Ordering::Relaxed) {
        let server_c = server.clone();
        server.spawn_task(run_ticker(server_c));
    }

    let player_c = player.clone();
    let server_c = server.clone();
    server.spawn_task(async move {
        let java = JavaPlayer(&player_c);
        // Fabric only lets the client send on channels the server registered
        java.send_custom_payload("minecraft:register", CHANNELS.join("\0").as_bytes())
            .await;
        java.send_custom_payload(
            "lss:session_config",
            &session_config(&server_c, session.capabilities),
        )
        .await;
        info!(
            "LSS: {} connected (LOD distance {} chunks)",
            player_c.gameprofile.name, server_c.advanced_config.lod_server.lod_distance_chunks
        );
        drop(player_c);
        run_session(server_c, session).await;
    });
}

fn session_config(server: &Server, capabilities: i32) -> Vec<u8> {
    let config = &server.advanced_config.lod_server;
    let mut out = Vec::with_capacity(16);
    write_var_int(&mut out, PROTOCOL_VERSION);
    out.push(1); // enabled
    write_var_int(&mut out, config.lod_distance_chunks as i32);
    out.push(0); // generation: only existing chunks are served
    write_var_int(&mut out, pumpkin_world::chunk::format::anvil::WORLD_DATA_VERSION);
    if capabilities & CAPABILITY_MAP_POLICY != 0 {
        write_var_int(&mut out, 0); // session flags: map ingestion allowed
    }
    out
}

fn handle_request(player: &Arc<Player>, data: &[u8]) {
    let Some(session) = session_of(player) else {
        return;
    };
    let mut reader = data;
    let Some(count) = read_var_int(&mut reader).and_then(|c| usize::try_from(c).ok()) else {
        return;
    };
    if count > MAX_BATCH_CHUNK_REQUESTS || reader.len() < count * 16 {
        return;
    }
    let mut wants = VecDeque::with_capacity(count);
    for entry in reader[..count * 16].chunks_exact(16) {
        let packed = i64::from_be_bytes(entry[..8].try_into().unwrap_or_default());
        let timestamp = i64::from_be_bytes(entry[8..].try_into().unwrap_or_default());
        wants.push_back(((packed >> 32) as i32, packed as i32, timestamp));
    }
    // The client re-declares everything it still wants on every scan, so the newest batch is
    // the whole truth: older undelivered wants are dropped instead of queueing up.
    *session
        .wants
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = wants;
    session.notify.notify_one();
}

enum Answer {
    Column(EncodedColumn),
    Response(u8),
}

async fn run_session(server: Arc<Server>, session: Arc<Session>) {
    let config = server.advanced_config.lod_server.clone();
    let zstd = session.capabilities & CAPABILITY_ZSTD_COLUMNS != 0;
    let mut budget = ByteBudget::new(config.max_bytes_per_second_per_player);
    loop {
        let Some(player) = session.player.upgrade() else {
            break;
        };
        let replaced = !session_of(&player).is_some_and(|s| Arc::ptr_eq(&s, &session));
        if replaced || player.client.closed() || crate::STOP_INTERRUPT.is_cancelled() {
            break;
        }

        let batch = take_batch(&session, &player, &config);
        if batch.is_empty() {
            drop(player);
            // Wake up now and then to notice disconnects even without new requests
            tokio::select! {
                _ = tokio::time::timeout(Duration::from_secs(5), session.notify.notified()) => {}
                () = crate::STOP_INTERRUPT.cancelled() => break,
            }
            continue;
        }

        let world = player.world();
        let dimension = world.dimension.minecraft_name;
        let mut responses = Vec::new();
        let mut answers = futures::stream::iter(batch)
            .map(|(x, z, timestamp)| {
                let world = world.clone();
                async move { (x, z, answer(world, x, z, timestamp).await) }
            })
            .buffer_unordered(config.concurrent_reads_per_player.max(1));
        let java = JavaPlayer(&player);
        while let Some((x, z, answer)) = answers.next().await {
            match answer {
                Answer::Column(column) => {
                    budget.spend(column.sections.len()).await;
                    let timestamp = stamps::now_seconds() as i64;
                    let frame = column::column_frame(x, z, dimension, timestamp, &column, zstd);
                    java.send_custom_payload("lss:voxel_column", &frame).await;
                }
                Answer::Response(kind) => responses.push((kind, x, z)),
            }
        }
        drop(answers);
        if !responses.is_empty() {
            let mut frame = Vec::with_capacity(5 + responses.len() * 9);
            write_var_int(&mut frame, responses.len() as i32);
            for &(kind, x, z) in &responses {
                frame.push(kind);
                frame.extend_from_slice(&pack_position(x, z).to_be_bytes());
            }
            java.send_custom_payload("lss:batch_response", &frame).await;
        }
        if session.summaries.load(Ordering::Relaxed) {
            let verified: Vec<i64> = responses
                .iter()
                .filter(|(kind, _, _)| *kind == RESPONSE_UP_TO_DATE)
                .map(|&(_, x, z)| pack_position(x, z))
                .collect();
            for chunk in verified.chunks(MAX_STAMP_ENTRIES) {
                java.send_custom_payload("lss:col_stamps", &column_stamps(dimension, chunk))
                    .await;
            }
        }
    }
    if let Some(player) = session.player.upgrade()
        && player.client.closed()
    {
        far_players::forget(&player.gameprofile.id);
    }
    debug!("LSS: session ended");
}

/// Takes the next positions to serve: in the client's order, skipping columns that are
/// out of range or still in flight.
fn take_batch(
    session: &Session,
    player: &Player,
    config: &pumpkin_config::lod_server::LodServerConfig,
) -> Vec<(i32, i32, i64)> {
    let center = player.get_entity().chunk_pos.load();
    let max_distance = config.lod_distance_chunks as i32 + LOD_DISTANCE_BUFFER;
    let limit = config.concurrent_reads_per_player.max(1) * 8;
    let now = Instant::now();
    let mut wants = session
        .wants
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut taken = session
        .taken
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    taken.retain(|_, at| now.duration_since(*at) < RESEND_GRACE);
    let mut batch = Vec::with_capacity(limit);
    while batch.len() < limit {
        let Some((x, z, timestamp)) = wants.pop_front() else {
            break;
        };
        if (x - center.x).abs().max((z - center.y).abs()) > max_distance
            || taken.contains_key(&(x, z))
        {
            continue;
        }
        taken.insert((x, z), now);
        batch.push((x, z, timestamp));
    }
    batch
}

async fn answer(world: Arc<World>, x: i32, z: i32, client_timestamp: i64) -> Answer {
    let pos = Vector2::new(x, z);
    let dimension = world.dimension.minecraft_name;
    let region_folder = world.level.level_folder.region_folder.clone();
    let loaded = world.level.is_chunk_loaded(&pos);

    // A client re-declaring with its cache stamp only needs the column if it changed since
    let (stamp, saved) = tokio::task::spawn_blocking(move || {
        (
            stamps::column_stamp(dimension, &region_folder, x, z),
            stamps::saved_stamp(&region_folder, x, z),
        )
    })
    .await
    .unwrap_or((u64::MAX, 0));
    if client_timestamp > 0 && (client_timestamp as u64) > stamp && (loaded || saved > 0) {
        return Answer::Response(RESPONSE_UP_TO_DATE);
    }

    // A loaded chunk is the freshest source
    if let Some(encoded) = world.level.read_chunk_sync(&pos, |chunk| {
        column::encode_column(chunk, column::COLUMN_SOURCE_IN_MEMORY)
    }) {
        return encoded.map_or(Answer::Response(RESPONSE_NOT_GENERATED), Answer::Column);
    }
    if saved == 0 {
        return Answer::Response(RESPONSE_NOT_GENERATED);
    }

    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let positions = [pos];
    world
        .level
        .chunk_saver
        .fetch_chunks(&world.level.level_folder, &positions, sender)
        .await;
    match receiver.recv().await {
        Some(LoadedData::Loaded(chunk)) => tokio::task::spawn_blocking(move || {
            column::encode_column(&chunk, column::COLUMN_SOURCE_DISK)
        })
        .await
        .ok()
        .flatten()
        .map_or(Answer::Response(RESPONSE_NOT_GENERATED), Answer::Column),
        _ => Answer::Response(RESPONSE_NOT_GENERATED),
    }
}

/// Background loop for all sessions: far players every 10 ticks, dirty columns and
/// dimension changes every few seconds.
async fn run_ticker(server: Arc<Server>) {
    let config = server.advanced_config.lod_server.clone();
    let tick = Duration::from_millis(50 * far_players::UPDATE_INTERVAL_TICKS as u64);
    let dirty_every = (config.dirty_broadcast_seconds.max(1) * 1000 / tick.as_millis() as u64).max(1);
    let mut interval = tokio::time::interval(tick);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut round = 0u64;
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            () = crate::STOP_INTERRUPT.cancelled() => break,
        }
        round += 1;
        let sessions: Vec<(Arc<Player>, Arc<Session>)> = SESSIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter_map(|s| s.player.upgrade().map(|p| (p, s.clone())))
            .filter(|(p, _)| !p.client.closed())
            .collect();

        let viewers: Vec<_> = sessions
            .iter()
            .filter(|(_, s)| s.capabilities & CAPABILITY_FAR_PLAYERS != 0)
            .map(|(p, s)| (p.clone(), s.far.clone()))
            .collect();
        if !viewers.is_empty() {
            far_players::broadcast(
                &viewers,
                &server.get_all_players(),
                config.far_players_max_distance_blocks,
            )
            .await;
        }

        if round % dirty_every == 0 {
            let dirty = stamps::take_dirty();
            for (player, session) in &sessions {
                let dimension = player.world().dimension.minecraft_name;
                let changed_dimension = {
                    let mut known = session
                        .dimension
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    std::mem::replace(&mut *known, dimension) != dimension
                };
                let java = JavaPlayer(player);
                if changed_dimension {
                    // Like LSS servers: a new session config makes the client start over
                    session
                        .wants
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clear();
                    java.send_custom_payload(
                        "lss:session_config",
                        &session_config(&server, session.capabilities),
                    )
                    .await;
                    continue;
                }
                let Some(columns) = dirty.get(dimension) else {
                    continue;
                };
                let center = player.get_entity().chunk_pos.load();
                let range = config.lod_distance_chunks as i32 + LOD_DISTANCE_BUFFER;
                let positions: Vec<i64> = columns
                    .iter()
                    .filter(|(x, z)| (x - center.x).abs().max((z - center.y).abs()) <= range)
                    .take(MAX_DIRTY_COLUMN_POSITIONS)
                    .map(|&(x, z)| pack_position(x, z))
                    .collect();
                if positions.is_empty() {
                    continue;
                }
                let mut frame = Vec::with_capacity(5 + positions.len() * 8);
                write_var_int(&mut frame, positions.len() as i32);
                for position in positions {
                    frame.extend_from_slice(&position.to_be_bytes());
                }
                java.send_custom_payload("lss:dirty_columns", &frame).await;
            }
        }
    }
}

/// `lss:col_stamps`: the columns just answered up to date were verified now, so the client
/// can move their cache stamps forward and later region summaries validate them.
fn column_stamps(dimension: &str, positions: &[i64]) -> Vec<u8> {
    let now = stamps::now_seconds() as i64;
    let mut out = Vec::with_capacity(32 + positions.len() * 9);
    out.push(1); // version
    write_var_int(&mut out, dimension.len() as i32);
    out.extend_from_slice(dimension.as_bytes());
    stamps::write_zig_var_long(&mut out, now);
    write_var_int(&mut out, positions.len() as i32);
    for position in positions {
        out.extend_from_slice(&position.to_be_bytes());
        stamps::write_zig_var_long(&mut out, 0);
    }
    out
}

const fn pack_position(x: i32, z: i32) -> i64 {
    ((x as i64) << 32) | (z as u32 as i64)
}

/// Spreads column sends so one player gets at most `per_second` raw bytes per second.
struct ByteBudget {
    per_second: u64,
    window_start: Instant,
    spent: u64,
}

impl ByteBudget {
    fn new(per_second: u64) -> Self {
        Self {
            per_second: per_second.max(1024),
            window_start: Instant::now(),
            spent: 0,
        }
    }

    async fn spend(&mut self, bytes: usize) {
        let elapsed = self.window_start.elapsed();
        if elapsed >= Duration::from_secs(1) {
            self.window_start = Instant::now();
            self.spent = 0;
        } else if self.spent >= self.per_second {
            tokio::time::sleep(Duration::from_secs(1) - elapsed).await;
            self.window_start = Instant::now();
            self.spent = 0;
        }
        self.spent += bytes as u64;
    }
}

fn write_var_int(out: &mut Vec<u8>, value: i32) {
    let mut value = value as u32;
    while value & !0x7F != 0 {
        out.push((value & 0x7F) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn read_var_int(input: &mut &[u8]) -> Option<i32> {
    let mut value = 0u32;
    for shift in (0..35).step_by(7) {
        let (&byte, rest) = input.split_first()?;
        *input = rest;
        value |= u32::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some(value as i32);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn var_int_round_trip() {
        for value in [0, 1, 127, 128, 255, 25_565, 2_097_151, i32::MAX, -1, i32::MIN] {
            let mut bytes = Vec::new();
            write_var_int(&mut bytes, value);
            assert_eq!(read_var_int(&mut bytes.as_slice()), Some(value));
        }
    }

    #[test]
    fn packed_positions_match_lss() {
        assert_eq!(pack_position(1, 2), (1i64 << 32) | 2);
        let packed = pack_position(-5, -7);
        assert_eq!(((packed >> 32) as i32, packed as i32), (-5, -7));
    }
}
