//! Far players: positions of players beyond the normal entity range, for clients that
//! declared `CAPABILITY_FAR_PLAYERS` (LSS `FarPlayerWire` / `FarPlayerBroadcastService`).
//! Equipment and vehicles are not sent; the client then shows plain player models.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use pumpkin_util::math::vector3::Vector3;
use uuid::Uuid;

use super::write_var_int;
use crate::entity::EntityBase;
use crate::entity::player::Player;

pub const CHANNEL_PREFS: &str = "lss:far_player_prefs";
const CHANNEL_ROSTER: &str = "lss:far_player_roster";
const CHANNEL_UPDATES: &str = "lss:far_player_updates";
pub const UPDATE_INTERVAL_TICKS: i32 = 10;
const FULL_ROSTER_MIN_INTERVAL: Duration = Duration::from_secs(5);
const MAX_ENTRIES: usize = 1024;
const POS_SCALE: f64 = 16.0;
const VEL_SCALE: f64 = 256.0;
const MAX_VELOCITY: f64 = 64.0;
const POSE_SNEAK: u8 = 1;
const POSE_GLIDE: u8 = 2;
const POSE_SWIM: u8 = 4;

#[derive(Clone, Copy)]
pub struct Prefs {
    enabled: bool,
    max_distance: u32,
    min_distance: u32,
    share_self: bool,
    share_distance: u32,
}

/// Prefs of every player that sent some, also players without a viewer session: their
/// `share_self` decides whether others see them.
static PREFS: LazyLock<Mutex<HashMap<Uuid, Prefs>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Last broadcast position per player, for velocities.
static LAST_POSITIONS: LazyLock<Mutex<HashMap<Uuid, (Vector3<f64>, Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Default, PartialEq, Eq, Clone, Copy)]
struct Row {
    quant: [i32; 3],
    angles: [u8; 3],
    pose: u8,
    moving: bool,
}

pub struct Viewer {
    epoch: i32,
    index_by_uuid: HashMap<Uuid, i32>,
    next_index: i32,
    rows: HashMap<Uuid, Row>,
    full_pending: bool,
    last_full: Option<Instant>,
    dimension: &'static str,
}

impl Viewer {
    pub fn new() -> Self {
        Self {
            epoch: 0,
            index_by_uuid: HashMap::new(),
            next_index: 0,
            rows: HashMap::new(),
            full_pending: true,
            last_full: None,
            dimension: "",
        }
    }
}

/// Handles `lss:far_player_prefs`: `enabled:u8 max:varint min:varint share_self:u8 share:varint`.
pub fn on_prefs(player: &Player, viewer: Option<&Mutex<Viewer>>, data: &[u8]) {
    let mut reader = data;
    let parse = |reader: &mut &[u8]| -> Option<Prefs> {
        let (&enabled, rest) = reader.split_first()?;
        *reader = rest;
        let max_distance = u32::try_from(super::read_var_int(reader)?).ok()?;
        let min_distance = u32::try_from(super::read_var_int(reader)?).ok()?;
        let (&share_self, rest) = reader.split_first()?;
        *reader = rest;
        let share_distance = u32::try_from(super::read_var_int(reader)?).ok()?;
        reader.is_empty().then_some(Prefs {
            enabled: enabled != 0,
            max_distance,
            min_distance,
            share_self: share_self != 0,
            share_distance,
        })
    };
    let Some(prefs) = parse(&mut reader) else {
        return;
    };
    PREFS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(player.gameprofile.id, prefs);
    if let Some(viewer) = viewer {
        viewer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .full_pending = true;
    }
}

pub fn forget(uuid: &Uuid) {
    PREFS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(uuid);
}

struct Snapshot {
    player: Arc<Player>,
    dimension: &'static str,
    pos: Vector3<f64>,
    row: Row,
    velocity: [i16; 3],
    visible_to_others: bool,
}

/// Snapshots of all online players; call once per broadcast round.
fn snapshots(players: &[Arc<Player>]) -> Vec<Snapshot> {
    let now = Instant::now();
    let mut last = LAST_POSITIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let snapshots: Vec<Snapshot> = players
        .iter()
        .map(|player| {
            let entity = player.get_entity();
            let pos = entity.pos.load();
            let velocity = last
                .get(&player.gameprofile.id)
                .map_or([0; 3], |(old, at)| {
                    let seconds = now.duration_since(*at).as_secs_f64().max(0.05);
                    [
                        quantize_velocity((pos.x - old.x) / seconds),
                        quantize_velocity((pos.y - old.y) / seconds),
                        quantize_velocity((pos.z - old.z) / seconds),
                    ]
                });
            last.insert(player.gameprofile.id, (pos, now));
            let mut pose = 0;
            if entity.is_sneaking() {
                pose |= POSE_SNEAK;
            }
            if entity.is_fall_flying() {
                pose |= POSE_GLIDE;
            }
            if entity.is_swimming() {
                pose |= POSE_SWIM;
            }
            Snapshot {
                player: player.clone(),
                dimension: player.world().dimension.minecraft_name,
                pos,
                row: Row {
                    quant: [quantize(pos.x), quantize(pos.y), quantize(pos.z)],
                    angles: [
                        angle_byte(entity.yaw.load()),
                        angle_byte(entity.head_yaw.load()),
                        angle_byte(entity.pitch.load()),
                    ],
                    pose,
                    moving: velocity != [0; 3],
                },
                velocity,
                visible_to_others: !player.is_spectator()
                    && !entity.invisible.load(std::sync::atomic::Ordering::Relaxed)
                    && !player
                        .living_entity
                        .dead
                        .load(std::sync::atomic::Ordering::Relaxed),
            }
        })
        .collect();
    last.retain(|uuid, _| players.iter().any(|p| p.gameprofile.id == *uuid));
    snapshots
}

/// One broadcast round for all far-player viewers.
pub async fn broadcast(viewers: &[(Arc<Player>, Arc<Mutex<Viewer>>)], all_players: &[Arc<Player>], max_distance: u32) {
    if viewers.is_empty() || max_distance == 0 {
        return;
    }
    let snapshots = snapshots(all_players);
    let prefs = PREFS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let now = Instant::now();
    for (player, viewer) in viewers {
        let Some(me) = snapshots
            .iter()
            .find(|s| s.player.gameprofile.id == player.gameprofile.id)
        else {
            continue;
        };
        let Some(my_prefs) = prefs.get(&player.gameprofile.id).filter(|p| p.enabled) else {
            continue;
        };
        let mut max = max_distance;
        if my_prefs.max_distance > 0 {
            max = max.min(my_prefs.max_distance);
        }
        let min = my_prefs.min_distance;
        let mut visible: Vec<&Snapshot> = snapshots
            .iter()
            .filter(|t| {
                let dx = t.pos.x - me.pos.x;
                let dz = t.pos.z - me.pos.z;
                let distance_sq = dx * dx + dz * dz;
                let target_prefs = prefs.get(&t.player.gameprofile.id);
                t.player.gameprofile.id != player.gameprofile.id
                    && t.dimension == me.dimension
                    && t.visible_to_others
                    && distance_sq <= f64::from(max).powi(2)
                    && (min == 0 || distance_sq >= f64::from(min).powi(2))
                    && target_prefs.is_none_or(|p| {
                        p.share_self
                            && (p.share_distance == 0
                                || distance_sq <= f64::from(p.share_distance).powi(2))
                    })
            })
            .collect();
        if visible.len() > MAX_ENTRIES {
            visible.sort_by(|a, b| {
                let da = (a.pos.x - me.pos.x).powi(2) + (a.pos.z - me.pos.z).powi(2);
                let db = (b.pos.x - me.pos.x).powi(2) + (b.pos.z - me.pos.z).powi(2);
                da.total_cmp(&db)
            });
            visible.truncate(MAX_ENTRIES);
        }

        let (roster, updates) = {
            let mut state = viewer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.dimension != me.dimension {
                state.dimension = me.dimension;
                state.full_pending = true;
            }
            frames(&mut state, &visible, me.dimension, now)
        };
        let java = crate::entity::player::JavaPlayer(player);
        if let Some(roster) = roster {
            java.send_custom_payload(CHANNEL_ROSTER, &roster).await;
        }
        if let Some(updates) = updates {
            java.send_custom_payload(CHANNEL_UPDATES, &updates).await;
        }
    }
}

/// Builds this round's roster diff and update frame and commits them to the viewer state.
fn frames(
    state: &mut Viewer,
    visible: &[&Snapshot],
    dimension: &str,
    now: Instant,
) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let mut roster = None;
    let full_allowed = state
        .last_full
        .is_none_or(|at| now.duration_since(at) >= FULL_ROSTER_MIN_INTERVAL);
    if state.full_pending && full_allowed {
        state.epoch += 1;
        state.index_by_uuid.clear();
        state.rows.clear();
        state.next_index = 0;
        let mut added = Vec::new();
        for target in visible {
            let index = state.next_index;
            state.next_index += 1;
            state.index_by_uuid.insert(target.player.gameprofile.id, index);
            added.push((index, *target));
        }
        roster = Some(encode_roster(state.epoch, true, &added, &[]));
        state.full_pending = false;
        state.last_full = Some(now);
    } else {
        let mut added = Vec::new();
        for target in visible {
            if !state.index_by_uuid.contains_key(&target.player.gameprofile.id) {
                let index = state.next_index;
                state.next_index += 1;
                state.index_by_uuid.insert(target.player.gameprofile.id, index);
                added.push((index, *target));
            }
        }
        let gone: Vec<Uuid> = state
            .index_by_uuid
            .keys()
            .filter(|uuid| !visible.iter().any(|t| t.player.gameprofile.id == **uuid))
            .copied()
            .collect();
        let mut removed = Vec::with_capacity(gone.len());
        for uuid in gone {
            if let Some(index) = state.index_by_uuid.remove(&uuid) {
                removed.push(index);
            }
            state.rows.remove(&uuid);
        }
        if !added.is_empty() || !removed.is_empty() {
            roster = Some(encode_roster(state.epoch, false, &added, &removed));
        }
    }

    let mut entries = Vec::new();
    for target in visible {
        let uuid = target.player.gameprofile.id;
        let Some(&index) = state.index_by_uuid.get(&uuid) else {
            continue;
        };
        let previous = state.rows.get(&uuid).copied();
        let stopped = previous.is_some_and(|row| row.moving) && !target.row.moving;
        if previous.is_some_and(|row| row == target.row) && !stopped {
            continue;
        }
        state.rows.insert(uuid, target.row);
        entries.push((index, *target));
    }
    let updates = (!entries.is_empty()).then(|| encode_updates(state.epoch, dimension, &entries));
    (roster, updates)
}

fn encode_roster(epoch: i32, full: bool, added: &[(i32, &Snapshot)], removed: &[i32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + added.len() * 40);
    write_var_int(&mut out, epoch);
    out.push(u8::from(full));
    write_var_int(&mut out, added.len() as i32);
    for (index, target) in added {
        write_var_int(&mut out, *index);
        out.extend_from_slice(target.player.gameprofile.id.as_bytes());
        let name = target.player.gameprofile.name.as_bytes();
        write_var_int(&mut out, name.len() as i32);
        out.extend_from_slice(name);
    }
    write_var_int(&mut out, removed.len() as i32);
    for index in removed {
        write_var_int(&mut out, *index);
    }
    out
}

fn encode_updates(epoch: i32, dimension: &str, entries: &[(i32, &Snapshot)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + entries.len() * 32);
    write_var_int(&mut out, epoch);
    write_var_int(&mut out, dimension.len() as i32);
    out.extend_from_slice(dimension.as_bytes());
    write_var_int(&mut out, UPDATE_INTERVAL_TICKS);
    write_var_int(&mut out, 0); // identity dictionary: no equipment or vehicles
    write_var_int(&mut out, entries.len() as i32);
    for (index, target) in entries {
        write_var_int(&mut out, *index);
        out.push(0); // presence: neither equipment nor vehicle
        for quant in target.row.quant {
            out.extend_from_slice(&quant.to_be_bytes());
        }
        out.extend_from_slice(&target.row.angles);
        out.push(target.row.pose);
        for velocity in target.velocity {
            out.extend_from_slice(&velocity.to_be_bytes());
        }
    }
    out
}

fn quantize(blocks: f64) -> i32 {
    (blocks * POS_SCALE).round() as i32
}

fn quantize_velocity(blocks_per_second: f64) -> i16 {
    (blocks_per_second.clamp(-MAX_VELOCITY, MAX_VELOCITY) * VEL_SCALE).round() as i16
}

fn angle_byte(degrees: f32) -> u8 {
    (f64::from(degrees) * 256.0 / 360.0).floor() as i64 as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn angles_wrap_like_java() {
        assert_eq!(angle_byte(0.0), 0);
        assert_eq!(angle_byte(90.0), 64);
        assert_eq!(angle_byte(-90.0), 192);
        assert_eq!(angle_byte(180.0) as i8, -128);
    }

    #[test]
    fn velocity_is_clamped() {
        assert_eq!(quantize_velocity(1.0), 256);
        assert_eq!(quantize_velocity(1000.0), (64.0 * 256.0) as i16);
    }
}
