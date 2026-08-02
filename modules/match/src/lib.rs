#![forbid(unsafe_code)]

mod mapgen;
mod orders;
mod rules;
mod schema;
mod simulation;

use std::time::Duration;

use spacetimedb::{ReducerContext, ScheduleAt, Table};

use crate::mapgen::regenerate_map;
use crate::schema::{
    MapPreset, MatchPhase, MobilizationPolicy, PLAYER_ONE, PLAYER_TWO, PlayerSlot, SINGLETON_ID,
    SimulationSchedule,
};
use crate::schema::{
    match_config, match_state, mobilization_policy, player_slot, simulation_schedule,
};

fn timestamp_us(ctx: &ReducerContext) -> u64 {
    ctx.timestamp
        .to_duration_since_unix_epoch()
        .unwrap_or_default()
        .as_micros() as u64
}

#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) -> Result<(), String> {
    for player_id in [PLAYER_ONE, PLAYER_TWO] {
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
    regenerate_map(ctx, MapPreset::Dev64, MapPreset::Dev64.seed())
}

#[spacetimedb::reducer]
pub fn configure_map(ctx: &ReducerContext, preset: MapPreset) -> Result<(), String> {
    let phase = ctx
        .db
        .match_state()
        .singleton_id()
        .find(SINGLETON_ID)
        .ok_or("match state is missing")?
        .phase;
    if phase != MatchPhase::Lobby {
        return Err("the map can only be configured in the lobby".into());
    }
    if ctx
        .db
        .player_slot()
        .iter()
        .any(|slot| slot.identity.is_some())
    {
        return Err("the map must be configured before either player joins".into());
    }
    regenerate_map(ctx, preset, preset.seed())
}

#[spacetimedb::reducer]
pub fn join_match(
    ctx: &ReducerContext,
    preferred_player_id: u8,
    display_name: String,
) -> Result<(), String> {
    let sender = ctx.sender();
    let now = timestamp_us(ctx);
    if let Some(mut existing) = ctx
        .db
        .player_slot()
        .iter()
        .find(|slot| slot.identity.as_ref() == Some(&sender))
    {
        let was_disconnected = !existing.connected;
        existing.connected = true;
        if was_disconnected {
            existing.has_reconnected = true;
            existing.reconnect_count = existing.reconnect_count.saturating_add(1);
        }
        existing.last_seen_at_us = now;
        if !display_name.trim().is_empty() {
            existing.display_name = display_name.trim().chars().take(32).collect();
        }
        ctx.db.player_slot().player_id().update(existing);
        return Ok(());
    }

    let player_id = if [PLAYER_ONE, PLAYER_TWO].contains(&preferred_player_id)
        && ctx
            .db
            .player_slot()
            .player_id()
            .find(preferred_player_id)
            .is_some_and(|slot| slot.identity.is_none())
    {
        preferred_player_id
    } else {
        [PLAYER_ONE, PLAYER_TWO]
            .into_iter()
            .find(|candidate| {
                ctx.db
                    .player_slot()
                    .player_id()
                    .find(*candidate)
                    .is_some_and(|slot| slot.identity.is_none())
            })
            .ok_or("both player slots are already claimed")?
    };
    let mut slot = ctx
        .db
        .player_slot()
        .player_id()
        .find(player_id)
        .ok_or("player slot is missing")?;
    slot.identity = Some(sender);
    slot.display_name = if display_name.trim().is_empty() {
        format!("Player {player_id}")
    } else {
        display_name.trim().chars().take(32).collect()
    };
    slot.connected = true;
    slot.ready = true;
    slot.joined_at_us = now;
    slot.last_seen_at_us = now;
    ctx.db.player_slot().player_id().update(slot);

    if ctx
        .db
        .player_slot()
        .iter()
        .all(|slot| slot.identity.is_some())
    {
        let mut state = ctx
            .db
            .match_state()
            .singleton_id()
            .find(SINGLETON_ID)
            .ok_or("match state is missing")?;
        if state.phase == MatchPhase::Lobby {
            state.phase = MatchPhase::Running;
            state.started_at_us = now;
            ctx.db.match_state().singleton_id().update(state);
            ensure_simulation_schedule(ctx)?;
        }
    }
    Ok(())
}

#[spacetimedb::reducer(client_connected)]
pub fn identity_connected(ctx: &ReducerContext) {
    let sender = ctx.sender();
    if let Some(mut slot) = ctx
        .db
        .player_slot()
        .iter()
        .find(|slot| slot.identity.as_ref() == Some(&sender))
    {
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
    if let Some(mut slot) = ctx
        .db
        .player_slot()
        .iter()
        .find(|slot| slot.identity.as_ref() == Some(&sender))
    {
        slot.connected = false;
        slot.last_seen_at_us = timestamp_us(ctx);
        ctx.db.player_slot().player_id().update(slot);
    }
}

fn ensure_simulation_schedule(ctx: &ReducerContext) -> Result<(), String> {
    if ctx.db.simulation_schedule().iter().next().is_some() {
        return Ok(());
    }
    schedule_next_simulation(ctx)
}

fn schedule_next_simulation(ctx: &ReducerContext) -> Result<(), String> {
    let duration_ms = ctx
        .db
        .match_config()
        .singleton_id()
        .find(SINGLETON_ID)
        .ok_or("match config is missing")?
        .logical_step_ms;
    ctx.db.simulation_schedule().insert(SimulationSchedule {
        scheduled_id: 0,
        scheduled_at: ScheduleAt::Time(
            ctx.timestamp + Duration::from_millis(u64::from(duration_ms)),
        ),
    });
    Ok(())
}

#[spacetimedb::reducer]
pub fn simulation_tick(ctx: &ReducerContext, _schedule: SimulationSchedule) -> Result<(), String> {
    if ctx.sender() != ctx.database_identity() {
        return Err("simulation_tick may only be invoked by the scheduler".into());
    }
    if crate::simulation::advance_simulation(ctx)? {
        schedule_next_simulation(ctx)?;
    }
    Ok(())
}
