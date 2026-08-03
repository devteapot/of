use std::collections::{BTreeMap, BTreeSet};

use hex_core::{
    AttackFront, CombatConfig, EdgeLimits, HexMap, LogisticsConfig, MovementConfig, MovementIntent,
    movement_step, resolve_edge_combat,
};
use spacetimedb::{ReducerContext, Table};

use crate::rules::{
    allocated_infantry_at_cell, cell_id_for_coordinate, cell_state, config, coordinate_for_cell,
    core_cell, edge_runtime_limits, packet_key, state, terrain,
};
use crate::schema::{
    CellState, CombatFront, MatchPhase, NEUTRAL_PLAYER, OrderKind, OrderStatus, PLAYER_ONE,
    PLAYER_TWO, TerrainClass, TransferOrder, TransitPacket,
};
use crate::schema::{
    cell_state as cell_state_table, combat_front, match_state, mobilization_policy,
    transfer_destination, transfer_order, transfer_source, transit_packet,
};

pub fn advance_simulation(ctx: &ReducerContext) -> Result<bool, String> {
    let mut match_state = state(ctx)?;
    if match_state.phase != MatchPhase::Running {
        return Ok(false);
    }
    match_state.logical_step = match_state
        .logical_step
        .checked_add(1)
        .ok_or_else(|| "logical step overflow".to_string())?;
    let logical_step = match_state.logical_step;
    ctx.db.match_state().singleton_id().update(match_state);

    clear_combat_fronts(ctx);
    trim_all_overallocated_packets(ctx, logical_step)?;
    move_friendly_packets(ctx, logical_step)?;
    resolve_combats(ctx, logical_step)?;
    finalize_orders(ctx, logical_step)?;

    let config = config(ctx)?;
    if logical_step % u64::from(config.population_step_interval.max(1)) == 0 {
        population_step(ctx, logical_step)?;
    }
    Ok(state(ctx)?.phase == MatchPhase::Running)
}

fn clear_combat_fronts(ctx: &ReducerContext) {
    let keys: Vec<_> = ctx
        .db
        .combat_front()
        .iter()
        .map(|front| front.front_key)
        .collect();
    for key in keys {
        ctx.db.combat_front().front_key().delete(key);
    }
}

fn population_step(ctx: &ReducerContext, logical_step: u64) -> Result<(), String> {
    let config = config(ctx)?;
    let policies: BTreeMap<_, _> = ctx
        .db
        .mobilization_policy()
        .iter()
        .map(|policy| (policy.player_id, policy.target_bps))
        .collect();
    let cells: Vec<_> = ctx.db.cell_state().iter().collect();
    for mut cell in cells {
        let Some(&target_bps) = policies.get(&cell.owner_player_id) else {
            continue;
        };
        if cell.civilian_capacity == 0 {
            continue;
        }
        let missing = cell.civilian_capacity.saturating_sub(cell.civilians);
        if missing > 0 {
            let growth =
                ((u128::from(missing) * u128::from(config.civilian_growth_bps)) / 10_000) as u64;
            cell.civilians = cell.civilians.saturating_add(growth.max(1).min(missing));
        }

        let local_population = cell.civilians.saturating_add(cell.infantry);
        let desired_infantry =
            ((u128::from(local_population) * u128::from(target_bps)) / 10_000) as u64;
        if cell.infantry < desired_infantry {
            let recruit = desired_infantry
                .saturating_sub(cell.infantry)
                .min(config.mobilization_per_population_step)
                .min(cell.civilians)
                .min(cell.military_capacity.saturating_sub(cell.infantry));
            cell.civilians -= recruit;
            cell.infantry += recruit;
        }
        cell.last_changed_step = logical_step;
        ctx.db.cell_state().cell_id().update(cell);
    }
    Ok(())
}

#[derive(Clone)]
struct FriendlyIntent {
    id: u64,
    packet: TransitPacket,
    next_cell: u32,
}

fn move_friendly_packets(ctx: &ReducerContext, logical_step: u64) -> Result<(), String> {
    let mut packets: Vec<_> = ctx.db.transit_packet().iter().collect();
    packets.sort_unstable_by(|left, right| left.packet_key.cmp(&right.packet_key));
    let mut intents = Vec::new();
    for packet in packets {
        let Some(&next_cell) = packet.route.get(packet.route_index as usize + 1) else {
            continue;
        };
        let next = cell_state(ctx, next_cell)?;
        if next.owner_player_id == packet.owner_player_id {
            intents.push(FriendlyIntent {
                id: intents.len() as u64 + 1,
                packet,
                next_cell,
            });
        }
    }
    if intents.is_empty() {
        return Ok(());
    }

    let config = config(ctx)?;
    let movement = MovementConfig {
        max_elevation_step: u16::from(config.max_elevation_step),
        ..MovementConfig::default()
    };
    let logistics = LogisticsConfig {
        default_military_capacity: config.base_military_capacity,
        default_edge_throughput: 0,
        default_combat_frontage: config.base_combat_frontage,
    };
    let mut map = HexMap::new();
    let mut participating = BTreeSet::new();
    for intent in &intents {
        participating.insert(intent.packet.current_cell);
        participating.insert(intent.next_cell);
    }
    for cell_id in participating.iter().copied() {
        map.insert(core_cell(ctx, cell_id)?);
    }
    for intent in &intents {
        let limits = edge_runtime_limits(ctx, intent.packet.current_cell, intent.next_cell)?
            .ok_or_else(|| "active route contains an impassable edge".to_string())?;
        map.set_edge_limits(
            coordinate_for_cell(ctx, intent.packet.current_cell)?,
            coordinate_for_cell(ctx, intent.next_cell)?,
            EdgeLimits {
                throughput: limits.throughput_per_step,
                frontage: limits.frontage,
            },
        );
    }
    let movement_intents: Vec<_> = intents
        .iter()
        .map(|intent| {
            Ok(MovementIntent {
                id: intent.id,
                priority: 0,
                owner: u32::from(intent.packet.owner_player_id),
                from: coordinate_for_cell(ctx, intent.packet.current_cell)?,
                to: coordinate_for_cell(ctx, intent.next_cell)?,
                requested: intent.packet.infantry,
            })
        })
        .collect::<Result<_, String>>()?;
    let result = movement_step(&mut map, &movement_intents, &movement, &logistics)
        .map_err(|error| format!("friendly movement failed: {error:?}"))?;

    for cell_id in participating {
        let coordinate = coordinate_for_cell(ctx, cell_id)?;
        let infantry = map
            .get(coordinate)
            .ok_or_else(|| "movement result omitted a participating cell".to_string())?
            .force();
        let mut row = cell_state(ctx, cell_id)?;
        row.infantry = infantry;
        row.last_changed_step = logical_step;
        ctx.db.cell_state().cell_id().update(row);
    }
    // Commit downstream metadata first. An upstream packet may flow into and
    // merge with the downstream packet's key in the same logical step; doing
    // the downstream update first prevents a stale snapshot from deleting the
    // newly merged strength.
    intents.sort_unstable_by(|left, right| {
        right
            .packet
            .route_index
            .cmp(&left.packet.route_index)
            .then_with(|| left.packet.packet_key.cmp(&right.packet.packet_key))
    });
    for intent in intents {
        let approved = result
            .outcomes
            .get(&intent.id)
            .map(|outcome| outcome.approved)
            .unwrap_or(0);
        if approved > 0 {
            advance_packet(ctx, &intent.packet, approved, logical_step)?;
        }
    }
    Ok(())
}

#[derive(Clone)]
struct FrontPackets {
    attacker: u8,
    from_cell: u32,
    to_cell: u32,
    packets: Vec<TransitPacket>,
}

fn resolve_combats(ctx: &ReducerContext, logical_step: u64) -> Result<(), String> {
    let mut packets: Vec<_> = ctx.db.transit_packet().iter().collect();
    packets.sort_unstable_by(|left, right| left.packet_key.cmp(&right.packet_key));
    let mut targets = BTreeMap::<u32, BTreeMap<u8, BTreeMap<u32, Vec<TransitPacket>>>>::new();
    for packet in packets {
        let Some(&next_cell) = packet.route.get(packet.route_index as usize + 1) else {
            continue;
        };
        if cell_state(ctx, next_cell)?.owner_player_id == packet.owner_player_id {
            continue;
        }
        targets
            .entry(next_cell)
            .or_default()
            .entry(packet.owner_player_id)
            .or_default()
            .entry(packet.current_cell)
            .or_default()
            .push(packet);
    }

    for (target_cell, attackers) in targets {
        if state(ctx)?.phase != MatchPhase::Running {
            break;
        }
        let attackers = refresh_target_attackers(ctx, target_cell, attackers)?;
        if attackers.is_empty() {
            continue;
        }
        let defender = cell_state(ctx, target_cell)?;
        let selected_attacker = if defender.owner_player_id == NEUTRAL_PLAYER {
            attackers
                .iter()
                .max_by(|(left_player, left_fronts), (right_player, right_fronts)| {
                    let left_strength = left_fronts
                        .values()
                        .flatten()
                        .map(|packet| packet.infantry)
                        .sum::<u64>();
                    let right_strength = right_fronts
                        .values()
                        .flatten()
                        .map(|packet| packet.infantry)
                        .sum::<u64>();
                    left_strength
                        .cmp(&right_strength)
                        .then_with(|| right_player.cmp(left_player))
                })
                .map(|(&player, _)| player)
        } else {
            attackers
                .keys()
                .copied()
                .find(|attacker| *attacker != defender.owner_player_id)
        };
        let Some(attacker) = selected_attacker else {
            continue;
        };
        let Some(front_map) = attackers.get(&attacker) else {
            continue;
        };
        let fronts: Vec<_> = front_map
            .iter()
            .map(|(&from_cell, packets)| FrontPackets {
                attacker,
                from_cell,
                to_cell: target_cell,
                packets: packets.clone(),
            })
            .collect();
        resolve_target_combat(ctx, defender, fronts, logical_step)?;
    }
    Ok(())
}

fn refresh_target_attackers(
    ctx: &ReducerContext,
    target_cell: u32,
    candidates: BTreeMap<u8, BTreeMap<u32, Vec<TransitPacket>>>,
) -> Result<BTreeMap<u8, BTreeMap<u32, Vec<TransitPacket>>>, String> {
    let mut current = BTreeMap::<u8, BTreeMap<u32, Vec<TransitPacket>>>::new();
    for (player, fronts) in candidates {
        for (from_cell, packets) in fronts {
            for snapshot in packets {
                let Some(packet) = ctx
                    .db
                    .transit_packet()
                    .packet_key()
                    .find(&snapshot.packet_key)
                else {
                    continue;
                };
                let next_cell = packet.route.get(packet.route_index as usize + 1).copied();
                if packet.owner_player_id != player
                    || packet.current_cell != from_cell
                    || next_cell != Some(target_cell)
                    || cell_state(ctx, from_cell)?.owner_player_id != player
                {
                    continue;
                }
                current
                    .entry(player)
                    .or_default()
                    .entry(from_cell)
                    .or_default()
                    .push(packet);
            }
        }
    }
    Ok(current)
}

fn resolve_target_combat(
    ctx: &ReducerContext,
    mut defender: CellState,
    fronts: Vec<FrontPackets>,
    logical_step: u64,
) -> Result<(), String> {
    let config = config(ctx)?;
    let target_coordinate = coordinate_for_cell(ctx, defender.cell_id)?;
    let target_terrain = terrain(ctx, defender.cell_id)?;
    let mut attacks = Vec::new();
    for front in &fronts {
        let limits = edge_runtime_limits(ctx, front.from_cell, front.to_cell)?
            .ok_or_else(|| "combat route contains an impassable edge".to_string())?;
        attacks.push(AttackFront {
            id: u64::from(front.from_cell),
            attacker: u32::from(front.attacker),
            from: coordinate_for_cell(ctx, front.from_cell)?,
            from_elevation: terrain(ctx, front.from_cell)?.elevation,
            offered: front.packets.iter().map(|packet| packet.infantry).sum(),
            frontage: limits.frontage,
        });
    }
    let combat_config = CombatConfig {
        max_elevation_step: u16::from(config.max_elevation_step),
        uphill_attack_bps: config.uphill_attack_bps,
        attacker_damage_bps: config.combat_lethality_bps,
        defender_damage_bps: config.combat_lethality_bps,
    };
    let resolution = resolve_edge_combat(
        target_coordinate,
        defender.infantry,
        target_terrain.elevation,
        &attacks,
        &combat_config,
    )
    .map_err(|error| format!("combat resolution failed: {error:?}"))?;

    let total_engaged: u64 = resolution
        .attacks
        .values()
        .map(|outcome| outcome.engaged)
        .sum();
    let extra_defender_casualty = u64::from(
        defender.infantry > 0 && total_engaged > 0 && resolution.defender_casualties == 0,
    );
    let defender_casualties = resolution
        .defender_casualties
        .saturating_add(extra_defender_casualty)
        .min(defender.infantry);

    let mut surviving_by_front = BTreeMap::new();
    for front in &fronts {
        let outcome = resolution
            .attacks
            .get(&u64::from(front.from_cell))
            .ok_or_else(|| "combat omitted an attack front".to_string())?;
        let extra_attacker = u64::from(
            outcome.engaged > 0
                && outcome.defense_allocated > 0
                && outcome.attacker_casualties == 0,
        );
        let attacker_casualties = outcome
            .attacker_casualties
            .saturating_add(extra_attacker)
            .min(outcome.offered);
        apply_attacker_casualties(ctx, &front.packets, attacker_casualties, logical_step)?;
        surviving_by_front.insert(
            front.from_cell,
            outcome.offered.saturating_sub(attacker_casualties),
        );
        let limits = edge_runtime_limits(ctx, front.from_cell, front.to_cell)?
            .ok_or_else(|| "combat route became impassable".to_string())?;
        let front_defender_casualties = outcome.defender_casualties
            + u64::from(extra_defender_casualty > 0 && front.from_cell == fronts[0].from_cell);
        ctx.db.combat_front().insert(CombatFront {
            front_key: format!("{}:{}:{}", front.attacker, front.from_cell, front.to_cell),
            attacker_player_id: front.attacker,
            defender_player_id: defender.owner_player_id,
            from_cell: front.from_cell,
            to_cell: front.to_cell,
            queued_infantry: outcome.waiting,
            attacker_engaged: outcome.engaged,
            defender_engaged: outcome.defense_allocated,
            attacker_casualties,
            defender_casualties: front_defender_casualties,
            frontage: limits.frontage,
            uphill: limits.uphill,
            logical_step,
        });
    }

    defender.infantry = defender.infantry.saturating_sub(defender_casualties);
    defender.last_changed_step = logical_step;
    ctx.db.cell_state().cell_id().update(defender.clone());
    trim_packets_at_cell(
        ctx,
        defender.cell_id,
        defender.owner_player_id,
        logical_step,
    )?;

    if defender.infantry == 0 {
        let capturing_front = surviving_by_front
            .iter()
            .filter(|(_, strength)| **strength > 0)
            .max_by(|(left_cell, left_strength), (right_cell, right_strength)| {
                left_strength
                    .cmp(right_strength)
                    .then_with(|| right_cell.cmp(left_cell))
            })
            .map(|(&cell, _)| cell);
        if let Some(from_cell) = capturing_front {
            let front = fronts
                .iter()
                .find(|front| front.from_cell == from_cell)
                .ok_or_else(|| "capturing front is missing".to_string())?;
            occupy_after_combat(ctx, front, defender, logical_step)?;
        }
    }
    Ok(())
}

fn apply_attacker_casualties(
    ctx: &ReducerContext,
    packets: &[TransitPacket],
    mut casualties: u64,
    logical_step: u64,
) -> Result<(), String> {
    let mut sorted = packets.to_vec();
    sorted.sort_unstable_by(|left, right| left.packet_key.cmp(&right.packet_key));
    for packet in sorted {
        if casualties == 0 {
            break;
        }
        let Some(current) = ctx
            .db
            .transit_packet()
            .packet_key()
            .find(&packet.packet_key)
        else {
            continue;
        };
        let lost = casualties.min(current.infantry);
        let mut source_state = cell_state(ctx, current.current_cell)?;
        source_state.infantry = source_state.infantry.saturating_sub(lost);
        source_state.last_changed_step = logical_step;
        ctx.db.cell_state().cell_id().update(source_state);
        reduce_packet_metadata(ctx, current, lost, logical_step, true)?;
        casualties -= lost;
    }
    Ok(())
}

fn occupy_after_combat(
    ctx: &ReducerContext,
    front: &FrontPackets,
    mut target: CellState,
    logical_step: u64,
) -> Result<(), String> {
    let limits = edge_runtime_limits(ctx, front.from_cell, front.to_cell)?
        .ok_or_else(|| "capturing edge is impassable".to_string())?;
    let mut packets: Vec<_> = front
        .packets
        .iter()
        .filter_map(|packet| {
            ctx.db
                .transit_packet()
                .packet_key()
                .find(&packet.packet_key)
        })
        .collect();
    packets.sort_unstable_by(|left, right| left.packet_key.cmp(&right.packet_key));
    let offered: u64 = packets.iter().map(|packet| packet.infantry).sum();
    let occupancy = offered
        .min(limits.throughput_per_step)
        .min(target.military_capacity.saturating_sub(target.infantry));
    if occupancy == 0 {
        return Ok(());
    }

    let old_owner = target.owner_player_id;
    target.owner_player_id = front.attacker;
    target.infantry = target.infantry.saturating_add(occupancy);
    target.last_changed_step = logical_step;
    ctx.db.cell_state().cell_id().update(target.clone());

    let mut remaining = occupancy;
    for packet in packets {
        if remaining == 0 {
            break;
        }
        let moved = remaining.min(packet.infantry);
        let mut source = cell_state(ctx, packet.current_cell)?;
        source.infantry = source.infantry.saturating_sub(moved);
        source.last_changed_step = logical_step;
        ctx.db.cell_state().cell_id().update(source);
        advance_packet(ctx, &packet, moved, logical_step)?;
        remaining -= moved;
    }
    station_capture_garrison(ctx, target.cell_id, front.attacker, logical_step)?;
    record_capture(ctx, target.cell_id, old_owner, front.attacker)?;
    Ok(())
}

fn record_capture(
    ctx: &ReducerContext,
    cell_id: u32,
    old_owner: u8,
    new_owner: u8,
) -> Result<(), String> {
    if old_owner == new_owner || !terrain(ctx, cell_id)?.capturable {
        return Ok(());
    }
    let mut match_state = state(ctx)?;
    match old_owner {
        PLAYER_ONE => {
            match_state.player_one_controlled = match_state.player_one_controlled.saturating_sub(1);
        }
        PLAYER_TWO => {
            match_state.player_two_controlled = match_state.player_two_controlled.saturating_sub(1);
        }
        _ => {}
    }
    match new_owner {
        PLAYER_ONE => match_state.player_one_controlled += 1,
        PLAYER_TWO => match_state.player_two_controlled += 1,
        _ => {}
    }
    let controlled = if new_owner == PLAYER_ONE {
        match_state.player_one_controlled
    } else {
        match_state.player_two_controlled
    };
    if controlled >= match_state.required_control {
        match_state.phase = MatchPhase::Completed;
        match_state.winner_player_id = new_owner;
        match_state.completed_at_us = crate::timestamp_us(ctx);
    }
    ctx.db.match_state().singleton_id().update(match_state);
    Ok(())
}

fn advance_packet(
    ctx: &ReducerContext,
    packet: &TransitPacket,
    moved: u64,
    logical_step: u64,
) -> Result<(), String> {
    let Some(mut packet) = ctx
        .db
        .transit_packet()
        .packet_key()
        .find(&packet.packet_key)
    else {
        return Ok(());
    };
    if moved == 0 || moved > packet.infantry {
        return Err("invalid packet movement amount".into());
    }
    let next_index = packet.route_index + 1;
    let next_cell = *packet
        .route
        .get(next_index as usize)
        .ok_or_else(|| "packet route ended before movement".to_string())?;

    if next_index as usize + 1 == packet.route.len() {
        extend_push_lane(ctx, &packet, next_cell, logical_step)?;
        packet = ctx
            .db
            .transit_packet()
            .packet_key()
            .find(&packet.packet_key)
            .ok_or_else(|| "push packet disappeared while extending its lane".to_string())?;
    }

    if moved == packet.infantry {
        ctx.db
            .transit_packet()
            .packet_key()
            .delete(&packet.packet_key);
    } else {
        let mut remainder = packet.clone();
        remainder.infantry -= moved;
        remainder.updated_step = logical_step;
        ctx.db.transit_packet().packet_key().update(remainder);
    }
    if packet.route_index == 0 {
        decrement_source_queue(ctx, packet.order_id, packet.origin_cell, moved)?;
    }

    if next_index as usize + 1 == packet.route.len() {
        increment_destination_received(ctx, packet.order_id, packet.destination_cell, moved)?;
        settle_stopped_push_lane(ctx, packet.order_id, packet.destination_cell, logical_step)?;
        return Ok(());
    }

    let child_key = packet_key(
        packet.order_id,
        packet.origin_cell,
        packet.destination_cell,
        next_cell,
        next_index,
    );
    if let Some(mut existing) = ctx.db.transit_packet().packet_key().find(&child_key) {
        existing.infantry = existing
            .infantry
            .checked_add(moved)
            .ok_or_else(|| "packet strength overflow".to_string())?;
        existing.updated_step = logical_step;
        ctx.db.transit_packet().packet_key().update(existing);
    } else {
        ctx.db.transit_packet().insert(TransitPacket {
            packet_key: child_key,
            order_id: packet.order_id,
            owner_player_id: packet.owner_player_id,
            origin_cell: packet.origin_cell,
            current_cell: next_cell,
            destination_cell: packet.destination_cell,
            infantry: moved,
            route_index: next_index,
            route: packet.route.clone(),
            updated_step: logical_step,
        });
    }
    Ok(())
}

/// Extends every packet in one push lane by exactly one outward cell.
///
/// `destination_cell` is deliberately kept as the stable lane anchor. This
/// allows packets from different selected source cells to share one advancing
/// ray without rewriting packet keys or destination metadata every layer.
fn extend_push_lane(
    ctx: &ReducerContext,
    packet: &TransitPacket,
    reached_cell: u32,
    logical_step: u64,
) -> Result<bool, String> {
    let Some(order) = ctx.db.transfer_order().order_id().find(packet.order_id) else {
        return Err("push order is missing while extending a lane".into());
    };
    if order.status != OrderStatus::Active || order.kind != OrderKind::PushFront {
        return Ok(false);
    }

    let current = ctx
        .db
        .transit_packet()
        .packet_key()
        .find(&packet.packet_key)
        .ok_or_else(|| "push packet is missing while extending a lane".to_string())?;
    if current.route.last().copied() != Some(reached_cell) {
        return Ok(true);
    }

    let direction = hex_core::Axial::new(order.orientation_q, order.orientation_r);
    if !hex_core::Axial::DIRECTIONS.contains(&direction) {
        return Err("active push order has an invalid direction".into());
    }
    let match_config = config(ctx)?;
    let next_coordinate = coordinate_for_cell(ctx, reached_cell)? + direction;
    let Some(next_cell) = cell_id_for_coordinate(&match_config, next_coordinate) else {
        return Ok(false);
    };
    let next_terrain = terrain(ctx, next_cell)?;
    let next_state = cell_state(ctx, next_cell)?;
    let traversable = edge_runtime_limits(ctx, reached_cell, next_cell)?.is_some();
    if !push_target_is_eligible(
        order.player_id,
        next_state.owner_player_id,
        next_terrain.passable,
        next_terrain.capturable,
        traversable,
    ) {
        return Ok(false);
    }

    let packets = ctx
        .db
        .transit_packet()
        .packet_by_order()
        .filter(order.order_id)
        .filter(|candidate| candidate.destination_cell == packet.destination_cell)
        .collect::<Vec<_>>();
    let mut extended = false;
    for mut candidate in packets {
        if !append_lane_layer(&mut candidate.route, reached_cell, next_cell) {
            continue;
        }
        candidate.updated_step = logical_step;
        ctx.db.transit_packet().packet_key().update(candidate);
        extended = true;
    }
    Ok(extended)
}

fn push_target_is_eligible(
    player_id: u8,
    target_owner: u8,
    passable: bool,
    capturable: bool,
    traversable: bool,
) -> bool {
    passable && capturable && traversable && target_owner != player_id
}

fn append_lane_layer(route: &mut Vec<u32>, reached_cell: u32, next_cell: u32) -> bool {
    if route.last().copied() != Some(reached_cell) {
        return false;
    }
    route.push(next_cell);
    true
}

fn decrement_source_queue(
    ctx: &ReducerContext,
    order_id: u64,
    origin_cell: u32,
    amount: u64,
) -> Result<(), String> {
    let key = format!("{order_id}:{origin_cell}");
    let mut source = ctx
        .db
        .transfer_source()
        .source_key()
        .find(&key)
        .ok_or_else(|| "transfer source row is missing".to_string())?;
    source.queued_infantry = source.queued_infantry.saturating_sub(amount);
    ctx.db.transfer_source().source_key().update(source);
    Ok(())
}

fn increment_destination_received(
    ctx: &ReducerContext,
    order_id: u64,
    destination_cell: u32,
    amount: u64,
) -> Result<(), String> {
    let key = format!("{order_id}:{destination_cell}");
    let mut destination = ctx
        .db
        .transfer_destination()
        .destination_key()
        .find(&key)
        .ok_or_else(|| "transfer destination row is missing".to_string())?;
    destination.received_infantry = destination
        .received_infantry
        .saturating_add(amount)
        .min(destination.target_infantry);
    ctx.db
        .transfer_destination()
        .destination_key()
        .update(destination);
    let mut order = ctx
        .db
        .transfer_order()
        .order_id()
        .find(order_id)
        .ok_or_else(|| "transfer order is missing".to_string())?;
    order.delivered_infantry = order.delivered_infantry.saturating_add(amount);
    ctx.db.transfer_order().order_id().update(order);
    Ok(())
}

fn station_capture_garrison(
    ctx: &ReducerContext,
    cell_id: u32,
    owner_player_id: u8,
    logical_step: u64,
) -> Result<(), String> {
    let cell = cell_state(ctx, cell_id)?;
    let terrain_class = terrain(ctx, cell_id)?.terrain;
    let required = occupation_garrison(cell.military_capacity, terrain_class).min(cell.infantry);
    let allocated = allocated_infantry_at_cell(ctx, owner_player_id, cell_id);
    let mut remaining = additional_garrison_required(required, cell.infantry, allocated);
    if remaining == 0 {
        return Ok(());
    }

    let mut packets = ctx
        .db
        .transit_packet()
        .packet_by_cell()
        .filter(cell_id)
        .filter(|packet| packet.owner_player_id == owner_player_id)
        .filter(|packet| {
            ctx.db
                .transfer_order()
                .order_id()
                .find(packet.order_id)
                .is_some_and(|order| order.status == OrderStatus::Active)
        })
        .collect::<Vec<_>>();
    packets.sort_unstable_by(|left, right| left.packet_key.cmp(&right.packet_key));
    for packet in packets {
        if remaining == 0 {
            break;
        }
        let stationed = remaining.min(packet.infantry);
        station_packet_allocation(ctx, &packet, stationed, logical_step)?;
        remaining -= stationed;
    }
    Ok(())
}

fn additional_garrison_required(required: u64, total: u64, allocated: u64) -> u64 {
    required.saturating_sub(total.saturating_sub(allocated))
}

fn occupation_garrison(military_capacity: u64, terrain: TerrainClass) -> u64 {
    if military_capacity == 0 || terrain == TerrainClass::Water {
        return 0;
    }
    let base = military_capacity.div_ceil(20).max(1);
    let multiplier = match terrain {
        TerrainClass::Plains => 1,
        TerrainClass::Hills => 2,
        TerrainClass::Mountain => 3,
        TerrainClass::Water => 0,
    };
    base.saturating_mul(multiplier).min(military_capacity)
}

/// Releases every still-allocated survivor in a lane where the directional
/// ray reached friendly territory, the map boundary, or an impassable edge.
/// The underlying infantry remains in its current cell; only the operation's
/// allocation metadata is retired.
fn settle_stopped_push_lane(
    ctx: &ReducerContext,
    order_id: u64,
    lane_anchor: u32,
    logical_step: u64,
) -> Result<(), String> {
    let Some(order) = ctx.db.transfer_order().order_id().find(order_id) else {
        return Err("order is missing while settling a stopped push lane".into());
    };
    if order.kind != OrderKind::PushFront {
        return Ok(());
    }
    let mut packets = ctx
        .db
        .transit_packet()
        .packet_by_order()
        .filter(order_id)
        .filter(|packet| packet.destination_cell == lane_anchor)
        .collect::<Vec<_>>();
    packets.sort_unstable_by(|left, right| left.packet_key.cmp(&right.packet_key));
    for packet in packets {
        station_packet_allocation(ctx, &packet, packet.infantry, logical_step)?;
    }
    Ok(())
}

/// Retires an allocation without removing infantry from the cell. The public
/// `delivered_infantry` counter therefore means all surviving strength that
/// has left this operation: endpoint arrivals, occupation garrisons, and
/// release-in-place at an automatic stop.
fn station_packet_allocation(
    ctx: &ReducerContext,
    packet: &TransitPacket,
    amount: u64,
    logical_step: u64,
) -> Result<(), String> {
    let Some(mut current) = ctx
        .db
        .transit_packet()
        .packet_key()
        .find(&packet.packet_key)
    else {
        return Ok(());
    };
    let stationed = amount.min(current.infantry);
    if stationed == 0 {
        return Ok(());
    }
    if stationed == current.infantry {
        ctx.db
            .transit_packet()
            .packet_key()
            .delete(&current.packet_key);
    } else {
        current.infantry -= stationed;
        current.updated_step = logical_step;
        ctx.db.transit_packet().packet_key().update(current.clone());
    }
    if current.route_index == 0 {
        decrement_source_queue(ctx, current.order_id, current.origin_cell, stationed)?;
    }
    let mut order = ctx
        .db
        .transfer_order()
        .order_id()
        .find(current.order_id)
        .ok_or_else(|| "order is missing while stationing infantry".to_string())?;
    order.delivered_infantry = order
        .delivered_infantry
        .checked_add(stationed)
        .ok_or_else(|| "stationed infantry overflow".to_string())?;
    order.updated_step = logical_step;
    ctx.db.transfer_order().order_id().update(order);
    Ok(())
}

fn reduce_packet_metadata(
    ctx: &ReducerContext,
    mut packet: TransitPacket,
    amount: u64,
    logical_step: u64,
    count_casualty: bool,
) -> Result<(), String> {
    let lost = amount.min(packet.infantry);
    if lost == packet.infantry {
        ctx.db
            .transit_packet()
            .packet_key()
            .delete(&packet.packet_key);
    } else {
        packet.infantry -= lost;
        packet.updated_step = logical_step;
        ctx.db.transit_packet().packet_key().update(packet.clone());
    }
    if packet.route_index == 0 {
        decrement_source_queue(ctx, packet.order_id, packet.origin_cell, lost)?;
    }
    if count_casualty {
        let mut order = ctx
            .db
            .transfer_order()
            .order_id()
            .find(packet.order_id)
            .ok_or_else(|| "casualty order is missing".to_string())?;
        order.casualty_infantry = order.casualty_infantry.saturating_add(lost);
        order.updated_step = logical_step;
        ctx.db.transfer_order().order_id().update(order);
    }
    Ok(())
}

fn trim_packets_at_cell(
    ctx: &ReducerContext,
    cell_id: u32,
    owner_player_id: u8,
    logical_step: u64,
) -> Result<(), String> {
    let infantry = cell_state(ctx, cell_id)?.infantry;
    let mut packets: Vec<_> = ctx
        .db
        .transit_packet()
        .packet_by_cell()
        .filter(cell_id)
        .filter(|packet| packet.owner_player_id == owner_player_id)
        .collect();
    packets.sort_unstable_by(|left, right| right.packet_key.cmp(&left.packet_key));
    let allocated: u64 = packets.iter().map(|packet| packet.infantry).sum();
    let mut trim = allocated.saturating_sub(infantry);
    for packet in packets {
        if trim == 0 {
            break;
        }
        let lost = trim.min(packet.infantry);
        reduce_packet_metadata(ctx, packet, lost, logical_step, true)?;
        trim -= lost;
    }
    Ok(())
}

fn trim_all_overallocated_packets(ctx: &ReducerContext, logical_step: u64) -> Result<(), String> {
    let locations: BTreeSet<_> = ctx
        .db
        .transit_packet()
        .iter()
        .map(|packet| (packet.current_cell, packet.owner_player_id))
        .collect();
    for (cell_id, owner) in locations {
        trim_packets_at_cell(ctx, cell_id, owner, logical_step)?;
    }
    Ok(())
}

fn finalize_orders(ctx: &ReducerContext, logical_step: u64) -> Result<(), String> {
    let mut active_strength = BTreeMap::<u64, u64>::new();
    for packet in ctx.db.transit_packet().iter() {
        *active_strength.entry(packet.order_id).or_default() += packet.infantry;
    }
    let orders: Vec<TransferOrder> = ctx
        .db
        .transfer_order()
        .order_by_status()
        .filter(OrderStatus::Active)
        .collect();
    for mut order in orders {
        order.in_transit_infantry = active_strength.get(&order.order_id).copied().unwrap_or(0);
        if order.in_transit_infantry == 0 {
            order.status = OrderStatus::Completed;
        }
        order.updated_step = logical_step;
        let accounted = order
            .in_transit_infantry
            .saturating_add(order.delivered_infantry)
            .saturating_add(order.casualty_infantry);
        if accounted != order.committed_infantry {
            return Err(format!(
                "order {} violates infantry conservation: committed {}, accounted {}",
                order.order_id, order.committed_infantry, accounted
            ));
        }
        ctx.db.transfer_order().order_id().update(order);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_lane_extends_through_successive_layers_without_touching_its_neighbor() {
        let mut first_lane = vec![1, 2, 3];
        let mut second_lane = vec![10, 11];

        assert!(append_lane_layer(&mut first_lane, 3, 4));
        assert!(append_lane_layer(&mut first_lane, 4, 5));
        assert!(!append_lane_layer(&mut second_lane, 3, 4));
        assert_eq!(first_lane, vec![1, 2, 3, 4, 5]);
        assert_eq!(second_lane, vec![10, 11]);
    }

    #[test]
    fn a_push_stops_before_friendly_impassable_or_uncapturable_ground() {
        assert!(push_target_is_eligible(1, 0, true, true, true));
        assert!(push_target_is_eligible(1, 2, true, true, true));
        assert!(!push_target_is_eligible(1, 1, true, true, true));
        assert!(!push_target_is_eligible(1, 0, false, true, true));
        assert!(!push_target_is_eligible(1, 0, true, false, true));
        assert!(!push_target_is_eligible(1, 0, true, true, false));
    }

    #[test]
    fn occupation_garrison_is_terrain_scaled_and_capacity_bounded() {
        assert_eq!(occupation_garrison(100, TerrainClass::Plains), 5);
        assert_eq!(occupation_garrison(80, TerrainClass::Hills), 8);
        assert_eq!(occupation_garrison(60, TerrainClass::Mountain), 9);
        assert_eq!(occupation_garrison(1, TerrainClass::Mountain), 1);
        assert_eq!(occupation_garrison(0, TerrainClass::Plains), 0);
        assert_eq!(occupation_garrison(100, TerrainClass::Water), 0);
    }

    #[test]
    fn garrison_and_surplus_exactly_partition_a_captured_wave() {
        for occupancy in 0..=100_u64 {
            let stationed = occupancy.min(occupation_garrison(100, TerrainClass::Plains));
            let continuing = occupancy - stationed;
            assert_eq!(stationed + continuing, occupancy);
            assert!(stationed <= 5);
        }
    }

    #[test]
    fn existing_unallocated_strength_counts_toward_a_capture_garrison() {
        assert_eq!(additional_garrison_required(10, 30, 30), 10);
        assert_eq!(additional_garrison_required(10, 30, 25), 5);
        assert_eq!(additional_garrison_required(10, 30, 20), 0);
        assert_eq!(additional_garrison_required(10, 30, 0), 0);
    }
}
