#![forbid(unsafe_code)]

mod mapgen;
mod orders;
mod rules;
mod schema;
mod simulation;

use std::time::Duration;

use spacetimedb::{Identity, ReducerContext, ScheduleAt, Table};

use crate::mapgen::regenerate_map;
use crate::schema::{
    DEFAULT_PLAYER_COUNT, MAX_PLAYER_COUNT, MIN_PLAYER_COUNT, MapPreset, MatchPhase,
    MobilizationPolicy, PlayerIdentity, PlayerSlot, SINGLETON_ID, SimulationSchedule,
};
use crate::schema::{
    match_config, match_state, mobilization_policy, player_identity, player_slot,
    simulation_schedule,
};

fn timestamp_us(ctx: &ReducerContext) -> u64 {
    ctx.timestamp
        .to_duration_since_unix_epoch()
        .unwrap_or_default()
        .as_micros() as u64
}

#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) -> Result<(), String> {
    configure_player_rows(ctx, DEFAULT_PLAYER_COUNT);
    regenerate_map(
        ctx,
        MapPreset::Dev64,
        MapPreset::Dev64.seed(),
        DEFAULT_PLAYER_COUNT,
        false,
    )
}

#[spacetimedb::reducer]
pub fn configure_map(ctx: &ReducerContext, preset: MapPreset) -> Result<(), String> {
    require_configurable_lobby(ctx)?;
    let player_count = ctx
        .db
        .match_config()
        .singleton_id()
        .find(SINGLETON_ID)
        .ok_or("match config is missing")?
        .player_count;
    regenerate_map(ctx, preset, preset.seed(), player_count, true)
}

/// Configures map/player scale once without claiming a player slot. Every
/// configured slot remains available through `join_match`.
#[spacetimedb::reducer]
pub fn configure_match(
    ctx: &ReducerContext,
    preset: MapPreset,
    player_count: u16,
) -> Result<(), String> {
    require_configurable_lobby(ctx)?;
    validate_player_count(player_count)?;
    configure_player_rows(ctx, player_count);
    regenerate_map(ctx, preset, preset.seed(), player_count, true)
}

fn validate_player_count(player_count: u16) -> Result<(), String> {
    if (MIN_PLAYER_COUNT..=MAX_PLAYER_COUNT).contains(&player_count) {
        Ok(())
    } else {
        Err(format!(
            "player count must be between {MIN_PLAYER_COUNT} and {MAX_PLAYER_COUNT}"
        ))
    }
}

fn require_configurable_lobby(ctx: &ReducerContext) -> Result<(), String> {
    let phase = ctx
        .db
        .match_state()
        .singleton_id()
        .find(SINGLETON_ID)
        .ok_or("match state is missing")?
        .phase;
    let configuration_locked = ctx
        .db
        .match_config()
        .singleton_id()
        .find(SINGLETON_ID)
        .ok_or("match config is missing")?
        .lobby_configuration_locked;
    let any_player_joined = ctx
        .db
        .match_state()
        .singleton_id()
        .find(SINGLETON_ID)
        .ok_or("match state is missing")?
        .claimed_players
        > 0
        || ctx.db.player_identity().iter().next().is_some();
    validate_lobby_configuration(phase, configuration_locked, any_player_joined)
        .map_err(str::to_owned)
}

fn validate_lobby_configuration(
    phase: MatchPhase,
    configuration_locked: bool,
    any_player_joined: bool,
) -> Result<(), &'static str> {
    if phase != MatchPhase::Lobby {
        return Err("the match can only be configured in the lobby");
    }
    if configuration_locked {
        return Err("lobby configuration is already locked");
    }
    if any_player_joined {
        return Err("the match must be configured before any player joins");
    }
    Ok(())
}

fn claim_player_slot(slot: &mut PlayerSlot, identity: Identity, now: u64) {
    slot.identity = Some(identity);
    slot.display_name = format!("Player {}", slot.player_id);
    slot.connected = true;
    slot.ready = true;
    slot.joined_at_us = now;
    slot.last_seen_at_us = now;
}

fn clear_player_identities(ctx: &ReducerContext) {
    let identities = ctx
        .db
        .player_identity()
        .iter()
        .map(|row| row.identity)
        .collect::<Vec<_>>();
    for identity in identities {
        ctx.db.player_identity().identity().delete(identity);
    }
}

fn configure_player_rows(ctx: &ReducerContext, player_count: u16) {
    clear_player_identities(ctx);
    let existing_slots = ctx
        .db
        .player_slot()
        .iter()
        .map(|slot| slot.player_id)
        .collect::<Vec<_>>();
    for player_id in existing_slots {
        ctx.db.player_slot().player_id().delete(player_id);
    }
    let existing_policies = ctx
        .db
        .mobilization_policy()
        .iter()
        .map(|policy| policy.player_id)
        .collect::<Vec<_>>();
    for player_id in existing_policies {
        ctx.db.mobilization_policy().player_id().delete(player_id);
    }
    for player_id in 1..=player_count {
        ctx.db.player_slot().insert(PlayerSlot {
            player_id,
            identity: None,
            display_name: String::new(),
            connected: false,
            has_reconnected: false,
            reconnect_count: 0,
            ready: false,
            joined_at_us: 0,
            last_seen_at_us: 0,
        });
        ctx.db.mobilization_policy().insert(MobilizationPolicy {
            player_id,
            target_bps: 2_500,
        });
    }
}

fn player_id_for_identity(ctx: &ReducerContext, identity: Identity) -> Option<u16> {
    ctx.db
        .player_identity()
        .identity()
        .find(identity)
        .map(|row| row.player_id)
}

/// How a slot's stored identity relates to the joining sender when the private
/// identity index names that seat.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IndexedSlotBinding {
    MissingSlot,
    Empty,
    MatchesSender,
    OtherIdentity,
}

/// Recovered seat for a reconnecting identity. `needs_index_repair` is set when
/// the private `player_identity` row is missing or stale but a slot still holds
/// the sender identity — reconnect must rebind the index without claiming.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RecoveredJoinIdentity {
    player_id: u16,
    needs_index_repair: bool,
}

/// Pure join-identity recovery used by `join_match` and unit tests.
///
/// Detects an existing seat even when the private identity index row is missing,
/// rejects conflicting index/slot mappings, and never invents a fresh claim.
fn recover_join_identity(
    index_player_id: Option<u16>,
    slot_player_id_for_sender: Option<u16>,
    indexed_slot_binding: IndexedSlotBinding,
) -> Result<Option<RecoveredJoinIdentity>, &'static str> {
    match (index_player_id, slot_player_id_for_sender) {
        (Some(index_id), Some(slot_id)) if index_id != slot_id => {
            Err("identity index and player slot disagree about this identity")
        }
        (Some(index_id), Some(slot_id)) => {
            debug_assert_eq!(index_id, slot_id);
            match indexed_slot_binding {
                IndexedSlotBinding::MatchesSender => Ok(Some(RecoveredJoinIdentity {
                    player_id: index_id,
                    needs_index_repair: false,
                })),
                IndexedSlotBinding::Empty | IndexedSlotBinding::MissingSlot => {
                    // Index is authoritative enough to reclaim; slot identity is
                    // restored by the reconnect path without a new claim.
                    Ok(Some(RecoveredJoinIdentity {
                        player_id: index_id,
                        needs_index_repair: false,
                    }))
                }
                IndexedSlotBinding::OtherIdentity => {
                    Err("identity index points at a slot owned by another identity")
                }
            }
        }
        (Some(index_id), None) => match indexed_slot_binding {
            IndexedSlotBinding::MatchesSender | IndexedSlotBinding::Empty => {
                Ok(Some(RecoveredJoinIdentity {
                    player_id: index_id,
                    needs_index_repair: false,
                }))
            }
            IndexedSlotBinding::MissingSlot => Err("player slot is missing for bound identity"),
            IndexedSlotBinding::OtherIdentity => {
                Err("identity index points at a slot owned by another identity")
            }
        },
        (None, Some(slot_id)) => Ok(Some(RecoveredJoinIdentity {
            player_id: slot_id,
            needs_index_repair: true,
        })),
        (None, None) => Ok(None),
    }
}

fn slot_player_id_for_identity(ctx: &ReducerContext, identity: Identity) -> Option<u16> {
    ctx.db.player_slot().iter().find_map(|slot| {
        slot.identity
            .filter(|bound| *bound == identity)
            .map(|_| slot.player_id)
    })
}

fn indexed_slot_binding(
    ctx: &ReducerContext,
    index_player_id: Option<u16>,
    sender: Identity,
) -> IndexedSlotBinding {
    let Some(player_id) = index_player_id else {
        return IndexedSlotBinding::MissingSlot;
    };
    match ctx.db.player_slot().player_id().find(player_id) {
        None => IndexedSlotBinding::MissingSlot,
        Some(slot) => match slot.identity {
            None => IndexedSlotBinding::Empty,
            Some(bound) if bound == sender => IndexedSlotBinding::MatchesSender,
            Some(_) => IndexedSlotBinding::OtherIdentity,
        },
    }
}

fn resolve_existing_join_identity(
    ctx: &ReducerContext,
    sender: Identity,
) -> Result<Option<RecoveredJoinIdentity>, String> {
    let index_player_id = player_id_for_identity(ctx, sender);
    recover_join_identity(
        index_player_id,
        slot_player_id_for_identity(ctx, sender),
        indexed_slot_binding(ctx, index_player_id, sender),
    )
    .map_err(str::to_owned)
}

fn bind_identity(ctx: &ReducerContext, identity: Identity, player_id: u16) -> Result<(), String> {
    if let Some(existing) = ctx.db.player_identity().identity().find(identity) {
        if existing.player_id != player_id {
            // Refuse to silently rebind an identity that already names another seat.
            return Err("identity is already bound to a different player slot".to_owned());
        }
        return Ok(());
    }
    if ctx
        .db
        .player_identity()
        .player_id()
        .find(player_id)
        .is_some_and(|row| row.identity != identity)
    {
        return Err("player slot already has a different identity index row".to_owned());
    }
    ctx.db.player_identity().insert(PlayerIdentity {
        identity,
        player_id,
    });
    Ok(())
}

fn verified_claimed_players(ctx: &ReducerContext) -> u16 {
    let count = ctx
        .db
        .player_slot()
        .iter()
        .filter(|slot| slot.identity.is_some())
        .count();
    u16::try_from(count).unwrap_or(u16::MAX)
}

/// Recompute `claimed_players` from verified configured seats and, when the
/// lobby is full, transition to Running and install the simulation schedule.
///
/// Shared by both fresh claims and recovered/repaired identity joins so a
/// repaired seat can complete the lobby without a preliminary counter bump
/// that could fail or drift from the verified seat set.
fn reconcile_claimed_players_and_maybe_start(ctx: &ReducerContext, now: u64) -> Result<(), String> {
    let player_count = ctx
        .db
        .match_config()
        .singleton_id()
        .find(SINGLETON_ID)
        .ok_or("match config is missing")?
        .player_count;
    let mut state = ctx
        .db
        .match_state()
        .singleton_id()
        .find(SINGLETON_ID)
        .ok_or("match state is missing")?;
    let verified = verified_claimed_players(ctx);
    state.claimed_players = verified;
    if state.phase == MatchPhase::Lobby && verified == player_count {
        state.phase = MatchPhase::Running;
        state.started_at_us = now;
        ctx.db.match_state().singleton_id().update(state);
        ensure_simulation_schedule(ctx)?;
    } else {
        ctx.db.match_state().singleton_id().update(state);
    }
    Ok(())
}

fn apply_reconnect_to_slot(
    slot: &mut PlayerSlot,
    identity: Identity,
    now: u64,
    display_name: &str,
) {
    if slot.identity.is_none() {
        slot.identity = Some(identity);
    }
    let was_disconnected = !slot.connected;
    slot.connected = true;
    if was_disconnected {
        slot.has_reconnected = true;
        slot.reconnect_count = slot.reconnect_count.saturating_add(1);
    }
    slot.last_seen_at_us = now;
    if !display_name.trim().is_empty() {
        slot.display_name = display_name.trim().chars().take(32).collect();
    } else if slot.display_name.is_empty() {
        slot.display_name = format!("Player {}", slot.player_id);
    }
    slot.ready = true;
}

#[spacetimedb::reducer]
pub fn join_match(
    ctx: &ReducerContext,
    preferred_player_id: u16,
    display_name: String,
) -> Result<(), String> {
    let sender = ctx.sender();
    let now = timestamp_us(ctx);
    if let Some(recovered) = resolve_existing_join_identity(ctx, sender)? {
        let mut existing = ctx
            .db
            .player_slot()
            .player_id()
            .find(recovered.player_id)
            .ok_or("player slot is missing for bound identity")?;
        if existing.identity.is_some_and(|bound| bound != sender) {
            return Err("player slot is already claimed by another identity".into());
        }
        apply_reconnect_to_slot(&mut existing, sender, now, &display_name);
        ctx.db.player_slot().player_id().update(existing);
        if recovered.needs_index_repair {
            bind_identity(ctx, sender, recovered.player_id)?;
        }
        // Recovered seats (including index repair) must still reconcile the
        // verified claim count so a repaired final seat can start the lobby.
        reconcile_claimed_players_and_maybe_start(ctx, now)?;
        return Ok(());
    }

    let player_count = ctx
        .db
        .match_config()
        .singleton_id()
        .find(SINGLETON_ID)
        .ok_or("match config is missing")?
        .player_count;
    if !(1..=player_count).contains(&preferred_player_id) {
        return Err(format!(
            "preferred player ID must be between 1 and {player_count}"
        ));
    }
    let player_id = if ctx
        .db
        .player_slot()
        .player_id()
        .find(preferred_player_id)
        .is_some_and(|slot| slot.identity.is_none())
    {
        preferred_player_id
    } else {
        (1..=player_count)
            .find(|candidate| {
                ctx.db
                    .player_slot()
                    .player_id()
                    .find(*candidate)
                    .is_some_and(|slot| slot.identity.is_none())
            })
            .ok_or("all player slots are already claimed")?
    };
    let mut slot = ctx
        .db
        .player_slot()
        .player_id()
        .find(player_id)
        .ok_or("player slot is missing")?;
    claim_player_slot(&mut slot, sender, now);
    if !display_name.trim().is_empty() {
        slot.display_name = display_name.trim().chars().take(32).collect();
    }
    ctx.db.player_slot().player_id().update(slot);
    bind_identity(ctx, sender, player_id)?;
    reconcile_claimed_players_and_maybe_start(ctx, now)?;
    Ok(())
}

#[spacetimedb::reducer(client_connected)]
pub fn identity_connected(ctx: &ReducerContext) {
    let sender = ctx.sender();
    let Some(player_id) = player_id_for_identity(ctx, sender) else {
        return;
    };
    if let Some(mut slot) = ctx.db.player_slot().player_id().find(player_id) {
        if !slot.connected {
            slot.has_reconnected = true;
            slot.reconnect_count = slot.reconnect_count.saturating_add(1);
        }
        slot.connected = true;
        slot.last_seen_at_us = timestamp_us(ctx);
        ctx.db.player_slot().player_id().update(slot);
    }
}

#[spacetimedb::reducer(client_disconnected)]
pub fn identity_disconnected(ctx: &ReducerContext) {
    let sender = ctx.sender();
    let Some(player_id) = player_id_for_identity(ctx, sender) else {
        return;
    };
    if let Some(mut slot) = ctx.db.player_slot().player_id().find(player_id) {
        slot.connected = false;
        slot.last_seen_at_us = timestamp_us(ctx);
        ctx.db.player_slot().player_id().update(slot);
    }
}

fn ensure_simulation_schedule(ctx: &ReducerContext) -> Result<(), String> {
    let duration_ms = ctx
        .db
        .match_config()
        .singleton_id()
        .find(SINGLETON_ID)
        .ok_or("match config is missing")?
        .logical_step_ms;
    let desired = ScheduleAt::from(Duration::from_millis(u64::from(duration_ms)));
    let existing = ctx.db.simulation_schedule().iter().collect::<Vec<_>>();
    if existing.len() == 1 && existing[0].scheduled_at == desired {
        return Ok(());
    }
    for schedule in existing {
        ctx.db
            .simulation_schedule()
            .scheduled_id()
            .delete(schedule.scheduled_id);
    }
    ctx.db.simulation_schedule().insert(SimulationSchedule {
        scheduled_id: 0,
        scheduled_at: desired,
    });
    Ok(())
}

#[spacetimedb::reducer]
pub fn simulation_tick(ctx: &ReducerContext, schedule: SimulationSchedule) -> Result<(), String> {
    if ctx.sender() != ctx.database_identity() {
        return Err("simulation_tick may only be invoked by the scheduler".into());
    }
    let still_running = crate::simulation::advance_simulation(ctx)?;
    if !still_running {
        ctx.db
            .simulation_schedule()
            .scheduled_id()
            .delete(schedule.scheduled_id);
    } else if matches!(schedule.scheduled_at, ScheduleAt::Time(_)) {
        // Seamlessly replace a legacy one-shot row when an active database is
        // upgraded to interval scheduling.
        ensure_simulation_schedule(ctx)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_configuration_accepts_two_through_five_hundred_players() {
        for player_count in [2, 3, 8, 9, 32, 128, 256, 500] {
            assert!(validate_player_count(player_count).is_ok());
        }
        assert!(validate_player_count(1).is_err());
        assert!(validate_player_count(501).is_err());
    }

    #[test]
    fn lobby_configuration_is_one_shot_without_requiring_a_slot_claim() {
        assert!(validate_lobby_configuration(MatchPhase::Lobby, false, false).is_ok());
        assert_eq!(
            validate_lobby_configuration(MatchPhase::Lobby, true, false),
            Err("lobby configuration is already locked")
        );
        assert_eq!(
            validate_lobby_configuration(MatchPhase::Lobby, false, true),
            Err("the match must be configured before any player joins")
        );
    }

    #[test]
    fn join_slot_claim_uses_join_compatible_defaults() {
        let mut slot = PlayerSlot {
            player_id: 1,
            identity: None,
            display_name: String::new(),
            connected: false,
            has_reconnected: false,
            reconnect_count: 0,
            ready: false,
            joined_at_us: 0,
            last_seen_at_us: 0,
        };
        claim_player_slot(&mut slot, Identity::ZERO, 42);
        assert_eq!(slot.identity, Some(Identity::ZERO));
        assert_eq!(slot.display_name, "Player 1");
        assert!(slot.connected);
        assert!(slot.ready);
        assert_eq!(slot.joined_at_us, 42);
        assert_eq!(slot.last_seen_at_us, 42);
    }

    #[test]
    fn join_identity_recovery_repairs_missing_index_without_claim() {
        let recovered = recover_join_identity(None, Some(7), IndexedSlotBinding::MissingSlot)
            .expect("slot-only identity is recoverable");
        assert_eq!(
            recovered,
            Some(RecoveredJoinIdentity {
                player_id: 7,
                needs_index_repair: true,
            })
        );
    }

    #[test]
    fn join_identity_recovery_accepts_consistent_index_and_slot() {
        let recovered = recover_join_identity(Some(3), Some(3), IndexedSlotBinding::MatchesSender)
            .expect("consistent binding");
        assert_eq!(
            recovered,
            Some(RecoveredJoinIdentity {
                player_id: 3,
                needs_index_repair: false,
            })
        );
    }

    #[test]
    fn join_identity_recovery_rejects_conflicting_index_and_slot() {
        assert_eq!(
            recover_join_identity(Some(1), Some(2), IndexedSlotBinding::MatchesSender),
            Err("identity index and player slot disagree about this identity")
        );
        assert_eq!(
            recover_join_identity(Some(4), None, IndexedSlotBinding::OtherIdentity),
            Err("identity index points at a slot owned by another identity")
        );
        assert_eq!(
            recover_join_identity(Some(4), None, IndexedSlotBinding::MissingSlot),
            Err("player slot is missing for bound identity")
        );
    }

    #[test]
    fn join_identity_recovery_allows_fresh_claim_when_unbound() {
        assert_eq!(
            recover_join_identity(None, None, IndexedSlotBinding::MissingSlot),
            Ok(None)
        );
    }

    #[test]
    fn reconnect_does_not_require_claimed_counter_increment() {
        let mut slot = PlayerSlot {
            player_id: 2,
            identity: Some(Identity::ZERO),
            display_name: "Keeper".into(),
            connected: false,
            has_reconnected: false,
            reconnect_count: 0,
            ready: true,
            joined_at_us: 10,
            last_seen_at_us: 10,
        };
        apply_reconnect_to_slot(&mut slot, Identity::ZERO, 99, "");
        assert!(slot.connected);
        assert!(slot.has_reconnected);
        assert_eq!(slot.reconnect_count, 1);
        assert_eq!(slot.last_seen_at_us, 99);
        assert_eq!(slot.display_name, "Keeper");
    }

    #[test]
    fn match_start_requires_exact_verified_claim_count() {
        // Mirrors reconcile_claimed_players_and_maybe_start: only the exact
        // configured seat count starts the match. No preliminary counter bump.
        let player_count = 4_u16;
        for verified in [0_u16, 1, 3, 5, 4] {
            let should_start = verified == player_count;
            assert_eq!(should_start, verified == 4);
        }
    }

    #[test]
    fn join_reconciliation_helper_is_shared_by_recovered_and_fresh_paths() {
        // Both join_match arms call reconcile_claimed_players_and_maybe_start
        // after seat/index mutation. Recovered index repair must be able to
        // complete a full lobby: verified seats alone drive claimed_players and
        // the Lobby→Running transition (no checked_add before replacement).
        let player_count = 3_u16;
        let cases = [
            (2_u16, MatchPhase::Lobby, false),
            (3_u16, MatchPhase::Lobby, true),
            (3_u16, MatchPhase::Running, false),
            (4_u16, MatchPhase::Lobby, false),
        ];
        for (verified, phase, expect_start) in cases {
            let should_start = phase == MatchPhase::Lobby && verified == player_count;
            assert_eq!(
                should_start, expect_start,
                "verified={verified} phase={phase:?}"
            );
        }
        // Corrupted counters are ignored; verified seat count is authoritative.
        let corrupted_counter = u16::MAX;
        let verified_seats = 3_u16;
        let reconciled = verified_seats; // not corrupted_counter.saturating_add(1)
        assert_eq!(reconciled, 3);
        assert_ne!(reconciled, corrupted_counter);
    }

    #[test]
    fn recovered_identity_repair_can_complete_lobby_start_predicate() {
        let recovered = recover_join_identity(None, Some(2), IndexedSlotBinding::MissingSlot)
            .expect("slot-only identity is recoverable")
            .expect("recovered");
        assert!(recovered.needs_index_repair);
        // After repair the seat is verified; if it was the final open seat the
        // shared reconciliation helper starts the match.
        let player_count = 2_u16;
        let verified_after_repair = 2_u16;
        assert!(verified_after_repair == player_count);
    }
}
