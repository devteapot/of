use hex_core::{Axial, ConquestRule, TerrainKind};
use spacetimedb::{ReducerContext, Table};
use worldgen::{generate_for_players, validate};

use crate::rules::{calculate_edge_runtime_limits, edge_key};
use crate::schema::{
    CellState, CellTerrain, ClusterPolicyAssignment, ClusterPolicyKind, MapPreset, MatchConfig,
    MatchPhase, MatchState, NEUTRAL_PLAYER, PlayerState, SINGLETON_ID, StaticEdgeLimit,
    TerrainClass,
};
use crate::schema::{
    cell_state, cell_terrain, cluster_policy_assignment, match_config, match_state, player_state,
    policy_replan_state, policy_topology_cache, static_edge_limit,
};

pub fn default_config() -> MatchConfig {
    let preset = MapPreset::Dev64;
    MatchConfig {
        singleton_id: SINGLETON_ID,
        map_preset: preset,
        player_count: crate::schema::DEFAULT_PLAYER_COUNT,
        lobby_configuration_locked: false,
        map_seed: preset.seed(),
        map_width: preset.side(),
        map_height: preset.side(),
        map_q_min: -(i32::from(preset.side()) / 2),
        map_r_min: -(i32::from(preset.side()) / 2),
        chunk_size: 16,
        logical_step_ms: 250,
        population_step_interval: 4,
        base_military_capacity: 100,
        base_edge_throughput_per_second: 20,
        base_combat_frontage: 25,
        max_elevation_step: 1,
        uphill_attack_bps: 7_500,
        combat_lethality_bps: 1_500,
        civilian_growth_bps: 100,
        mobilization_per_population_step: 2,
        conquest_threshold_bps: 8_000,
        map_hash: 0,
    }
}

pub fn regenerate_map(
    ctx: &ReducerContext,
    preset: MapPreset,
    seed: u64,
    player_count: u16,
    lobby_configuration_locked: bool,
) -> Result<(), String> {
    clear_map(ctx);
    let side = preset.side();
    let generated = generate_for_players(
        match preset {
            MapPreset::Dev64 => "dev-stepped-island",
            MapPreset::Playtest128 => "playtest-stepped-island",
            MapPreset::Validation192 => "validation-stepped-island",
        },
        side,
        side,
        seed,
        player_count,
    );
    validate(&generated).map_err(|error| format!("generated map is invalid: {error}"))?;
    let manifest = &generated.manifest;
    let width = manifest.width;
    let height = manifest.height;
    let spawn_cell_ids = manifest
        .spawn_cells
        .iter()
        .map(|spawn| cell_id_for_manifest(manifest, *spawn))
        .collect::<Result<Vec<_>, _>>()?;
    let capturable = u64::from(manifest.capturable_land);
    let mut controlled = vec![0_u64; usize::from(player_count)];
    for cell in generated.cells.cells() {
        let coordinate = cell.coordinate;
        let cell_id = cell_id_for_manifest(manifest, coordinate)?;
        let terrain = match cell.terrain {
            TerrainKind::Water => TerrainClass::Water,
            TerrainKind::Plains => TerrainClass::Plains,
            TerrainKind::Hills => TerrainClass::Hills,
            TerrainKind::Mountain => TerrainClass::Mountain,
        };
        let owner = u16::try_from(cell.owner.unwrap_or_default())
            .map_err(|_| "world generator emitted an unsupported owner")?;
        if cell.capturable && owner != NEUTRAL_PLAYER {
            let index = usize::from(owner - 1);
            let count = controlled
                .get_mut(index)
                .ok_or("world generator emitted an unsupported owner")?;
            *count += 1;
        }
        let column = coordinate.q - manifest.q_min;
        let row = coordinate.r - manifest.r_min;
        let population_interval = default_config().population_step_interval.max(1);
        if population_interval > u32::from(u16::MAX) {
            return Err(format!(
                "population_step_interval {population_interval} exceeds u16 shard storage"
            ));
        }
        let chunk_q = i16::try_from(column.div_euclid(16)).map_err(|_| "chunk q overflow")?;
        let chunk_r = i16::try_from(row.div_euclid(16)).map_err(|_| "chunk r overflow")?;
        ctx.db.cell_terrain().insert(CellTerrain {
            cell_id,
            q: coordinate.q,
            r: coordinate.r,
            chunk_q,
            chunk_r,
            terrain,
            elevation: cell.elevation,
            passable: cell.terrain.ground_passable(),
            capturable: cell.capturable,
            habitable: cell.habitable,
        });
        ctx.db.cell_state().insert(CellState {
            cell_id,
            owner_player_id: owner,
            civilians: cell.civilian_population,
            civilian_capacity: cell.civilian_capacity,
            infantry: cell.force(),
            military_capacity: cell.military_capacity,
            population_shard: u16::try_from(cell_id % population_interval)
                .map_err(|_| "population shard overflow")?,
            chunk_q,
            chunk_r,
            last_changed_step: 0,
            last_policy_changed_step: 0,
        });
        if owner != NEUTRAL_PLAYER && owner <= player_count {
            ctx.db
                .cluster_policy_assignment()
                .insert(ClusterPolicyAssignment {
                    cell_id,
                    owner_player_id: owner,
                    kind: ClusterPolicyKind::Balanced,
                    orientation_q: 0,
                    orientation_r: 0,
                    revision: 0,
                });
        }
    }

    let mut config = default_config();
    config.map_preset = preset;
    config.player_count = player_count;
    config.lobby_configuration_locked = lobby_configuration_locked;
    config.map_seed = seed;
    config.map_width = width;
    config.map_height = height;
    config.map_q_min = manifest.q_min;
    config.map_r_min = manifest.r_min;
    config.map_hash = manifest.content_hash;

    for from in generated.cells.cells() {
        let from_cell = cell_id_for_manifest(manifest, from.coordinate)?;
        for neighbor in from.coordinate.neighbors() {
            let Some(to) = generated.cells.get(neighbor) else {
                continue;
            };
            let to_cell = cell_id_for_manifest(manifest, neighbor)?;
            if from_cell >= to_cell {
                continue;
            }
            let forward = calculate_edge_runtime_limits(&config, from, to);
            let reverse = calculate_edge_runtime_limits(&config, to, from);
            let traversable = forward.is_some() && reverse.is_some();
            let forward = forward.unwrap_or(crate::rules::EdgeRuntimeLimits {
                throughput_per_step: 0,
                frontage: 0,
                uphill: false,
            });
            let reverse = reverse.unwrap_or(crate::rules::EdgeRuntimeLimits {
                throughput_per_step: 0,
                frontage: 0,
                uphill: false,
            });
            ctx.db.static_edge_limit().insert(StaticEdgeLimit {
                edge_key: edge_key(from_cell, to_cell),
                first_cell: from_cell,
                second_cell: to_cell,
                traversable,
                first_to_second_throughput: forward.throughput_per_step,
                second_to_first_throughput: reverse.throughput_per_step,
                first_to_second_frontage: forward.frontage,
                second_to_first_frontage: reverse.frontage,
                first_to_second_uphill: forward.uphill,
                second_to_first_uphill: reverse.uphill,
            });
        }
    }

    let rule = ConquestRule::new(capturable, 8_000)
        .map_err(|error| format!("invalid conquest map: {error:?}"))?;
    ctx.db.match_config().insert(config);
    for (index, spawn_cell_id) in spawn_cell_ids.into_iter().enumerate() {
        let player_id = u16::try_from(index + 1).map_err(|_| "player ID overflow")?;
        ctx.db.player_state().insert(PlayerState {
            player_id,
            spawn_cell_id,
            controlled_cells: controlled[index],
        });
    }
    ctx.db.match_state().insert(MatchState {
        singleton_id: SINGLETON_ID,
        phase: MatchPhase::Lobby,
        logical_step: 0,
        capturable_cells: capturable,
        required_control: rule.required_control(),
        winner_player_id: NEUTRAL_PLAYER,
        claimed_players: 0,
        latest_cluster_policy_revision: 0,
        ownership_revision: 1,
        policy_topology_revision: 0,
        policy_replan_cursor: 0,
        started_at_us: 0,
        completed_at_us: 0,
    });
    Ok(())
}

fn clear_map(ctx: &ReducerContext) {
    let player_ids = ctx
        .db
        .player_state()
        .iter()
        .map(|row| row.player_id)
        .collect::<Vec<_>>();
    for player_id in player_ids {
        ctx.db.player_state().player_id().delete(player_id);
    }
    let replan_keys = ctx
        .db
        .policy_replan_state()
        .iter()
        .map(|row| row.component_key)
        .collect::<Vec<_>>();
    for component_key in replan_keys {
        ctx.db
            .policy_replan_state()
            .component_key()
            .delete(component_key);
    }
    let topology_keys = ctx
        .db
        .policy_topology_cache()
        .iter()
        .map(|row| row.component_key)
        .collect::<Vec<_>>();
    for component_key in topology_keys {
        ctx.db
            .policy_topology_cache()
            .component_key()
            .delete(component_key);
    }
    let edge_keys = ctx
        .db
        .static_edge_limit()
        .iter()
        .map(|row| row.edge_key)
        .collect::<Vec<_>>();
    for edge_key in edge_keys {
        ctx.db.static_edge_limit().edge_key().delete(edge_key);
    }
    let terrain_ids: Vec<_> = ctx
        .db
        .cell_terrain()
        .iter()
        .map(|row| row.cell_id)
        .collect();
    for cell_id in terrain_ids {
        ctx.db.cell_terrain().cell_id().delete(cell_id);
    }
    let state_ids: Vec<_> = ctx.db.cell_state().iter().map(|row| row.cell_id).collect();
    for cell_id in state_ids {
        ctx.db.cell_state().cell_id().delete(cell_id);
    }
    let policy_cell_ids: Vec<_> = ctx
        .db
        .cluster_policy_assignment()
        .iter()
        .map(|row| row.cell_id)
        .collect();
    for cell_id in policy_cell_ids {
        ctx.db.cluster_policy_assignment().cell_id().delete(cell_id);
    }
    ctx.db.match_config().singleton_id().delete(SINGLETON_ID);
    ctx.db.match_state().singleton_id().delete(SINGLETON_ID);
}

fn cell_id_for_manifest(
    manifest: &worldgen::MapManifest,
    coordinate: Axial,
) -> Result<u32, String> {
    let column = coordinate.q - manifest.q_min;
    let row = coordinate.r - manifest.r_min;
    if column < 0
        || row < 0
        || column >= i32::from(manifest.width)
        || row >= i32::from(manifest.height)
    {
        return Err("world generator emitted an out-of-bounds coordinate".into());
    }
    Ok((row as u32) * u32::from(manifest.width) + column as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_map_configuration_is_unlocked() {
        assert!(!default_config().lobby_configuration_locked);
    }

    #[test]
    fn presets_have_the_locked_dimensions() {
        assert_eq!(MapPreset::Dev64.side(), 64);
        assert_eq!(MapPreset::Playtest128.side(), 128);
        assert_eq!(MapPreset::Validation192.side(), 192);
    }

    #[test]
    fn presets_expose_stable_seeds() {
        assert_ne!(MapPreset::Dev64.seed(), MapPreset::Playtest128.seed());
        assert_ne!(
            MapPreset::Playtest128.seed(),
            MapPreset::Validation192.seed()
        );
    }
}
