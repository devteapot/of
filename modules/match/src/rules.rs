use hex_core::{Axial, Cell, ForceComposition, MovementConfig, TerrainKind, ground_traversal};
use spacetimedb::{ReducerContext, Table};

use crate::schema::{
    CellState, CellTerrain, CommandReceipt, MatchConfig, MatchPhase, MatchState, ReceiptStatus,
    SINGLETON_ID, TerrainClass,
};
use crate::schema::{
    cell_state, cell_terrain, command_receipt, match_config, match_state, player_slot,
    static_edge_limit, transit_packet,
};

pub const BASIS_POINTS: u64 = 10_000;
/// V1 exact-payload ceiling. This covers Select All on the largest current map
/// preset (21,484 capturable cells) while retaining a bounded reducer input.
pub const MAX_SELECTION_CELLS: usize = 32_768;

#[derive(Clone, Copy, Debug)]
pub struct EdgeRuntimeLimits {
    pub throughput_per_step: u64,
    pub frontage: u64,
    pub uphill: bool,
}

pub const fn edge_key(first_cell: u32, second_cell: u32) -> u64 {
    let first = if first_cell < second_cell {
        first_cell
    } else {
        second_cell
    };
    let second = if first_cell < second_cell {
        second_cell
    } else {
        first_cell
    };
    (first as u64) << 32 | second as u64
}

pub fn config(ctx: &ReducerContext) -> Result<MatchConfig, String> {
    ctx.db
        .match_config()
        .singleton_id()
        .find(SINGLETON_ID)
        .ok_or_else(|| "match config is missing".into())
}

pub fn state(ctx: &ReducerContext) -> Result<MatchState, String> {
    ctx.db
        .match_state()
        .singleton_id()
        .find(SINGLETON_ID)
        .ok_or_else(|| "match state is missing".into())
}

pub fn require_running_player(ctx: &ReducerContext) -> Result<u8, String> {
    if state(ctx)?.phase != MatchPhase::Running {
        return Err("commands are only accepted while the match is running".into());
    }
    let sender = ctx.sender();
    ctx.db
        .player_slot()
        .iter()
        .find(|slot| slot.identity.as_ref() == Some(&sender))
        .map(|slot| slot.player_id)
        .ok_or_else(|| "the calling identity does not own a player slot".into())
}

pub fn command_key(player_id: u8, client_command_id: u64) -> String {
    format!("{player_id}:{client_command_id}")
}

pub fn command_was_seen(ctx: &ReducerContext, player_id: u8, client_command_id: u64) -> bool {
    ctx.db
        .command_receipt()
        .receipt_key()
        .find(command_key(player_id, client_command_id))
        .is_some()
}

pub fn write_receipt(
    ctx: &ReducerContext,
    player_id: u8,
    client_command_id: u64,
    command_name: &str,
    status: ReceiptStatus,
    order_id: u64,
    message: impl Into<String>,
) -> Result<(), String> {
    let logical_step = state(ctx)?.logical_step;
    ctx.db.command_receipt().insert(CommandReceipt {
        receipt_key: command_key(player_id, client_command_id),
        player_id,
        client_command_id,
        command_name: command_name.into(),
        status,
        order_id,
        message: message.into(),
        logical_step,
    });
    Ok(())
}

pub fn coordinate_for_cell(ctx: &ReducerContext, cell_id: u32) -> Result<Axial, String> {
    let terrain = terrain(ctx, cell_id)?;
    Ok(Axial::new(terrain.q, terrain.r))
}

pub fn cell_id_for_coordinate(config: &MatchConfig, coordinate: Axial) -> Option<u32> {
    let column = coordinate.q.checked_sub(config.map_q_min)?;
    let row = coordinate.r.checked_sub(config.map_r_min)?;
    if column < 0
        || row < 0
        || column >= i32::from(config.map_width)
        || row >= i32::from(config.map_height)
    {
        return None;
    }
    Some((row as u32) * u32::from(config.map_width) + column as u32)
}

pub fn terrain(ctx: &ReducerContext, cell_id: u32) -> Result<CellTerrain, String> {
    ctx.db
        .cell_terrain()
        .cell_id()
        .find(cell_id)
        .ok_or_else(|| format!("unknown cell {cell_id}"))
}

pub fn cell_state(ctx: &ReducerContext, cell_id: u32) -> Result<CellState, String> {
    ctx.db
        .cell_state()
        .cell_id()
        .find(cell_id)
        .ok_or_else(|| format!("state for cell {cell_id} is missing"))
}

pub fn core_cell(ctx: &ReducerContext, cell_id: u32) -> Result<Cell, String> {
    let terrain_row = terrain(ctx, cell_id)?;
    let state_row = cell_state(ctx, cell_id)?;
    let coordinate = Axial::new(terrain_row.q, terrain_row.r);
    let owner = (state_row.owner_player_id != 0).then_some(u32::from(state_row.owner_player_id));
    let mut cell = if terrain_row.terrain == TerrainClass::Water {
        Cell::water(coordinate, terrain_row.elevation)
    } else {
        Cell::ground(
            coordinate,
            terrain_row.elevation,
            owner,
            state_row.military_capacity,
        )
    };
    cell.terrain = match terrain_row.terrain {
        TerrainClass::Water => TerrainKind::Water,
        TerrainClass::Plains => TerrainKind::Plains,
        TerrainClass::Hills => TerrainKind::Hills,
        TerrainClass::Mountain => TerrainKind::Mountain,
    };
    cell.capturable = terrain_row.capturable;
    cell.habitable = terrain_row.habitable;
    cell.owner = owner;
    cell.civilian_population = state_row.civilians;
    cell.civilian_capacity = state_row.civilian_capacity;
    cell.forces = ForceComposition::infantry(state_row.infantry);
    cell.military_capacity = state_row.military_capacity;
    Ok(cell)
}

pub fn edge_runtime_limits(
    ctx: &ReducerContext,
    from_cell: u32,
    to_cell: u32,
) -> Result<Option<EdgeRuntimeLimits>, String> {
    if let Some(limits) = ctx
        .db
        .static_edge_limit()
        .edge_key()
        .find(edge_key(from_cell, to_cell))
    {
        if !limits.traversable {
            return Ok(None);
        }
        let forward = from_cell == limits.first_cell;
        return Ok(Some(if forward {
            EdgeRuntimeLimits {
                throughput_per_step: limits.first_to_second_throughput,
                frontage: limits.first_to_second_frontage,
                uphill: limits.first_to_second_uphill,
            }
        } else {
            EdgeRuntimeLimits {
                throughput_per_step: limits.second_to_first_throughput,
                frontage: limits.second_to_first_frontage,
                uphill: limits.second_to_first_uphill,
            }
        }));
    }

    // Databases created before the static-edge table was introduced remain
    // playable until their next lobby map regeneration backfills the cache.
    let config = config(ctx)?;
    let from = core_cell(ctx, from_cell)?;
    let to = core_cell(ctx, to_cell)?;
    Ok(calculate_edge_runtime_limits(&config, &from, &to))
}

pub fn calculate_edge_runtime_limits(
    config: &MatchConfig,
    from: &Cell,
    to: &Cell,
) -> Option<EdgeRuntimeLimits> {
    let movement = MovementConfig {
        max_elevation_step: u16::from(config.max_elevation_step),
        ..MovementConfig::default()
    };
    ground_traversal(from, to, &movement)?;
    let minimum_capacity = from.military_capacity.min(to.military_capacity);
    let capacity_scale = minimum_capacity.min(config.base_military_capacity);
    let mut throughput = u128::from(config.base_edge_throughput_per_second)
        * u128::from(config.logical_step_ms)
        * u128::from(capacity_scale)
        / 1_000
        / u128::from(config.base_military_capacity.max(1));
    let mut frontage = u128::from(config.base_combat_frontage) * u128::from(capacity_scale)
        / u128::from(config.base_military_capacity.max(1));
    if matches!(from.terrain, TerrainKind::Hills | TerrainKind::Mountain)
        || matches!(to.terrain, TerrainKind::Hills | TerrainKind::Mountain)
    {
        throughput = throughput * 8_000 / u128::from(BASIS_POINTS);
        frontage = frontage * 8_000 / u128::from(BASIS_POINTS);
    }
    if to.elevation > from.elevation {
        throughput = throughput * 7_500 / u128::from(BASIS_POINTS);
    }
    Some(EdgeRuntimeLimits {
        throughput_per_step: (throughput as u64).max(1),
        frontage: (frontage as u64).max(1),
        uphill: to.elevation > from.elevation,
    })
}

pub fn allocated_infantry_at_cell(ctx: &ReducerContext, owner_player_id: u8, cell_id: u32) -> u64 {
    ctx.db
        .transit_packet()
        .packet_by_cell()
        .filter(cell_id)
        .filter(|packet| packet.owner_player_id == owner_player_id)
        .map(|packet| packet.infantry)
        .sum()
}

pub fn packet_key(
    order_id: u64,
    origin_cell: u32,
    destination_cell: u32,
    current_cell: u32,
    route_index: u32,
) -> String {
    format!("{order_id}:{origin_cell}:{destination_cell}:{current_cell}:{route_index}")
}
