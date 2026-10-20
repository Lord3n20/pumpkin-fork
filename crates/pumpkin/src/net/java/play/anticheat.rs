//! Light movement checks (Stierzunge fork), modelled on vanilla's
//! "moved too quickly" set-back and "Flying is not enabled" kick.

#[allow(clippy::wildcard_imports)]
use super::*;
use pumpkin_data::BlockState;
use pumpkin_data::effect::StatusEffect;

pub enum MoveVerdict {
    Allow,
    /// Teleport the player back to the last accepted position.
    SetBack,
    /// Kick with the vanilla flying message.
    KickFlying,
}

impl JavaClient {
    pub(super) fn check_movement(
        &self,
        player: &Arc<Player>,
        server: &Server,
        last_pos: Vector3<f64>,
        new_pos: Vector3<f64>,
        on_ground: bool,
    ) -> MoveVerdict {
        let config = &server.advanced_config.anticheat;
        if !config.enabled {
            return MoveVerdict::Allow;
        }
        let gamemode = player.gamemode.load();
        let may_fly = player
            .abilities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .allow_flying;
        if may_fly || matches!(gamemode, GameMode::Creative | GameMode::Spectator) {
            self.floating_packets.store(0, Ordering::Relaxed);
            return MoveVerdict::Allow;
        }
        let entity = player.get_entity();
        let gliding = entity.is_fall_flying();

        if config.check_speed {
            // Like vanilla, allow for velocity the server applied itself (knockback, explosions)
            let velocity = entity.velocity.load();
            let mut allowed = config.max_move_per_packet;
            if gliding {
                allowed *= 3.0;
            }
            let allowed = allowed + velocity.length();
            if last_pos.squared_distance_to_vec(&new_pos) > allowed * allowed {
                warn!(
                    "{} moved too quickly ({:.1} blocks), setting back",
                    player.gameprofile.name,
                    last_pos.squared_distance_to_vec(&new_pos).sqrt()
                );
                return MoveVerdict::SetBack;
            }
        }

        if config.check_flying {
            let floating = !on_ground
                && !gliding
                && new_pos.y - last_pos.y >= -0.03125
                && !player.living_entity.has_effect(&StatusEffect::LEVITATION)
                && !player.living_entity.has_effect(&StatusEffect::SLOW_FALLING)
                && !entity.is_in_water()
                && no_blocks_around(&player.world(), new_pos);
            if floating {
                let count = self.floating_packets.fetch_add(1, Ordering::Relaxed) + 1;
                if count > config.max_floating_packets {
                    warn!("{} was floating too long, kicking", player.gameprofile.name);
                    self.floating_packets.store(0, Ordering::Relaxed);
                    return MoveVerdict::KickFlying;
                }
            } else {
                self.floating_packets.store(0, Ordering::Relaxed);
            }
        }
        MoveVerdict::Allow
    }

    /// Applies a verdict. Returns true when the movement must not be processed.
    pub(super) fn enforce_movement(&self, player: &Arc<Player>, verdict: MoveVerdict) -> bool {
        match verdict {
            MoveVerdict::Allow => false,
            MoveVerdict::SetBack => {
                self.force_tp(player, player.get_entity().pos.load());
                true
            }
            MoveVerdict::KickFlying => {
                self.try_kick(&TextComponent::translate_cross(
                    translation::java::MULTIPLAYER_DISCONNECT_FLYING,
                    translation::java::MULTIPLAYER_DISCONNECT_FLYING,
                    [],
                ));
                true
            }
        }
    }
}

/// True when no block (anything but air) touches the player's slightly enlarged bounding box,
/// the same idea as vanilla's `noBlocksAround`. Unloaded blocks count as present.
fn no_blocks_around(world: &World, feet: Vector3<f64>) -> bool {
    const HALF_WIDTH: f64 = 0.3 + 0.0625;
    let min_x = (feet.x - HALF_WIDTH).floor() as i32;
    let max_x = (feet.x + HALF_WIDTH).floor() as i32;
    let min_z = (feet.z - HALF_WIDTH).floor() as i32;
    let max_z = (feet.z + HALF_WIDTH).floor() as i32;
    let min_y = (feet.y - 0.55).floor() as i32;
    let max_y = (feet.y + 1.8 + 0.0625).floor() as i32;
    for x in min_x..=max_x {
        for y in min_y..=max_y {
            for z in min_z..=max_z {
                match world.get_block_state_id_if_loaded(&BlockPos::new(x, y, z)) {
                    Some(id) if BlockState::from_id(id).is_air() => {}
                    _ => return false,
                }
            }
        }
    }
    true
}
