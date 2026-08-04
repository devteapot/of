//! Narrow authority boundary for the native client.
//!
//! Rendering and input emit [`ClientIntent`] and consume [`ServerUpdate`]. The
//! online adapter and offline fixture both translate into this boundary, so
//! camera, selection, overlays, and HUD systems remain transport-agnostic.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use bevy::prelude::*;
use hex_core::{
    Axial, BALANCE_WEIGHT, Cell, DirectedFrontEdge, DistributionError,
    DistributionPreset as CoreDistributionPreset, ForceComposition, FrontSelectionError, HexMap,
    LocalFrontRoute, LogisticsConfig, MovementConfig, MovementIntent, focus_branch_weight,
    ground_traversal, movement_step, redistribution_targets_with_commitment,
    redistribution_targets_with_fallback_constraints, selected_all_front_edges,
    selected_directional_routes, selected_front_edges, selected_local_front_routes,
    weighted_branch_quotas_rotated,
};

pub use crate::model::ClusterPolicy;
use crate::model::{
    ActiveFlow, ActiveFront, ClusterPolicyView, MatchPhase, MatchView,
    OrderSelectionProjectionError, ProjectedOrderSelection, ToastKind,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RedistributionPreset {
    Balance,
    FrontLoad,
    CoreLoad,
    PerimeterLoad,
}

#[derive(Message, Clone, Debug)]
pub enum ClientIntent {
    /// Cluster-first neutral expansion. `sources` may contain several complete
    /// owned components; `focus` mildly weights every branch toward the click
    /// while retaining a positive allocation on the rest of the perimeter.
    ExpandClusters {
        sources: BTreeSet<Axial>,
        focus: Axial,
        commitment_percent: u8,
    },
    /// Cluster-first attack against the snapshotted union of one or more enemy
    /// connected components. The authoritative wave recomputes its exposed
    /// fronts as cells inside `targets` change ownership.
    AttackClusters {
        sources: BTreeSet<Axial>,
        targets: BTreeSet<Axial>,
        commitment_percent: u8,
    },
    /// Changes metadata for every complete owned cluster touched by `sources`.
    /// This never retasks active action packets; the background policy solves
    /// only their currently free strength in residual capacity.
    SetClusterPolicy {
        sources: BTreeSet<Axial>,
        policy: ClusterPolicy,
        direction: Option<Axial>,
    },
    PushFront {
        sources: BTreeSet<Axial>,
        supersede_order_ids: BTreeSet<u64>,
        direction: Axial,
        commitment_percent: u8,
    },
    ExpandAll {
        sources: BTreeSet<Axial>,
        supersede_order_ids: BTreeSet<u64>,
        commitment_percent: u8,
    },
    Reshape {
        sources: BTreeSet<Axial>,
        targets: BTreeSet<Axial>,
        supersede_order_ids: BTreeSet<u64>,
    },
    CancelOrders {
        order_ids: BTreeSet<u64>,
    },
    Redistribute {
        cells: BTreeSet<Axial>,
        supersede_order_ids: BTreeSet<u64>,
        preset: RedistributionPreset,
        /// Exact fixed-point axial orientation for directional Bias. This is
        /// intentionally not restricted to the six neighboring Push axes.
        direction: Option<Axial>,
    },
    SetMobilization {
        target: f32,
    },
}

#[derive(Clone, Debug)]
pub struct CellPatch {
    pub coordinate: Axial,
    pub owner: Option<u32>,
    pub infantry: u64,
}

#[derive(Message, Clone, Debug)]
pub enum ServerUpdate {
    SubmissionStarted {
        command_id: u64,
    },
    Accepted {
        command_id: Option<u64>,
        summary: String,
        patches: Vec<CellPatch>,
        flow: Option<ActiveFlow>,
        front: Option<ActiveFront>,
    },
    MobilizationChanged {
        command_id: Option<u64>,
        target: f32,
    },
    Rejected {
        command_id: Option<u64>,
        reason: String,
        relevant_cell: Option<Axial>,
    },
}

pub struct NetworkBoundaryPlugin;

#[derive(SystemSet, Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub enum NetworkSet {
    Transport,
    Apply,
}

impl Plugin for NetworkBoundaryPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<ClientIntent>()
            .add_message::<ServerUpdate>()
            .configure_sets(Update, (NetworkSet::Transport, NetworkSet::Apply).chain());
    }
}

/// Local authority used only when `--offline` or `OF_OFFLINE=1` is explicit.
pub struct OfflineTransportPlugin;

impl Plugin for OfflineTransportPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            resolve_offline_intents.in_set(NetworkSet::Transport),
        );
    }
}

pub fn resolve_offline_intents(
    mut intents: MessageReader<ClientIntent>,
    mut view: ResMut<MatchView>,
    mut updates: MessageWriter<ServerUpdate>,
) {
    for intent in intents.read() {
        let update = match intent {
            ClientIntent::ExpandClusters {
                sources,
                focus,
                commitment_percent,
            } => resolve_expand_clusters(&view, sources, *focus, *commitment_percent),
            ClientIntent::AttackClusters {
                sources,
                targets,
                commitment_percent,
            } => resolve_attack_clusters(&view, sources, targets, *commitment_percent),
            ClientIntent::SetClusterPolicy {
                sources,
                policy,
                direction,
            } => {
                let update = resolve_cluster_policy(&view, sources, *policy, *direction);
                if matches!(update, ServerUpdate::Accepted { .. }) {
                    persist_offline_cluster_policy(&mut view, sources, *policy, *direction);
                }
                update
            }
            ClientIntent::PushFront {
                sources,
                supersede_order_ids,
                direction,
                commitment_percent,
            } => resolve_push_front_with_retask(
                &view,
                sources,
                supersede_order_ids,
                *direction,
                *commitment_percent,
            ),
            ClientIntent::ExpandAll {
                sources,
                supersede_order_ids,
                commitment_percent,
            } => resolve_expand_all_with_retask(
                &view,
                sources,
                supersede_order_ids,
                *commitment_percent,
            ),
            ClientIntent::Reshape {
                sources,
                targets,
                supersede_order_ids,
            } => resolve_reshape_with_retask(&view, sources, targets, supersede_order_ids),
            ClientIntent::CancelOrders { .. } => ServerUpdate::Rejected {
                command_id: None,
                reason: "Stopping exact orders requires an authoritative online match".to_owned(),
                relevant_cell: None,
            },
            ClientIntent::Redistribute {
                cells,
                supersede_order_ids,
                preset,
                direction,
            } => resolve_redistribution_with_retask(
                &view,
                cells,
                supersede_order_ids,
                *preset,
                *direction,
            ),
            ClientIntent::SetMobilization { target } => ServerUpdate::MobilizationChanged {
                command_id: None,
                target: target.clamp(0.0, 1.0),
            },
        };
        updates.write(update);
    }
}

fn resolve_cluster_policy(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    policy: ClusterPolicy,
    direction: Option<Axial>,
) -> ServerUpdate {
    if sources.is_empty() {
        return rejection("Cluster policy needs at least one selected cluster", None);
    }
    if let Some(invalid) = sources
        .iter()
        .find(|coordinate| !view.is_local_owned_passable(**coordinate))
    {
        return rejection(
            "Cluster policy sources must be owned passable ground",
            Some(*invalid),
        );
    }
    if policy == ClusterPolicy::Directional
        && direction.is_none_or(|orientation| orientation == Axial::ZERO)
    {
        return rejection(
            "Directional cluster policy needs a visible orientation",
            sources.first().copied(),
        );
    }
    if policy != ClusterPolicy::Directional && direction.is_some() {
        return rejection(
            "Only the directional cluster policy accepts an orientation",
            sources.first().copied(),
        );
    }
    let complete_sources = offline_local_clusters(view)
        .into_iter()
        .filter(|cluster| !cluster.is_disjoint(sources))
        .flatten()
        .collect::<BTreeSet<_>>();
    let assignment = ClusterPolicyView {
        kind: policy,
        orientation: direction.unwrap_or(Axial::ZERO),
        revision: 0,
    };
    // Setting metadata is authoritative; its first redistribution is only a
    // best-effort convenience. In particular, an active action reservation
    // must not make the persistent policy edit itself fail.
    let patches = offline_policy_maintenance_targets(view, &complete_sources, assignment)
        .unwrap_or_default()
        .into_iter()
        .map(|(coordinate, infantry)| CellPatch {
            coordinate,
            owner: Some(view.local_player),
            infantry,
        })
        .collect();
    ServerUpdate::Accepted {
        command_id: None,
        summary: format!(
            "Cluster policy set to {} · free troops redistributed · active dispatches remain committed",
            policy.label()
        ),
        patches,
        flow: None,
        front: None,
    }
}

fn persist_offline_cluster_policy(
    view: &mut MatchView,
    seeds: &BTreeSet<Axial>,
    policy: ClusterPolicy,
    direction: Option<Axial>,
) {
    let revision = view
        .cluster_policies
        .values()
        .map(|assignment| assignment.revision)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    let orientation = direction.unwrap_or(Axial::ZERO);
    let selected = offline_local_clusters(view)
        .into_iter()
        .filter(|cluster| !cluster.is_disjoint(seeds))
        .flatten()
        .collect::<BTreeSet<_>>();
    for coordinate in selected {
        view.cluster_policies.insert(
            coordinate,
            ClusterPolicyView {
                kind: policy,
                orientation,
                revision,
            },
        );
    }
}

fn offline_local_clusters(view: &MatchView) -> Vec<BTreeSet<Axial>> {
    let mut remaining = view
        .cells
        .keys()
        .filter(|coordinate| view.is_local_owned_passable(**coordinate))
        .copied()
        .collect::<BTreeSet<_>>();
    let mut clusters = Vec::new();
    while let Some(seed) = remaining.pop_first() {
        let mut cluster = BTreeSet::from([seed]);
        let mut pending = VecDeque::from([seed]);
        while let Some(current) = pending.pop_front() {
            for neighbor in current.neighbors() {
                if remaining.contains(&neighbor)
                    && view.is_local_traversable_edge(current, neighbor)
                    && remaining.remove(&neighbor)
                {
                    cluster.insert(neighbor);
                    pending.push_back(neighbor);
                }
            }
        }
        clusters.push(cluster);
    }
    clusters
}

fn reconcile_offline_cluster_policies(view: &mut MatchView) {
    let existing = view.cluster_policies.clone();
    let mut reconciled = BTreeMap::new();
    for cluster in offline_local_clusters(view) {
        let winner = cluster
            .iter()
            .filter_map(|coordinate| {
                existing
                    .get(coordinate)
                    .copied()
                    .map(|policy| (*coordinate, policy))
            })
            .max_by_key(|(coordinate, policy)| (policy.revision, *coordinate))
            .map_or(ClusterPolicyView::BALANCED_DEFAULT, |(_, policy)| policy);
        reconciled.extend(cluster.into_iter().map(|coordinate| (coordinate, winner)));
    }
    view.cluster_policies = reconciled;
}

/// Re-applies every surviving offline cluster policy after a local action has
/// changed strength or ownership. The real authority persists background
/// redistribution orders and retries them over time; the offline fixture has
/// no simulation clock for those orders, so it performs one deterministic
/// best-effort transfer immediately.
///
/// Live action strength remains fixed at its current cell and reduces that
/// cell's usable capacity. Outstanding inbound reservations also reduce the
/// amount moved into a destination. A malformed projection or temporarily
/// impossible distribution simply leaves that component alone: maintenance
/// must never turn an already accepted action into a rejection.
fn maintain_offline_cluster_policies(view: &mut MatchView) {
    for cluster in offline_local_clusters(view) {
        let Some(policy) = cluster
            .first()
            .and_then(|coordinate| view.cluster_policy_at(*coordinate))
        else {
            continue;
        };
        let Ok(targets) = offline_policy_maintenance_targets(view, &cluster, policy) else {
            continue;
        };
        for (coordinate, infantry) in targets {
            let Some(cell) = view.cell_mut(coordinate) else {
                continue;
            };
            cell.infantry = infantry.min(cell.military_capacity);
        }
    }
}

fn offline_policy_maintenance_targets(
    view: &MatchView,
    cluster: &BTreeSet<Axial>,
    policy: ClusterPolicyView,
) -> Result<BTreeMap<Axial, u64>, &'static str> {
    let preset = match policy.kind {
        ClusterPolicy::Balanced => CoreDistributionPreset::Balance,
        ClusterPolicy::Center => CoreDistributionPreset::CoreLoad,
        ClusterPolicy::Perimeter => CoreDistributionPreset::PerimeterLoad,
        ClusterPolicy::Directional if policy.orientation != Axial::ZERO => {
            CoreDistributionPreset::front_load(policy.orientation)
        }
        ClusterPolicy::Directional => return Err("directional policy has no orientation"),
    };
    let projection = view
        .project_order_selection(cluster, &BTreeSet::new())
        .map_err(|_| "cluster policy projection is stale")?;

    let mut map = HexMap::new();
    let mut free_strength = 0_u64;
    for &coordinate in cluster {
        let cell = view
            .cell(coordinate)
            .ok_or("cluster policy cell is missing")?;
        let affected = projection
            .affected_strength_by_cell
            .get(&coordinate)
            .copied()
            .unwrap_or(0);
        let fixed = projection
            .unaffected_strength_by_cell
            .get(&coordinate)
            .copied()
            .unwrap_or(0);
        free_strength = free_strength
            .checked_add(affected)
            .ok_or("cluster policy strength overflow")?;
        map.insert(projected_cell(cell, affected, fixed));
    }
    let distribution = redistribution_targets_with_commitment(
        &map,
        view.local_player,
        cluster.iter().copied(),
        free_strength,
        preset,
        10_000,
    )
    .map_err(|_| "cluster policy distribution is temporarily unavailable")?;

    // Authority subtracts unrelated inbound reservations from each desired
    // increase before planning transfer legs. Reproduce that behavior instead
    // of applying the ideal heatmap outright: source surplus with no currently
    // safe destination simply stays where it is until the next maintenance.
    let mut desired = BTreeMap::new();
    let mut demands = BTreeMap::new();
    let mut requested = 0_u64;
    for (&coordinate, &affected_target) in &distribution.targets {
        let current = view
            .cell(coordinate)
            .ok_or("cluster policy cell disappeared")?
            .infantry;
        let fixed = projection
            .unaffected_strength_by_cell
            .get(&coordinate)
            .copied()
            .unwrap_or(0);
        let target = fixed
            .checked_add(affected_target)
            .ok_or("cluster policy target overflow")?;
        desired.insert(coordinate, target);
        if target > current {
            let reserved = projection
                .unrelated_destination_reservations_by_cell
                .get(&coordinate)
                .copied()
                .unwrap_or(0);
            let demand = (target - current).saturating_sub(reserved);
            if demand > 0 {
                demands.insert(coordinate, demand);
                requested = requested
                    .checked_add(demand)
                    .ok_or("cluster policy demand overflow")?;
            }
        }
    }
    if requested == 0 {
        return Ok(BTreeMap::new());
    }

    let mut result = cluster
        .iter()
        .map(|&coordinate| {
            view.cell(coordinate)
                .map(|cell| (coordinate, cell.infantry))
                .ok_or("cluster policy cell disappeared")
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let mut remaining = requested;
    for &coordinate in cluster {
        if remaining == 0 {
            break;
        }
        let current = result[&coordinate];
        let target = desired[&coordinate];
        let moved = current.saturating_sub(target).min(remaining);
        if moved > 0 {
            result.insert(coordinate, current - moved);
            remaining -= moved;
        }
    }
    if remaining != 0 {
        return Err("cluster policy demand exceeds movable strength");
    }
    for (coordinate, demand) in demands {
        let current = result[&coordinate];
        result.insert(
            coordinate,
            current
                .checked_add(demand)
                .ok_or("cluster policy result overflow")?,
        );
    }
    result.retain(|coordinate, infantry| {
        view.cell(*coordinate)
            .is_some_and(|cell| cell.infantry != *infantry)
    });
    Ok(result)
}

pub fn apply_server_updates(mut updates: MessageReader<ServerUpdate>, mut view: ResMut<MatchView>) {
    let mut offline_cells_changed = false;
    for update in updates.read() {
        match update {
            ServerUpdate::SubmissionStarted { .. } => {}
            ServerUpdate::Accepted {
                summary,
                patches,
                flow,
                front,
                ..
            } => {
                offline_cells_changed |= !patches.is_empty();
                for patch in patches {
                    if let Some(cell) = view.cell_mut(patch.coordinate) {
                        cell.owner = patch.owner;
                        cell.infantry = patch.infantry.min(cell.military_capacity);
                    }
                }
                if let Some(flow) = flow {
                    view.active_flows.push(flow.clone());
                }
                if let Some(front) = front {
                    view.active_fronts.push(front.clone());
                }
                view.push_log(summary);
                view.show_toast("Command accepted", ToastKind::Success);
            }
            ServerUpdate::MobilizationChanged { command_id, target } => {
                view.mobilization_target = *target;
                let command = command_id.map_or_else(String::new, |id| format!(" · command #{id}"));
                view.push_log(format!(
                    "Mobilization target set to {:.0}%{command}",
                    target * 100.0
                ));
                view.show_toast("Future recruitment target updated", ToastKind::Success);
            }
            ServerUpdate::Rejected {
                reason,
                relevant_cell,
                ..
            } => {
                let marker = relevant_cell.map_or_else(String::new, |cell| {
                    format!(" · marked {},{}", cell.q, cell.r)
                });
                view.push_log(format!("Rejected: {reason}{marker}"));
                view.show_toast(reason, ToastKind::Rejection);
            }
        }
    }

    if offline_cells_changed && matches!(view.authority, crate::model::AuthorityState::Offline) {
        reconcile_offline_cluster_policies(&mut view);
        maintain_offline_cluster_policies(&mut view);
    }

    if view.authoritative_control.is_none() {
        let one = view.conquest_percent(1);
        let two = view.conquest_percent(2);
        let target = view.conquest_threshold_bps as f32 / 100.0;
        if one >= target {
            view.phase = MatchPhase::Victory(1);
        } else if two >= target {
            view.phase = MatchPhase::Victory(2);
        }
    }
}

#[cfg(test)]
fn resolve_push_front(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    direction: Axial,
    commitment_percent: u8,
) -> ServerUpdate {
    resolve_push_front_with_retask(
        view,
        sources,
        &BTreeSet::new(),
        direction,
        commitment_percent,
    )
}

fn resolve_push_front_with_retask(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    supersede_order_ids: &BTreeSet<u64>,
    direction: Axial,
    commitment_percent: u8,
) -> ServerUpdate {
    let projection = match view.project_cluster_action_selection(sources, supersede_order_ids) {
        Ok(projection) => projection,
        Err(error) => return projection_rejection(error),
    };
    resolve_projected_push_front(view, &projection, direction, commitment_percent)
}

pub(crate) fn resolve_projected_push_front(
    view: &MatchView,
    projection: &ProjectedOrderSelection,
    direction: Axial,
    commitment_percent: u8,
) -> ServerUpdate {
    let sources = &projection.cells;
    if let Some(invalid) = sources.iter().find(|coordinate| {
        view.cell(**coordinate).is_none_or(|cell| {
            !view.is_local_owned(**coordinate) || !cell.is_land() || cell.blocked
        })
    }) {
        return rejection("Push sources must be owned passable ground", Some(*invalid));
    }

    if direction == Axial::ZERO {
        return resolve_projected_arc_push(view, projection, commitment_percent);
    }

    let edges = match selected_front_edges(sources, direction, |source, target| {
        push_edge_is_eligible(view, source, target)
    }) {
        Ok(edges) => edges,
        Err(error) => return rejection(front_error_message(error), sources.first().copied()),
    };

    let front_sources = edges
        .iter()
        .map(|edge| edge.source)
        .collect::<BTreeSet<_>>();
    let assignments = selected_front_assignments(view, sources, &front_sources, direction);

    let percentage = u64::from(commitment_percent.clamp(10, 100));
    let requested_by_source = assignments
        .keys()
        .map(|coordinate| {
            let strength = projection
                .affected_strength_by_cell
                .get(coordinate)
                .copied()
                .unwrap_or(0);
            (*coordinate, strength.saturating_mul(percentage) / 100)
        })
        .collect::<BTreeMap<_, _>>();
    let requested = requested_by_source
        .values()
        .copied()
        .fold(0_u64, u64::saturating_add);
    if requested == 0 {
        return rejection(
            "Selected sources have no infantry to commit",
            sources.first().copied(),
        );
    }

    let target_by_boundary = edges
        .iter()
        .map(|edge| (edge.source, edge.target))
        .collect::<BTreeMap<_, _>>();
    let friendly_boundaries = target_by_boundary
        .iter()
        .filter_map(|(&boundary, &target)| view.is_local_owned(target).then_some(boundary))
        .collect::<BTreeSet<_>>();
    let mut changed = BTreeMap::<Axial, (Option<u32>, u64)>::new();
    let mut committed_by_boundary = BTreeMap::<Axial, u64>::new();
    for (source, source_request) in &requested_by_source {
        let Some(boundary) = assignments.get(source) else {
            continue;
        };
        if friendly_boundaries.contains(boundary) {
            continue;
        }
        let committed = committed_by_boundary.entry(*boundary).or_default();
        *committed = committed.saturating_add(*source_request);
        let cell = view.cell(*source).expect("push source was validated");
        let unaffected = projection
            .unaffected_strength_by_cell
            .get(source)
            .copied()
            .unwrap_or(0);
        let affected = projection
            .affected_strength_by_cell
            .get(source)
            .copied()
            .unwrap_or(0);
        changed.insert(
            *source,
            (
                cell.owner,
                unaffected.saturating_add(affected.saturating_sub(*source_request)),
            ),
        );
    }
    let committed = requested_by_source.values().copied().sum();
    if committed == 0 {
        return rejection(
            "The selected front has no infantry to commit",
            target_by_boundary.values().next().copied(),
        );
    }

    let mut captured = 0_u32;
    let mut defender_losses = 0_u64;
    let mut repositioned = 0_u64;
    let mut representative_route = Vec::new();
    let mut representative_attacking = true;

    let mut retreat_map = HexMap::new();
    let mut retreat_intents = Vec::new();
    let mut retreat_trailing_sources = BTreeSet::new();
    for (&source, &amount) in &requested_by_source {
        let Some(boundary) = assignments.get(&source) else {
            continue;
        };
        if !friendly_boundaries.contains(boundary) || amount == 0 {
            continue;
        }
        let destination = source + direction;
        for coordinate in [source, destination] {
            if retreat_map.get(coordinate).is_none() {
                let cell = view
                    .cell(coordinate)
                    .expect("validated friendly retreat cell exists");
                retreat_map.insert(projected_cell(cell, cell.infantry, 0));
            }
        }
        retreat_intents.push(MovementIntent {
            id: retreat_intents.len() as u64 + 1,
            priority: 0,
            owner: view.local_player,
            from: source,
            to: destination,
            requested: amount,
        });
        if !sources.contains(&(source - direction)) {
            retreat_trailing_sources.insert(source);
        }
    }
    if !retreat_intents.is_empty() {
        let movement = MovementConfig {
            max_elevation_step: view.max_elevation_step,
            ..MovementConfig::default()
        };
        let logistics = LogisticsConfig {
            default_military_capacity: u64::MAX,
            default_edge_throughput: u64::MAX,
            default_combat_frontage: u64::MAX,
        };
        let Ok(result) = movement_step(&mut retreat_map, &retreat_intents, &movement, &logistics)
        else {
            return rejection(
                "The selected retreat cannot be resolved safely",
                sources.first().copied(),
            );
        };
        repositioned = result
            .outcomes
            .values()
            .map(|outcome| outcome.approved)
            .sum();
        for cell in retreat_map.cells() {
            let owner = if retreat_trailing_sources.contains(&cell.coordinate)
                && cell.force() == 0
                && view.is_capturable(cell.coordinate)
                && !projection
                    .unrelated_destination_reservations_by_cell
                    .contains_key(&cell.coordinate)
                && !projection
                    .unrelated_destination_claims
                    .contains(&cell.coordinate)
            {
                None
            } else {
                Some(view.local_player)
            };
            changed.insert(cell.coordinate, (owner, cell.force()));
        }
        let intent = retreat_intents[0];
        representative_route = vec![intent.from, intent.to];
        representative_attacking = false;
    }

    for (boundary, mut mobile) in committed_by_boundary {
        let mut current = boundary;
        let mut occupied = assignments
            .iter()
            .filter_map(|(source, assigned)| (*assigned == boundary).then_some(*source))
            .filter(|source| *source != boundary)
            .collect::<Vec<_>>();
        occupied.push(boundary);
        let mut lane_route = vec![boundary];
        while mobile > 0 {
            let next = current + direction;
            let Some(destination) = view.cell(next) else {
                break;
            };
            let Some(from) = view.cell(current) else {
                break;
            };
            if !destination.is_land()
                || destination.blocked
                || !view.is_capturable(next)
                || destination.owner == Some(view.local_player)
                || (i32::from(from.elevation) - i32::from(destination.elevation)).unsigned_abs()
                    > u32::from(view.max_elevation_step)
            {
                break;
            }

            lane_route.push(next);
            let (defender_owner, defenders) = changed
                .get(&next)
                .copied()
                .unwrap_or((destination.owner, destination.infantry));
            let exchanged = defenders.min(mobile);
            defender_losses = defender_losses.saturating_add(exchanged);
            mobile = mobile.saturating_sub(exchanged);
            if mobile == 0 {
                changed.insert(next, (defender_owner, defenders - exchanged));
                break;
            }

            captured = captured.saturating_add(1);
            let garrison = occupation_garrison(destination).min(mobile);
            mobile -= garrison;
            changed.insert(next, (Some(view.local_player), garrison));
            current = next;
            occupied.push(next);
        }

        if mobile > 0 {
            station_offline_strength(view, &mut changed, &occupied, mobile);
        }
        if lane_route.len() > representative_route.len() {
            representative_route = lane_route;
            representative_attacking = true;
        }
    }

    let first_edge = edges[0];
    let combat_edge = edges
        .iter()
        .copied()
        .find(|edge| !view.is_local_owned(edge.target));
    let summary = format!(
        "Push Front accepted · {committed} committed · {repositioned} repositioned · {captured} cells captured · {defender_losses} defender losses"
    );
    ServerUpdate::Accepted {
        command_id: None,
        summary,
        patches: changed
            .into_iter()
            .map(|(coordinate, (owner, infantry))| CellPatch {
                coordinate,
                owner,
                infantry,
            })
            .collect(),
        flow: Some(ActiveFlow {
            route: if representative_route.len() >= 2 {
                representative_route
            } else {
                vec![first_edge.source, first_edge.target]
            },
            strength: committed,
            attacking: representative_attacking,
            age: 0.0,
            lifetime: 10.0,
        }),
        front: combat_edge.map(|edge| ActiveFront {
            friendly: edge.source,
            hostile: edge.target,
            intensity: (committed as f32 / 100.0).clamp(0.25, 1.0),
            age: 0.0,
        }),
    }
}

const ARC_PUSH_DIRECTION_PROMPT: &str = "No hostile local contact · drag P to choose a direction";

/// Builds one deterministic local-contact route for every selected source that
/// can reach a hostile boundary. The zero-direction Push sentinel never treats
/// neutral or friendly perimeter cells as contacts.
pub(crate) fn arc_push_routes(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
) -> Result<BTreeMap<Axial, LocalFrontRoute>, &'static str> {
    let edges = selected_all_front_edges(sources, |source, target| {
        arc_push_edge_is_eligible(view, source, target)
    })
    .map_err(|_| ARC_PUSH_DIRECTION_PROMPT)?;
    let movement = MovementConfig {
        max_elevation_step: view.max_elevation_step,
        ..MovementConfig::default()
    };
    let routes = selected_local_front_routes(sources, edges, |from, to| {
        let (Some(from), Some(to)) = (view.cell(from), view.cell(to)) else {
            return None;
        };
        if from.blocked || to.blocked {
            return None;
        }
        ground_traversal(
            &projected_cell(from, from.infantry, 0),
            &projected_cell(to, to.infantry, 0),
            &movement,
        )
        .map(|traversal| u64::from(traversal.cost))
    });
    if routes.is_empty() {
        Err(ARC_PUSH_DIRECTION_PROMPT)
    } else {
        Ok(routes)
    }
}

fn arc_push_edge_is_eligible(view: &MatchView, source: Axial, target: Axial) -> bool {
    let (Some(source), Some(target)) = (view.cell(source), view.cell(target)) else {
        return false;
    };
    let movement = MovementConfig {
        max_elevation_step: view.max_elevation_step,
        ..MovementConfig::default()
    };
    source.owner == Some(view.local_player)
        && source.is_land()
        && !source.blocked
        && target.is_land()
        && !target.blocked
        && target.owner.is_some_and(|owner| owner != view.local_player)
        && view.is_capturable(target.coordinate)
        && ground_traversal(
            &projected_cell(source, source.infantry, 0),
            &projected_cell(target, target.infantry, 0),
            &movement,
        )
        .is_some()
}

fn resolve_projected_arc_push(
    view: &MatchView,
    projection: &ProjectedOrderSelection,
    commitment_percent: u8,
) -> ServerUpdate {
    let sources = &projection.cells;
    let routes = match arc_push_routes(view, sources) {
        Ok(routes) => routes,
        Err(reason) => return rejection(reason, sources.first().copied()),
    };
    let percentage = u64::from(commitment_percent.clamp(10, 100));
    let requested_by_source = routes
        .keys()
        .map(|coordinate| {
            let affected = projection
                .affected_strength_by_cell
                .get(coordinate)
                .copied()
                .unwrap_or(0);
            (*coordinate, affected.saturating_mul(percentage) / 100)
        })
        .collect::<BTreeMap<_, _>>();
    let committed = requested_by_source
        .values()
        .copied()
        .fold(0_u64, u64::saturating_add);
    if committed == 0 {
        return rejection(
            "Selected hostile-contact sources have no infantry to commit",
            sources.first().copied(),
        );
    }

    let mut changed = BTreeMap::<Axial, (Option<u32>, u64)>::new();
    let mut lanes = BTreeMap::<(Axial, Axial), (u64, Vec<Axial>)>::new();
    for (&source, route) in &routes {
        let amount = requested_by_source[&source];
        let cell = view.cell(source).expect("arc Push source was validated");
        let unaffected = projection
            .unaffected_strength_by_cell
            .get(&source)
            .copied()
            .unwrap_or(0);
        let affected = projection
            .affected_strength_by_cell
            .get(&source)
            .copied()
            .unwrap_or(0);
        changed.insert(
            source,
            (
                cell.owner,
                unaffected.saturating_add(affected.saturating_sub(amount)),
            ),
        );
        if amount > 0 {
            let lane = lanes
                .entry((route.edge.source, route.edge.target))
                .or_default();
            lane.0 = lane.0.saturating_add(amount);
            lane.1.push(source);
        }
    }

    let mut captured = 0_u32;
    let mut defender_losses = 0_u64;
    let mut representative_route = Vec::new();
    let mut active_edges = Vec::new();
    for ((boundary, first_target), (mut mobile, mut origins)) in lanes {
        let direction = first_target - boundary;
        active_edges.push(DirectedFrontEdge {
            source: boundary,
            target: first_target,
        });
        origins.sort_unstable();
        origins.dedup();
        let mut occupied = origins;
        let mut current = boundary;
        let mut lane_route = vec![boundary];
        while mobile > 0 {
            let next = current + direction;
            let Some(destination) = view.cell(next) else {
                break;
            };
            let Some(from) = view.cell(current) else {
                break;
            };
            let (defender_owner, defenders) = changed
                .get(&next)
                .copied()
                .unwrap_or((destination.owner, destination.infantry));
            if defender_owner == Some(view.local_player) {
                // Another local-normal lane captured this shared contact first.
                // Reinforce that cell as capacity permits, then stop this ray;
                // one capture must not turn every converging normal into a
                // sustained line through newly friendly ground.
                lane_route.push(next);
                let reinforced =
                    mobile.min(destination.military_capacity.saturating_sub(defenders));
                if reinforced > 0 {
                    changed.insert(
                        next,
                        (
                            Some(view.local_player),
                            defenders.saturating_add(reinforced),
                        ),
                    );
                    mobile -= reinforced;
                    occupied.push(next);
                }
                break;
            }
            if !destination.is_land()
                || destination.blocked
                || !view.is_capturable(next)
                || (i32::from(from.elevation) - i32::from(destination.elevation)).unsigned_abs()
                    > u32::from(view.max_elevation_step)
            {
                break;
            }

            lane_route.push(next);
            let exchanged = defenders.min(mobile);
            defender_losses = defender_losses.saturating_add(exchanged);
            mobile = mobile.saturating_sub(exchanged);
            if mobile == 0 {
                changed.insert(next, (defender_owner, defenders - exchanged));
                break;
            }

            captured = captured.saturating_add(1);
            let garrison = occupation_garrison(destination).min(mobile);
            mobile -= garrison;
            changed.insert(next, (Some(view.local_player), garrison));
            current = next;
            occupied.push(next);
        }

        if mobile > 0 {
            station_offline_strength(view, &mut changed, &occupied, mobile);
        }
        if lane_route.len() > representative_route.len() {
            representative_route = lane_route;
        }
    }

    let first_edge = active_edges[0];
    ServerUpdate::Accepted {
        command_id: None,
        summary: format!(
            "Arc Push accepted · {committed} committed · {captured} cells captured · {defender_losses} defender losses"
        ),
        patches: changed
            .into_iter()
            .map(|(coordinate, (owner, infantry))| CellPatch {
                coordinate,
                owner,
                infantry,
            })
            .collect(),
        flow: Some(ActiveFlow {
            route: if representative_route.len() >= 2 {
                representative_route
            } else {
                vec![first_edge.source, first_edge.target]
            },
            strength: committed,
            attacking: true,
            age: 0.0,
            lifetime: 10.0,
        }),
        front: Some(ActiveFront {
            friendly: first_edge.source,
            hostile: first_edge.target,
            intensity: (committed as f32 / 100.0).clamp(0.25, 1.0),
            age: 0.0,
        }),
    }
}

/// Push can cross a selected boundary into any passable adjacent cell. Friendly
/// ground is a one-cell reposition endpoint and therefore does not need to be
/// capturable; neutral or hostile ground retains the conquest restriction.
pub(crate) fn push_edge_is_eligible(view: &MatchView, source: Axial, target: Axial) -> bool {
    let (Some(source), Some(target)) = (view.cell(source), view.cell(target)) else {
        return false;
    };
    source.is_land()
        && !source.blocked
        && target.is_land()
        && !target.blocked
        && (target.owner == Some(view.local_player) || view.is_capturable(target.coordinate))
        && (i32::from(source.elevation) - i32::from(target.elevation)).unsigned_abs()
            <= u32::from(view.max_elevation_step)
}

/// Every passable selected-to-neutral edge around a selected region.
///
/// A target is deliberately neutral-only: Expand Perimeter grows into unclaimed
/// territory and never turns into an implicit attack when it reaches another
/// player. The returned ordering is stable so both commitment splitting and
/// previews remain deterministic.
pub(crate) fn expand_all_front_edges(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
) -> Result<Vec<DirectedFrontEdge>, FrontSelectionError> {
    selected_all_front_edges(sources, |source, target| {
        let Some(source_cell) = view.cell(source) else {
            return false;
        };
        view.cell(target).is_some_and(|target_cell| {
            target_cell.owner.is_none()
                && target_cell.is_land()
                && !target_cell.blocked
                && view.is_capturable(target)
                && (i32::from(source_cell.elevation) - i32::from(target_cell.elevation))
                    .unsigned_abs()
                    <= u32::from(view.max_elevation_step)
        })
    })
}

pub(crate) const MAX_WAVE_PREVIEW_RINGS: u16 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExpandWaveError {
    Front(FrontSelectionError),
}

#[derive(Clone, Debug, Default)]
struct ExpandWaveTopology {
    initial_edges: Vec<DirectedFrontEdge>,
    selected_depth: BTreeMap<Axial, u16>,
    outside_depth: BTreeMap<Axial, u16>,
    outgoing: BTreeMap<Axial, Vec<Axial>>,
    parents: BTreeMap<Axial, Vec<Axial>>,
    focus: Option<Axial>,
    truncated: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ExpandWaveForecast {
    pub initial_edges: Vec<DirectedFrontEdge>,
    pub reached_depth: BTreeMap<Axial, u16>,
    pub max_internal_depth: u16,
    pub strength_upper_bound: u64,
    pub first_ring_capacity: u64,
    pub active_sources: BTreeSet<Axial>,
    pub truncated: bool,
}

fn build_expand_wave_topology(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    focus: Option<Axial>,
    max_rings: Option<u16>,
) -> Result<ExpandWaveTopology, ExpandWaveError> {
    let initial_edges = expand_all_front_edges(view, sources).map_err(ExpandWaveError::Front)?;
    let boundary = initial_edges
        .iter()
        .map(|edge| edge.source)
        .collect::<BTreeSet<_>>();
    let selected_depth = selected_depths_to_boundary(view, sources, &boundary);

    let mut topology = ExpandWaveTopology {
        initial_edges: initial_edges.clone(),
        selected_depth,
        focus,
        ..Default::default()
    };

    // Inside the selected seed, strength moves down every shortest local
    // depth. A central pool can therefore branch, and equal-depth routes merge
    // naturally before reaching the outside perimeter.
    for (&source, &depth) in &topology.selected_depth {
        if depth == 0 {
            continue;
        }
        for neighbor in source.neighbors() {
            if topology.selected_depth.get(&neighbor) == Some(&(depth - 1))
                && wave_edge_is_traversable(view, source, neighbor)
            {
                topology.outgoing.entry(source).or_default().push(neighbor);
            }
        }
    }

    let mut first_ring = BTreeSet::new();
    for edge in initial_edges {
        topology
            .outgoing
            .entry(edge.source)
            .or_default()
            .push(edge.target);
        topology.outside_depth.insert(edge.target, 1);
        first_ring.insert(edge.target);
    }

    let ring_limit = max_rings.unwrap_or(u16::MAX);
    let mut depth = 1_u16;
    let mut frontier = first_ring;
    while !frontier.is_empty() {
        let candidates = next_wave_ring(view, sources, &topology.outside_depth, &frontier);
        if candidates.is_empty() {
            break;
        }
        if depth >= ring_limit {
            topology.truncated = true;
            break;
        }
        let next_depth = depth.saturating_add(1);
        let mut next_frontier = BTreeSet::new();
        for (target, parents) in candidates {
            topology.outside_depth.insert(target, next_depth);
            next_frontier.insert(target);
            for parent in parents {
                topology.outgoing.entry(parent).or_default().push(target);
            }
        }
        frontier = next_frontier;
        depth = next_depth;
    }

    for children in topology.outgoing.values_mut() {
        children.sort_unstable();
        children.dedup();
    }
    for (&parent, children) in &topology.outgoing {
        for &child in children {
            topology.parents.entry(child).or_default().push(parent);
        }
    }
    for parents in topology.parents.values_mut() {
        parents.sort_unstable();
        parents.dedup();
    }
    Ok(topology)
}

fn selected_depths_to_boundary(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    boundary: &BTreeSet<Axial>,
) -> BTreeMap<Axial, u16> {
    let mut depths = BTreeMap::new();
    let mut pending = VecDeque::new();
    for &coordinate in boundary {
        depths.insert(coordinate, 0_u16);
        pending.push_back(coordinate);
    }
    while let Some(current) = pending.pop_front() {
        let next_depth = depths[&current].saturating_add(1);
        for neighbor in current.neighbors() {
            if sources.contains(&neighbor)
                && !depths.contains_key(&neighbor)
                && wave_edge_is_traversable(view, current, neighbor)
            {
                depths.insert(neighbor, next_depth);
                pending.push_back(neighbor);
            }
        }
    }
    depths
}

fn next_wave_ring(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    visited: &BTreeMap<Axial, u16>,
    frontier: &BTreeSet<Axial>,
) -> BTreeMap<Axial, BTreeSet<Axial>> {
    let mut candidates = BTreeMap::<Axial, BTreeSet<Axial>>::new();
    for &parent in frontier {
        for target in parent.neighbors() {
            if sources.contains(&target)
                || visited.contains_key(&target)
                || !wave_continuation_target_is_eligible(view, parent, target)
            {
                continue;
            }
            candidates.entry(target).or_default().insert(parent);
        }
    }
    candidates
}

fn wave_edge_is_traversable(view: &MatchView, from: Axial, to: Axial) -> bool {
    view.cell(from)
        .zip(view.cell(to))
        .is_some_and(|(from, to)| {
            from.is_land()
                && to.is_land()
                && !from.blocked
                && !to.blocked
                && (i32::from(from.elevation) - i32::from(to.elevation)).unsigned_abs()
                    <= u32::from(view.max_elevation_step)
        })
}

fn wave_continuation_target_is_eligible(view: &MatchView, from: Axial, target: Axial) -> bool {
    wave_edge_is_traversable(view, from, target)
        && view.is_capturable(target)
        && view
            .cell(target)
            .is_some_and(|cell| cell.owner.is_none() || cell.owner == Some(view.local_player))
}

fn boundary_strength_pools(
    sources: &BTreeSet<Axial>,
    commitment_percent: u8,
    topology: &ExpandWaveTopology,
    source_strength_by_cell: &BTreeMap<Axial, u64>,
) -> (u64, BTreeMap<Axial, u64>, BTreeMap<Axial, u64>) {
    let percentage = u64::from(commitment_percent.clamp(10, 100));
    let requested_by_source = sources
        .iter()
        .filter(|coordinate| topology.selected_depth.contains_key(coordinate))
        .map(|coordinate| {
            let strength = source_strength_by_cell
                .get(coordinate)
                .copied()
                .unwrap_or(0);
            (*coordinate, strength.saturating_mul(percentage) / 100)
        })
        .collect::<BTreeMap<_, _>>();
    let requested = requested_by_source.values().copied().sum();
    let mut pools = requested_by_source.clone();
    let max_depth = topology.selected_depth.values().copied().max().unwrap_or(0);
    for depth in (1..=max_depth).rev() {
        let layer = topology
            .selected_depth
            .iter()
            .filter_map(|(&coordinate, &coordinate_depth)| {
                (coordinate_depth == depth).then_some(coordinate)
            })
            .collect::<Vec<_>>();
        for coordinate in layer {
            let amount = pools.remove(&coordinate).unwrap_or(0);
            distribute_wave_strength(
                amount,
                coordinate,
                topology
                    .outgoing
                    .get(&coordinate)
                    .map_or(&[][..], Vec::as_slice),
                topology.focus,
                &mut pools,
            );
        }
    }
    (requested, requested_by_source, pools)
}

fn distribute_wave_strength(
    total: u64,
    parent: Axial,
    targets: &[Axial],
    focus: Option<Axial>,
    incoming: &mut BTreeMap<Axial, u64>,
) {
    if total == 0 || targets.is_empty() {
        return;
    }
    let weights = targets
        .iter()
        .map(|&target| focus.map_or(1, |focus| focus_branch_weight(parent, target, focus)))
        .collect::<Vec<_>>();
    let quotas = weighted_branch_quotas_rotated(total, &weights, 0)
        .expect("positive wave branch weights conserve strength");
    for (&target, share) in targets.iter().zip(quotas.by_child) {
        let target_pool = incoming.entry(target).or_default();
        *target_pool = target_pool.saturating_add(share);
    }
}

pub(crate) fn forecast_expand_wave(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    source_strength_by_cell: &BTreeMap<Axial, u64>,
    commitment_percent: u8,
    max_rings: u16,
) -> Result<ExpandWaveForecast, ExpandWaveError> {
    forecast_expand_wave_toward(
        view,
        sources,
        source_strength_by_cell,
        commitment_percent,
        None,
        max_rings,
    )
}

pub(crate) fn forecast_expand_wave_toward(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    source_strength_by_cell: &BTreeMap<Axial, u64>,
    commitment_percent: u8,
    focus: Option<Axial>,
    max_rings: u16,
) -> Result<ExpandWaveForecast, ExpandWaveError> {
    let topology = build_expand_wave_topology(view, sources, focus, Some(max_rings))?;
    let (strength_upper_bound, _, boundary_pools) = boundary_strength_pools(
        sources,
        commitment_percent,
        &topology,
        source_strength_by_cell,
    );
    let mut incoming = BTreeMap::new();
    for (boundary, amount) in boundary_pools {
        distribute_wave_strength(
            amount,
            boundary,
            topology
                .outgoing
                .get(&boundary)
                .map_or(&[][..], Vec::as_slice),
            topology.focus,
            &mut incoming,
        );
    }

    let mut reached_cells = BTreeMap::new();
    let mut forecast_truncated = false;
    let max_depth = topology.outside_depth.values().copied().max().unwrap_or(0);
    for depth in 1..=max_depth {
        let current = std::mem::take(&mut incoming);
        for (coordinate, amount) in current {
            if amount == 0 || topology.outside_depth.get(&coordinate) != Some(&depth) {
                continue;
            }
            reached_cells.insert(coordinate, depth);
            let cell = view.cell(coordinate).expect("wave topology cell exists");
            let mobile = if cell.owner.is_none() {
                amount.saturating_sub(occupation_garrison(cell).min(amount))
            } else {
                amount
            };
            let children = topology
                .outgoing
                .get(&coordinate)
                .map_or(&[][..], Vec::as_slice);
            if children.is_empty() {
                forecast_truncated |= topology.truncated && depth == max_depth && mobile > 0;
            } else {
                distribute_wave_strength(
                    mobile,
                    coordinate,
                    children,
                    topology.focus,
                    &mut incoming,
                );
            }
        }
    }

    // Draw complete geometric bands through the furthest depth that any
    // forecast strength reaches. This keeps the preview a continuous
    // perimeter wave instead of turning low integer shares into radial dots.
    let reached_max_depth = reached_cells.values().copied().max().unwrap_or(0);
    let reached_depth = topology
        .outside_depth
        .iter()
        .filter_map(|(&coordinate, &depth)| {
            (depth <= reached_max_depth).then_some((coordinate, depth))
        })
        .collect();
    let first_ring_capacity = topology
        .outside_depth
        .iter()
        .filter_map(|(&coordinate, &depth)| (depth == 1).then_some(coordinate))
        .filter_map(|coordinate| view.cell(coordinate))
        .map(|cell| cell.military_capacity)
        .sum();
    Ok(ExpandWaveForecast {
        initial_edges: topology.initial_edges,
        reached_depth,
        max_internal_depth: topology.selected_depth.values().copied().max().unwrap_or(0),
        strength_upper_bound,
        first_ring_capacity,
        active_sources: topology.selected_depth.keys().copied().collect(),
        truncated: forecast_truncated,
    })
}

fn resolve_expand_clusters(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    focus: Axial,
    commitment_percent: u8,
) -> ServerUpdate {
    let Some(focus_cell) = view.cell(focus) else {
        return rejection("Expansion focus is outside the map", Some(focus));
    };
    if focus_cell.owner.is_some() || !view.is_capturable(focus) {
        return rejection(
            "Expansion focus must be unclaimed passable ground",
            Some(focus),
        );
    }
    let projection = match view.project_cluster_action_selection(sources, &BTreeSet::new()) {
        Ok(projection) => projection,
        Err(error) => return projection_rejection(error),
    };
    resolve_projected_expand_all(view, &projection, commitment_percent, Some(focus))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AttackWaveTopologyError {
    reason: &'static str,
    relevant_cell: Option<Axial>,
}

#[derive(Clone, Debug)]
pub(crate) struct AttackWaveForecast {
    pub initial_edges: Vec<DirectedFrontEdge>,
    pub reached_depth: BTreeMap<Axial, u16>,
    pub participating_sources: BTreeSet<Axial>,
}

fn attack_wave_error(
    reason: &'static str,
    relevant_cell: Option<Axial>,
) -> AttackWaveTopologyError {
    AttackWaveTopologyError {
        reason,
        relevant_cell,
    }
}

/// Validates that the supplied mask is a union of complete traversable enemy
/// clusters belonging to one player. A same-owner traversable neighbor may not
/// be omitted, which keeps the offline boundary honest to the cluster-only UI.
fn validate_attack_target_union(
    view: &MatchView,
    targets: &BTreeSet<Axial>,
) -> Result<u32, AttackWaveTopologyError> {
    if targets.is_empty() {
        return Err(attack_wave_error("Select at least one enemy cluster", None));
    }

    let mut enemy_owner = None;
    for &coordinate in targets {
        let Some(cell) = view.cell(coordinate) else {
            return Err(attack_wave_error(
                "Attack target is outside the map",
                Some(coordinate),
            ));
        };
        let Some(owner) = cell.owner else {
            return Err(attack_wave_error(
                "Attack targets must be enemy-owned clusters",
                Some(coordinate),
            ));
        };
        if owner == view.local_player {
            return Err(attack_wave_error(
                "Attack targets must be enemy-owned clusters",
                Some(coordinate),
            ));
        }
        if enemy_owner.is_some_and(|expected| expected != owner) {
            return Err(attack_wave_error(
                "One attack may target clusters belonging to only one enemy",
                Some(coordinate),
            ));
        }
        if !cell.is_land() || cell.blocked || !view.is_capturable(coordinate) {
            return Err(attack_wave_error(
                "Attack targets must be passable capturable ground",
                Some(coordinate),
            ));
        }
        enemy_owner = Some(owner);
    }

    let enemy_owner = enemy_owner.expect("a nonempty target union has an owner");
    for &coordinate in targets {
        for neighbor in coordinate.neighbors() {
            if targets.contains(&neighbor) {
                continue;
            }
            let omitted_from_same_cluster = view.cell(neighbor).is_some_and(|cell| {
                cell.owner == Some(enemy_owner)
                    && cell.is_land()
                    && !cell.blocked
                    && view.is_capturable(neighbor)
                    && wave_edge_is_traversable(view, coordinate, neighbor)
            });
            if omitted_from_same_cluster {
                return Err(attack_wave_error(
                    "Attack targets must contain complete enemy clusters",
                    Some(neighbor),
                ));
            }
        }
    }
    Ok(enemy_owner)
}

/// Builds one deterministic target-mask DAG from all currently shared fronts.
/// Source strength moves down its inward distance, then target strength moves
/// up its minimum distance from any shared edge. A target with several parents
/// merges their strength before combat, while no edge can leave `targets`.
fn build_attack_wave_topology(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    targets: &BTreeSet<Axial>,
) -> Result<ExpandWaveTopology, AttackWaveTopologyError> {
    validate_attack_target_union(view, targets)?;
    let initial_edges = selected_all_front_edges(sources, |source, target| {
        targets.contains(&target)
            && view.is_capturable(target)
            && wave_edge_is_traversable(view, source, target)
    })
    .map_err(|error| match error {
        FrontSelectionError::EmptySelection => {
            attack_wave_error("Attack source selection is empty", None)
        }
        FrontSelectionError::NoEligibleFront => attack_wave_error(
            "Selected source and enemy clusters share no passable front",
            targets.first().copied(),
        ),
        FrontSelectionError::InvalidDirection => {
            attack_wave_error("Attack cluster boundary is invalid", None)
        }
    })?;
    let boundary = initial_edges
        .iter()
        .map(|edge| edge.source)
        .collect::<BTreeSet<_>>();
    let selected_depth = selected_depths_to_boundary(view, sources, &boundary);
    let mut topology = ExpandWaveTopology {
        initial_edges: initial_edges.clone(),
        selected_depth,
        ..Default::default()
    };

    for (&source, &depth) in &topology.selected_depth {
        if depth == 0 {
            continue;
        }
        for neighbor in source.neighbors() {
            if topology.selected_depth.get(&neighbor) == Some(&(depth - 1))
                && wave_edge_is_traversable(view, source, neighbor)
            {
                topology.outgoing.entry(source).or_default().push(neighbor);
            }
        }
    }

    let mut frontier = BTreeSet::new();
    for edge in initial_edges {
        topology
            .outgoing
            .entry(edge.source)
            .or_default()
            .push(edge.target);
        topology.outside_depth.insert(edge.target, 1);
        frontier.insert(edge.target);
    }

    let mut depth = 1_u16;
    while !frontier.is_empty() {
        let mut candidates = BTreeMap::<Axial, BTreeSet<Axial>>::new();
        for &parent in &frontier {
            for target in parent.neighbors() {
                if !targets.contains(&target)
                    || topology.outside_depth.contains_key(&target)
                    || !wave_edge_is_traversable(view, parent, target)
                {
                    continue;
                }
                candidates.entry(target).or_default().insert(parent);
            }
        }
        if candidates.is_empty() {
            break;
        }
        let next_depth = depth
            .checked_add(1)
            .ok_or_else(|| attack_wave_error("Attack target depth overflow", None))?;
        frontier.clear();
        for (target, parents) in candidates {
            topology.outside_depth.insert(target, next_depth);
            frontier.insert(target);
            for parent in parents {
                topology.outgoing.entry(parent).or_default().push(target);
            }
        }
        depth = next_depth;
    }

    if topology.outside_depth.len() != targets.len() {
        let disconnected = targets
            .iter()
            .find(|target| !topology.outside_depth.contains_key(target))
            .copied();
        return Err(attack_wave_error(
            "Every targeted enemy cluster must share a passable front with the selection",
            disconnected,
        ));
    }

    for children in topology.outgoing.values_mut() {
        children.sort_unstable();
        children.dedup();
    }
    for (&parent, children) in &topology.outgoing {
        for &child in children {
            topology.parents.entry(child).or_default().push(parent);
        }
    }
    for parents in topology.parents.values_mut() {
        parents.sort_unstable();
        parents.dedup();
    }
    Ok(topology)
}

/// Runs the same complete-target and passable-front validation used by the
/// offline resolver, while exposing only the data needed by interaction
/// previews. In particular, success means every targeted enemy component is
/// reachable from at least one selected source component.
pub(crate) fn forecast_attack_wave(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    targets: &BTreeSet<Axial>,
) -> Result<AttackWaveForecast, &'static str> {
    let topology =
        build_attack_wave_topology(view, sources, targets).map_err(|error| error.reason)?;
    Ok(AttackWaveForecast {
        initial_edges: topology.initial_edges,
        reached_depth: topology.outside_depth,
        participating_sources: topology.selected_depth.into_keys().collect(),
    })
}

/// Immediate deterministic offline resolution of one cluster-targeted attack.
/// The online simulation uses the same source commitment and target-mask wave
/// semantics over time; offline resolves the complete DAG in one update.
pub(crate) fn resolve_attack_clusters(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    targets: &BTreeSet<Axial>,
    commitment_percent: u8,
) -> ServerUpdate {
    let projection = match view.project_cluster_action_selection(sources, &BTreeSet::new()) {
        Ok(projection) => projection,
        Err(error) => return projection_rejection(error),
    };
    if let Some(invalid) = projection.cells.iter().find(|coordinate| {
        view.cell(**coordinate).is_none_or(|cell| {
            !view.is_local_owned(**coordinate) || !cell.is_land() || cell.blocked
        })
    }) {
        return rejection(
            "Attack sources must be owned passable ground",
            Some(*invalid),
        );
    }

    let topology = match build_attack_wave_topology(view, &projection.cells, targets) {
        Ok(topology) => topology,
        Err(error) => return rejection(error.reason, error.relevant_cell),
    };
    let (committed, requested_by_source, boundary_pools) = boundary_strength_pools(
        &projection.cells,
        commitment_percent,
        &topology,
        &projection.affected_strength_by_cell,
    );
    if committed == 0 {
        return rejection(
            "Participating source clusters have no infantry to commit",
            projection.cells.first().copied(),
        );
    }

    let mut changed = BTreeMap::<Axial, (Option<u32>, u64)>::new();
    for (source, source_request) in &requested_by_source {
        let cell = view.cell(*source).expect("attack source was validated");
        let unaffected = projection
            .unaffected_strength_by_cell
            .get(source)
            .copied()
            .unwrap_or(0);
        let affected = projection
            .affected_strength_by_cell
            .get(source)
            .copied()
            .unwrap_or(0);
        changed.insert(
            *source,
            (
                cell.owner,
                unaffected.saturating_add(affected.saturating_sub(*source_request)),
            ),
        );
    }

    let mut incoming = BTreeMap::new();
    for (boundary, amount) in boundary_pools {
        distribute_wave_strength(
            amount,
            boundary,
            topology
                .outgoing
                .get(&boundary)
                .map_or(&[][..], Vec::as_slice),
            None,
            &mut incoming,
        );
    }

    let mut captured = 0_u32;
    let mut defender_losses = 0_u64;
    let mut terminal_strength = Vec::<(Axial, u64)>::new();
    let max_depth = topology.outside_depth.values().copied().max().unwrap_or(0);
    for depth in 1..=max_depth {
        let current = std::mem::take(&mut incoming);
        for (coordinate, amount) in current {
            if amount == 0 || topology.outside_depth.get(&coordinate) != Some(&depth) {
                continue;
            }
            let cell = view.cell(coordinate).expect("attack topology cell exists");
            let exchanged = cell.infantry.min(amount);
            defender_losses = defender_losses.saturating_add(exchanged);
            let mobile = amount - exchanged;
            if mobile == 0 {
                changed.insert(
                    coordinate,
                    (cell.owner, cell.infantry.saturating_sub(exchanged)),
                );
                continue;
            }

            captured = captured.saturating_add(1);
            let garrison = occupation_garrison(cell).min(mobile);
            changed.insert(coordinate, (Some(view.local_player), garrison));
            let mobile = mobile - garrison;
            if mobile == 0 {
                continue;
            }
            let children = topology
                .outgoing
                .get(&coordinate)
                .map_or(&[][..], Vec::as_slice);
            if children.is_empty() {
                terminal_strength.push((coordinate, mobile));
            } else {
                distribute_wave_strength(mobile, coordinate, children, None, &mut incoming);
            }
        }
    }

    settle_wave_strength(view, &topology, &mut changed, &terminal_strength);

    ServerUpdate::Accepted {
        command_id: None,
        summary: format!(
            "Cluster attack accepted · {committed} committed at {}% · {captured} cells captured · {defender_losses} defender losses",
            commitment_percent.clamp(10, 100)
        ),
        patches: changed
            .into_iter()
            .map(|(coordinate, (owner, infantry))| CellPatch {
                coordinate,
                owner,
                infantry,
            })
            .collect(),
        // A cluster engagement has several changing local fronts, never one
        // representative ray or edge.
        flow: None,
        front: None,
    }
}

#[cfg(test)]
fn resolve_expand_all(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    commitment_percent: u8,
) -> ServerUpdate {
    resolve_expand_all_with_retask(view, sources, &BTreeSet::new(), commitment_percent)
}

fn resolve_expand_all_with_retask(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    supersede_order_ids: &BTreeSet<u64>,
    commitment_percent: u8,
) -> ServerUpdate {
    let projection = match view.project_cluster_action_selection(sources, supersede_order_ids) {
        Ok(projection) => projection,
        Err(error) => return projection_rejection(error),
    };
    resolve_projected_expand_all(view, &projection, commitment_percent, None)
}

fn resolve_projected_expand_all(
    view: &MatchView,
    projection: &ProjectedOrderSelection,
    commitment_percent: u8,
    focus: Option<Axial>,
) -> ServerUpdate {
    let sources = &projection.cells;
    if sources.is_empty() {
        return rejection("Expand Perimeter selection is empty", None);
    }
    if let Some(invalid) = sources.iter().find(|coordinate| {
        view.cell(**coordinate).is_none_or(|cell| {
            !view.is_local_owned(**coordinate) || !cell.is_land() || cell.blocked
        })
    }) {
        return rejection(
            "Expand Perimeter sources must be owned passable ground",
            Some(*invalid),
        );
    }

    let topology = match build_expand_wave_topology(view, sources, focus, None) {
        Ok(topology) => topology,
        Err(ExpandWaveError::Front(error)) => {
            return rejection(expand_error_message(error), sources.first().copied());
        }
    };
    let (committed, requested_by_source, boundary_pools) = boundary_strength_pools(
        sources,
        commitment_percent,
        &topology,
        &projection.affected_strength_by_cell,
    );
    if committed == 0 {
        return rejection(
            "Selected sources have no infantry to dispatch",
            sources.first().copied(),
        );
    }

    let mut changed = BTreeMap::<Axial, (Option<u32>, u64)>::new();
    for (source, source_request) in &requested_by_source {
        let cell = view.cell(*source).expect("expand source was validated");
        let unaffected = projection
            .unaffected_strength_by_cell
            .get(source)
            .copied()
            .unwrap_or(0);
        let affected = projection
            .affected_strength_by_cell
            .get(source)
            .copied()
            .unwrap_or(0);
        changed.insert(
            *source,
            (
                cell.owner,
                unaffected.saturating_add(affected.saturating_sub(*source_request)),
            ),
        );
    }

    let mut incoming = BTreeMap::new();
    for (boundary, amount) in boundary_pools {
        distribute_wave_strength(
            amount,
            boundary,
            topology
                .outgoing
                .get(&boundary)
                .map_or(&[][..], Vec::as_slice),
            topology.focus,
            &mut incoming,
        );
    }

    let mut captured = 0_u32;
    let mut terminal_strength = Vec::<(Axial, u64)>::new();
    let max_depth = topology.outside_depth.values().copied().max().unwrap_or(0);
    for depth in 1..=max_depth {
        let current = std::mem::take(&mut incoming);
        for (coordinate, amount) in current {
            if amount == 0 || topology.outside_depth.get(&coordinate) != Some(&depth) {
                continue;
            }
            let cell = view.cell(coordinate).expect("wave topology cell exists");
            let mobile = if cell.owner.is_none() {
                captured = captured.saturating_add(1);
                let garrison = occupation_garrison(cell).min(amount);
                changed.insert(coordinate, (Some(view.local_player), garrison));
                amount - garrison
            } else {
                amount
            };
            if mobile == 0 {
                continue;
            }
            let children = topology
                .outgoing
                .get(&coordinate)
                .map_or(&[][..], Vec::as_slice);
            if children.is_empty() {
                terminal_strength.push((coordinate, mobile));
            } else {
                distribute_wave_strength(
                    mobile,
                    coordinate,
                    children,
                    topology.focus,
                    &mut incoming,
                );
            }
        }
    }

    settle_wave_strength(view, &topology, &mut changed, &terminal_strength);

    ServerUpdate::Accepted {
        command_id: None,
        summary: format!(
            "Expand Perimeter accepted · {committed} dispatched at {}% · {captured} neutral cells captured",
            commitment_percent.clamp(10, 100)
        ),
        patches: changed
            .into_iter()
            .map(|(coordinate, (owner, infantry))| CellPatch {
                coordinate,
                owner,
                infantry,
            })
            .collect(),
        // A wave is a branching/merging region, not a representative ray.
        flow: None,
        front: None,
    }
}

fn settle_wave_strength(
    view: &MatchView,
    topology: &ExpandWaveTopology,
    changed: &mut BTreeMap<Axial, (Option<u32>, u64)>,
    terminals: &[(Axial, u64)],
) {
    // Preserve the spatial split first: surplus remains in the terminal cell
    // that carried it whenever that cell has capacity. Only true overflow is
    // pooled backward through the merged ancestry graph.
    let mut overflow = Vec::new();
    for &(coordinate, amount) in terminals {
        let Some(cell) = view.cell(coordinate) else {
            continue;
        };
        let (owner, current) = changed
            .get(&coordinate)
            .copied()
            .unwrap_or((cell.owner, cell.infantry));
        let stationed = if owner == Some(view.local_player) {
            amount.min(cell.military_capacity.saturating_sub(current))
        } else {
            0
        };
        if stationed > 0 {
            changed.insert(
                coordinate,
                (Some(view.local_player), current.saturating_add(stationed)),
            );
        }
        if amount > stationed {
            overflow.push((coordinate, amount - stationed));
        }
    }
    let mut strength: u64 = overflow.iter().map(|(_, strength)| *strength).sum();
    if strength == 0 {
        return;
    }
    let mut candidates = Vec::new();
    let mut visited = overflow
        .iter()
        .map(|(coordinate, _)| *coordinate)
        .collect::<BTreeSet<_>>();
    let mut pending = visited.iter().copied().collect::<VecDeque<_>>();
    while let Some(current) = pending.pop_front() {
        candidates.push(current);
        for &parent in topology
            .parents
            .get(&current)
            .map_or(&[][..], Vec::as_slice)
        {
            if visited.insert(parent) {
                pending.push_back(parent);
            }
        }
    }

    for coordinate in candidates {
        if strength == 0 {
            break;
        }
        let Some(cell) = view.cell(coordinate) else {
            continue;
        };
        let (owner, current) = changed
            .get(&coordinate)
            .copied()
            .unwrap_or((cell.owner, cell.infantry));
        if owner != Some(view.local_player) {
            continue;
        }
        let stationed = strength.min(cell.military_capacity.saturating_sub(current));
        if stationed > 0 {
            changed.insert(
                coordinate,
                (Some(view.local_player), current.saturating_add(stationed)),
            );
            strength -= stationed;
        }
    }
    debug_assert_eq!(strength, 0, "wave ancestry must retain committed capacity");
}

fn selected_front_assignments(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    front_sources: &BTreeSet<Axial>,
    direction: Axial,
) -> BTreeMap<Axial, Axial> {
    selected_directional_routes(sources, direction, front_sources, |from, to| {
        view.cell(from)
            .zip(view.cell(to))
            .is_some_and(|(from, to)| {
                from.is_land()
                    && to.is_land()
                    && !from.blocked
                    && !to.blocked
                    && (i32::from(from.elevation) - i32::from(to.elevation)).unsigned_abs()
                        <= u32::from(view.max_elevation_step)
            })
    })
    .into_iter()
    .filter_map(|(source, route)| route.last().copied().map(|boundary| (source, boundary)))
    .collect()
}

fn occupation_garrison(cell: &crate::model::CellView) -> u64 {
    if cell.military_capacity == 0 || !cell.is_land() {
        return 0;
    }
    let multiplier = match cell.terrain {
        hex_core::TerrainKind::Plains => 1,
        hex_core::TerrainKind::Hills => 2,
        hex_core::TerrainKind::Mountain => 3,
        hex_core::TerrainKind::Water => 0,
    };
    cell.military_capacity
        .div_ceil(20)
        .max(1)
        .saturating_mul(multiplier)
        .min(cell.military_capacity)
}

fn station_offline_strength(
    view: &MatchView,
    changed: &mut BTreeMap<Axial, (Option<u32>, u64)>,
    occupied: &[Axial],
    mut strength: u64,
) {
    for &coordinate in occupied.iter().rev() {
        if strength == 0 {
            break;
        }
        let Some(cell) = view.cell(coordinate) else {
            continue;
        };
        let (_, current) = changed
            .get(&coordinate)
            .copied()
            .unwrap_or((cell.owner, cell.infantry));
        let stationed = strength.min(cell.military_capacity.saturating_sub(current));
        changed.insert(
            coordinate,
            (Some(view.local_player), current.saturating_add(stationed)),
        );
        strength -= stationed;
    }
    debug_assert_eq!(
        strength, 0,
        "a push lane must retain capacity for survivors"
    );
}

fn front_error_message(error: FrontSelectionError) -> &'static str {
    match error {
        FrontSelectionError::EmptySelection => "Push selection is empty",
        FrontSelectionError::InvalidDirection => "Push direction is invalid",
        FrontSelectionError::NoEligibleFront => "No passable lane faces that direction",
    }
}

fn expand_error_message(error: FrontSelectionError) -> &'static str {
    match error {
        FrontSelectionError::EmptySelection => "Expand Perimeter selection is empty",
        FrontSelectionError::NoEligibleFront => "The selection has no passable neutral frontier",
        FrontSelectionError::InvalidDirection => "Expand Perimeter direction is invalid",
    }
}

#[cfg(test)]
fn resolve_redistribution(
    view: &MatchView,
    cells: &BTreeSet<Axial>,
    preset: RedistributionPreset,
    direction: Option<Axial>,
) -> ServerUpdate {
    resolve_redistribution_with_retask(view, cells, &BTreeSet::new(), preset, direction)
}

fn resolve_redistribution_with_retask(
    view: &MatchView,
    cells: &BTreeSet<Axial>,
    supersede_order_ids: &BTreeSet<u64>,
    preset: RedistributionPreset,
    direction: Option<Axial>,
) -> ServerUpdate {
    let projection = match view.project_order_selection(cells, supersede_order_ids) {
        Ok(projection) => projection,
        Err(error) => return projection_rejection(error),
    };
    resolve_projected_redistribution(view, &projection, preset, direction)
}

fn resolve_projected_redistribution(
    view: &MatchView,
    projection: &ProjectedOrderSelection,
    preset: RedistributionPreset,
    direction: Option<Axial>,
) -> ServerUpdate {
    let core_preset = match preset {
        RedistributionPreset::Balance => CoreDistributionPreset::Balance,
        RedistributionPreset::FrontLoad => {
            let Some(direction) = direction.filter(|direction| *direction != Axial::ZERO) else {
                return rejection(
                    "Directional Bias direction is too short",
                    projection.cells.first().copied(),
                );
            };
            CoreDistributionPreset::front_load(direction)
        }
        RedistributionPreset::CoreLoad => CoreDistributionPreset::CoreLoad,
        RedistributionPreset::PerimeterLoad => CoreDistributionPreset::PerimeterLoad,
    };

    let plan = match projected_redistribution_distribution(view, projection, core_preset) {
        Ok(plan) => plan,
        Err(reason) => return rejection(reason, projection.cells.first().copied()),
    };
    let patches = plan
        .final_strength_by_cell
        .into_iter()
        .map(|(coordinate, infantry)| CellPatch {
            coordinate,
            owner: Some(view.local_player),
            infantry,
        })
        .collect();

    let label = match preset {
        RedistributionPreset::Balance => "Formation · Balanced",
        RedistributionPreset::FrontLoad => "Directional Bias",
        RedistributionPreset::CoreLoad => "Formation · Center",
        RedistributionPreset::PerimeterLoad => "Formation · Perimeter",
    };
    ServerUpdate::Accepted {
        command_id: None,

        summary: format!(
            "{label} accepted · {} available infantry redistributed",
            plan.participating_strength,
        ),
        patches,
        flow: None,
        front: None,
    }
}
#[derive(Clone, Debug, Default)]
pub(crate) struct ProjectedDistribution {
    pub final_strength_by_cell: BTreeMap<Axial, u64>,
    pub excluded: BTreeSet<Axial>,
    pub participating_strength: u64,
    pub destination_capacity: u64,
    pub destination_strength: u64,
    pub outside_strength: u64,
}

pub(crate) fn projected_redistribution_distribution(
    view: &MatchView,
    projection: &ProjectedOrderSelection,
    preset: CoreDistributionPreset,
) -> Result<ProjectedDistribution, &'static str> {
    if projection.cells.is_empty() {
        return Err("Formation or Directional Bias needs selected troops");
    }
    validate_projected_cells(view, &projection.cells)?;
    if projection.cells.iter().any(|coordinate| {
        projection
            .unrelated_destination_reservations_by_cell
            .contains_key(coordinate)
    }) {
        return Err("A selected cell is reserved by another active order");
    }
    let mut plan = ProjectedDistribution::default();
    for component in owned_relevant_components(view, &projection.cells) {
        let mut map = HexMap::new();
        let mut total = 0_u64;
        for &coordinate in &component {
            let cell = view.cell(coordinate).expect("projected cell was validated");
            let affected = projection
                .affected_strength_by_cell
                .get(&coordinate)
                .copied()
                .unwrap_or(0);
            let unaffected = projection
                .unaffected_strength_by_cell
                .get(&coordinate)
                .copied()
                .unwrap_or(0);
            total = total.saturating_add(affected);
            map.insert(projected_cell(cell, affected, unaffected));
        }
        let distribution = redistribution_targets_with_commitment(
            &map,
            view.local_player,
            component.iter().copied(),
            total,
            preset,
            10_000,
        )
        .map_err(|_| "This Formation or Directional Bias cannot be resolved")?;
        let final_strength = distribution
            .targets
            .into_iter()
            .map(|(coordinate, target)| {
                let unaffected = projection
                    .unaffected_strength_by_cell
                    .get(&coordinate)
                    .copied()
                    .unwrap_or(0);
                (coordinate, unaffected.saturating_add(target))
            })
            .collect::<BTreeMap<_, _>>();
        if final_strength.iter().all(|(coordinate, target)| {
            view.cell(*coordinate)
                .is_some_and(|cell| cell.infantry == *target)
        }) {
            plan.excluded.extend(component);
            continue;
        }
        plan.participating_strength = plan.participating_strength.saturating_add(
            component
                .iter()
                .map(|coordinate| {
                    projection
                        .affected_strength_by_cell
                        .get(coordinate)
                        .copied()
                        .unwrap_or(0)
                })
                .sum::<u64>(),
        );
        plan.destination_capacity = plan.destination_capacity.saturating_add(
            component
                .iter()
                .filter_map(|coordinate| view.cell(*coordinate))
                .map(|cell| cell.military_capacity)
                .sum::<u64>(),
        );
        plan.final_strength_by_cell.extend(final_strength);
    }
    Ok(plan)
}

pub(crate) fn projected_shape_distribution(
    view: &MatchView,
    projection: &ProjectedOrderSelection,
    targets: &BTreeSet<Axial>,
) -> Result<ProjectedDistribution, &'static str> {
    if targets.is_empty() {
        return Err("Draw at least one destination hex");
    }
    let cells = projection
        .cells
        .union(targets)
        .copied()
        .collect::<BTreeSet<_>>();
    if cells.len() > 32_768 {
        return Err("Source and destination shape exceed the 32,768-cell command limit");
    }

    validate_projected_cells(view, &cells)?;
    if targets.iter().any(|coordinate| {
        projection
            .unrelated_destination_reservations_by_cell
            .contains_key(coordinate)
    }) {
        return Err(
            "Destination shape overlaps an inbound destination reserved by another active order",
        );
    }
    let mut plan = ProjectedDistribution::default();
    let mut has_changes = false;
    for component in owned_relevant_components(view, &cells) {
        let component_sources = projection
            .cells
            .intersection(&component)
            .copied()
            .collect::<BTreeSet<_>>();
        let component_targets = targets
            .intersection(&component)
            .copied()
            .collect::<BTreeSet<_>>();
        if component_sources.is_empty() {
            plan.excluded.extend(component);
            continue;
        }
        let component_strength = component_sources
            .iter()
            .map(|coordinate| {
                projection
                    .affected_strength_by_cell
                    .get(coordinate)
                    .copied()
                    .unwrap_or(0)
            })
            .sum::<u64>();
        plan.participating_strength = plan
            .participating_strength
            .saturating_add(component_strength);
        if component_targets.is_empty() {
            plan.outside_strength = plan.outside_strength.saturating_add(component_strength);
            plan.excluded.extend(component_sources);
            continue;
        }
        let mut map = HexMap::new();
        let mut weights = BTreeMap::new();
        let mut lower_bounds = BTreeMap::new();
        let mut fixed = BTreeMap::new();
        let mut total = 0_u64;
        for &coordinate in &component {
            let cell = view.cell(coordinate).expect("shape cell was validated");
            let affected = if component_sources.contains(&coordinate) {
                projection
                    .affected_strength_by_cell
                    .get(&coordinate)
                    .copied()
                    .unwrap_or(0)
            } else {
                0
            };
            let fixed_strength = if component_sources.contains(&coordinate) {
                projection
                    .unaffected_strength_by_cell
                    .get(&coordinate)
                    .copied()
                    .unwrap_or(0)
            } else {
                cell.infantry
            };
            total = total.saturating_add(affected);
            if component_targets.contains(&coordinate) {
                plan.destination_capacity = plan
                    .destination_capacity
                    .saturating_add(cell.military_capacity.saturating_sub(fixed_strength));
            }
            fixed.insert(coordinate, fixed_strength);
            weights.insert(
                coordinate,
                if component_targets.contains(&coordinate) {
                    BALANCE_WEIGHT
                } else {
                    0
                },
            );
            lower_bounds.insert(coordinate, 0);
            map.insert(projected_cell(cell, affected, fixed_strength));
        }
        let distribution = redistribution_targets_with_fallback_constraints(
            &map,
            view.local_player,
            weights,
            lower_bounds,
            total,
        )
        .map_err(|error| match error {
            DistributionError::InsufficientTargetCapacity { .. } => {
                "A local part of the source and drawn shape cannot conserve its selected troops"
            }
            _ => "A local part of the drawn shape cannot satisfy its shape constraints",
        })?;
        let component_destination_strength = distribution
            .targets
            .iter()
            .filter(|(coordinate, _)| component_targets.contains(coordinate))
            .map(|(_, strength)| *strength)
            .sum::<u64>();
        let component_outside_strength = distribution
            .targets
            .iter()
            .filter(|(coordinate, _)| !component_targets.contains(coordinate))
            .map(|(_, strength)| *strength)
            .sum::<u64>();
        plan.destination_strength = plan
            .destination_strength
            .saturating_add(component_destination_strength);
        plan.outside_strength = plan
            .outside_strength
            .saturating_add(component_outside_strength);
        let final_strength = distribution
            .targets
            .into_iter()
            .map(|(coordinate, target)| (coordinate, fixed[&coordinate].saturating_add(target)))
            .collect::<BTreeMap<_, _>>();
        if final_strength.iter().all(|(coordinate, target)| {
            view.cell(*coordinate)
                .is_some_and(|cell| cell.infantry == *target)
        }) {
            plan.excluded.extend(component);
            continue;
        }
        has_changes = true;
        plan.final_strength_by_cell.extend(final_strength);
    }
    if has_changes {
        Ok(plan)
    } else {
        Err("Destination shape does not move any selected troops")
    }
}

fn validate_projected_cells(view: &MatchView, cells: &BTreeSet<Axial>) -> Result<(), &'static str> {
    if cells
        .iter()
        .any(|coordinate| !view.is_local_owned_passable(*coordinate))
    {
        return Err("Every command cell must be owned passable ground");
    }
    Ok(())
}

fn projected_cell(cell: &crate::model::CellView, affected: u64, fixed: u64) -> Cell {
    Cell {
        coordinate: cell.coordinate,
        terrain: cell.terrain,
        elevation: cell.elevation,
        capturable: true,
        habitable: true,
        owner: cell.owner,
        civilian_population: cell.civilians,
        civilian_capacity: cell.civilians,
        forces: ForceComposition::infantry(affected),
        military_capacity: cell.military_capacity.saturating_sub(fixed),
    }
}

fn owned_relevant_components(view: &MatchView, relevant: &BTreeSet<Axial>) -> Vec<BTreeSet<Axial>> {
    let mut remaining = view
        .cells
        .keys()
        .filter(|coordinate| view.is_local_owned_passable(**coordinate))
        .copied()
        .collect::<BTreeSet<_>>();
    let mut components = Vec::new();
    while let Some(seed) = remaining.pop_first() {
        let mut pending = VecDeque::from([seed]);
        let mut component = BTreeSet::new();
        while let Some(current) = pending.pop_front() {
            if relevant.contains(&current) {
                component.insert(current);
            }
            let elevation = view.cell(current).expect("owned cell exists").elevation;
            for neighbor in current.neighbors() {
                let Some(neighbor_cell) = view.cell(neighbor) else {
                    continue;
                };
                let delta =
                    (i32::from(elevation) - i32::from(neighbor_cell.elevation)).unsigned_abs();
                if delta <= u32::from(view.max_elevation_step) && remaining.remove(&neighbor) {
                    pending.push_back(neighbor);
                }
            }
        }
        if !component.is_empty() {
            components.push(component);
        }
    }
    components
}

fn resolve_reshape_with_retask(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    targets: &BTreeSet<Axial>,
    supersede_order_ids: &BTreeSet<u64>,
) -> ServerUpdate {
    let projection = match view.project_cluster_action_selection(sources, supersede_order_ids) {
        Ok(projection) => projection,
        Err(error) => return projection_rejection(error),
    };
    let shape = match projected_shape_distribution(view, &projection, targets) {
        Ok(shape) => shape,
        Err(reason) => return rejection(reason, targets.first().copied()),
    };
    let patches = shape
        .final_strength_by_cell
        .into_iter()
        .map(|(coordinate, infantry)| CellPatch {
            coordinate,
            owner: Some(view.local_player),
            infantry,
        })
        .collect();
    ServerUpdate::Accepted {
        command_id: None,
        summary: if shape.outside_strength > 0 {
            format!(
                "Reshape accepted · {} infantry fit the destination · {} stay outside (best effort)",
                shape.destination_strength, shape.outside_strength,
            )
        } else {
            format!(
                "Reshape accepted · {} infantry fit the exact destination shape",
                shape.destination_strength,
            )
        },
        patches,
        flow: None,
        front: None,
    }
}

fn rejection(reason: impl Into<String>, relevant_cell: Option<Axial>) -> ServerUpdate {
    ServerUpdate::Rejected {
        command_id: None,
        reason: reason.into(),
        relevant_cell,
    }
}

fn projection_rejection(error: OrderSelectionProjectionError) -> ServerUpdate {
    match error {
        OrderSelectionProjectionError::InvalidSource(coordinate) => rejection(
            "Order source is no longer owned passable ground",
            Some(coordinate),
        ),
        OrderSelectionProjectionError::StaleOrder(order_id) => rejection(
            format!("Retask order #{order_id} is no longer active with local troops"),
            None,
        ),
        OrderSelectionProjectionError::UnknownPacketCell(coordinate) => rejection(
            "Retask order references a cell missing from the local map",
            Some(coordinate),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AuthorityState, CellView, RetaskProjection};
    use hex_core::TerrainKind;

    fn cell(coordinate: Axial, infantry: u64) -> CellView {
        CellView {
            coordinate,
            terrain: TerrainKind::Plains,
            elevation: 0,
            owner: Some(1),
            civilians: 0,
            infantry,
            military_capacity: 100,
            blocked: false,
        }
    }

    fn neutral_cell(coordinate: Axial) -> CellView {
        CellView {
            owner: None,
            infantry: 0,
            ..cell(coordinate, 0)
        }
    }

    fn hostile_cell(coordinate: Axial, infantry: u64) -> CellView {
        CellView {
            owner: Some(2),
            infantry,
            ..cell(coordinate, infantry)
        }
    }

    fn hex_disk(radius: i32) -> Vec<Axial> {
        (-radius..=radius)
            .flat_map(|q| (-radius..=radius).map(move |r| Axial::new(q, r)))
            .filter(|coordinate| coordinate.distance(Axial::ZERO) <= radius as u64)
            .collect()
    }

    #[test]
    fn arc_push_routes_keep_all_six_normals_around_a_hostile_pocket() {
        let pocket = Axial::ZERO;
        let sources = Axial::DIRECTIONS
            .into_iter()
            .map(|direction| pocket + direction)
            .collect::<BTreeSet<_>>();
        let mut view = MatchView::connecting(1);
        view.cells.insert(pocket, hostile_cell(pocket, 10));
        for &source in &sources {
            view.cells.insert(source, cell(source, 20));
        }
        let outer = sources
            .iter()
            .flat_map(|source| source.neighbors())
            .filter(|coordinate| *coordinate != pocket && !sources.contains(coordinate))
            .collect::<BTreeSet<_>>();
        assert!(!outer.is_empty());
        for (index, coordinate) in outer.iter().copied().enumerate() {
            let perimeter = if index % 2 == 0 {
                neutral_cell(coordinate)
            } else {
                cell(coordinate, 0)
            };
            view.cells.insert(coordinate, perimeter);
        }
        view.rebuild_chunk_index();

        let routes = arc_push_routes(&view, &sources).expect("the hostile pocket is contacted");

        assert_eq!(routes.len(), 6);
        assert_eq!(routes.keys().copied().collect::<BTreeSet<_>>(), sources);
        assert_eq!(
            routes
                .values()
                .map(|route| route.edge.target - route.edge.source)
                .collect::<BTreeSet<_>>(),
            Axial::DIRECTIONS.into_iter().collect()
        );
        assert!(routes.iter().all(|(&source, route)| {
            route.edge.source == source
                && route.edge.target == pocket
                && route.cells == vec![source, pocket]
        }));
    }

    #[test]
    fn arc_push_commits_one_share_when_one_source_touches_many_hostiles() {
        let source = Axial::ZERO;
        let sources = BTreeSet::from([source]);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 20));
        for target in source.neighbors() {
            view.cells.insert(target, hostile_cell(target, 0));
        }
        view.rebuild_chunk_index();

        let routes = arc_push_routes(&view, &sources).expect("all six contacts are eligible");
        assert_eq!(routes.len(), 1, "a source receives exactly one local route");

        let ServerUpdate::Accepted {
            summary,
            patches,
            flow,
            ..
        } = resolve_push_front(&view, &sources, Axial::ZERO, 50)
        else {
            panic!("zero-direction Push should use the hostile-contact arc");
        };
        assert!(summary.contains("10 committed"));
        assert_eq!(flow.expect("accepted Push has a flow").strength, 10);
        assert_eq!(
            patches
                .iter()
                .find(|patch| patch.coordinate == source)
                .map(|patch| patch.infantry),
            Some(10)
        );
        assert_eq!(
            patches
                .iter()
                .filter(|patch| patch.coordinate != source && patch.owner == Some(1))
                .count(),
            1,
            "the source share must not be repeated once per hostile edge"
        );
    }

    #[test]
    fn arc_push_routes_omit_cliff_isolated_and_disconnected_sources() {
        let cliff_isolated = Axial::new(0, 0);
        let interior = Axial::new(1, 0);
        let boundary = Axial::new(2, 0);
        let hostile = Axial::new(3, 0);
        let disconnected = Axial::new(10, 0);
        let sources = BTreeSet::from([cliff_isolated, interior, boundary, disconnected]);
        let mut view = MatchView::connecting(1);
        view.cells.insert(cliff_isolated, cell(cliff_isolated, 20));
        for coordinate in [interior, boundary] {
            let mut elevated = cell(coordinate, 20);
            elevated.elevation = 3;
            view.cells.insert(coordinate, elevated);
        }
        view.cells.insert(disconnected, cell(disconnected, 20));
        let mut target = hostile_cell(hostile, 0);
        target.elevation = 3;
        view.cells.insert(hostile, target);
        view.max_elevation_step = 1;
        view.rebuild_chunk_index();

        let routes = arc_push_routes(&view, &sources).expect("one component reaches contact");

        assert_eq!(
            routes.keys().copied().collect::<BTreeSet<_>>(),
            BTreeSet::from([interior, boundary])
        );
        assert_eq!(routes[&interior].cells, vec![interior, boundary, hostile]);
        assert_eq!(routes[&boundary].cells, vec![boundary, hostile]);
        assert!(!routes.contains_key(&cliff_isolated));
        assert!(!routes.contains_key(&disconnected));
    }

    #[test]
    fn zero_direction_arc_push_rejects_a_neutral_or_friendly_only_perimeter() {
        let source = Axial::ZERO;
        let sources = BTreeSet::from([source]);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 20));
        for (index, target) in source.neighbors().into_iter().enumerate() {
            let perimeter = if index % 2 == 0 {
                neutral_cell(target)
            } else {
                cell(target, 0)
            };
            view.cells.insert(target, perimeter);
        }
        view.rebuild_chunk_index();

        assert_eq!(
            arc_push_routes(&view, &sources).expect_err("there is no hostile contact"),
            ARC_PUSH_DIRECTION_PROMPT
        );
        assert!(matches!(
            resolve_push_front(&view, &sources, Axial::ZERO, 100),
            ServerUpdate::Rejected { reason, .. } if reason == ARC_PUSH_DIRECTION_PROMPT
        ));
    }

    #[test]
    fn offline_arc_push_advances_six_directions_and_conserves_strength() {
        let mut view = MatchView::connecting(1);
        let mut sources = BTreeSet::new();
        let mut targets = BTreeSet::new();
        for (q, direction) in [0, 5, 10, 15, 20, 25].into_iter().zip(Axial::DIRECTIONS) {
            let source = Axial::new(q, 0);
            let target = source + direction;
            sources.insert(source);
            targets.insert(target);
            view.cells.insert(source, cell(source, 20));
            view.cells.insert(target, hostile_cell(target, 0));
        }
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted { patches, flow, .. } =
            resolve_push_front(&view, &sources, Axial::ZERO, 50)
        else {
            panic!("each disconnected hostile contact should advance independently");
        };
        let infantry_after = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .map(|patch| patch.infantry)
                .expect("every participating cell has a patch")
        };

        assert!(sources.iter().all(|source| infantry_after(*source) == 10));
        assert!(targets.iter().all(|target| {
            infantry_after(*target) == 10
                && patches
                    .iter()
                    .any(|patch| patch.coordinate == *target && patch.owner == Some(1))
        }));
        assert_eq!(
            sources
                .iter()
                .chain(&targets)
                .map(|coordinate| infantry_after(*coordinate))
                .sum::<u64>(),
            120
        );
        assert_eq!(flow.expect("accepted Push has a flow").strength, 60);
    }

    #[test]
    fn offline_push_front_advances_every_edge_and_conserves_strength() {
        let sources = BTreeSet::from([Axial::new(0, -1), Axial::new(0, 0), Axial::new(0, 1)]);
        let targets = BTreeSet::from([Axial::new(1, -1), Axial::new(1, 0), Axial::new(1, 1)]);
        let mut view = MatchView::connecting(1);
        for source in &sources {
            let cell = cell(*source, 30);
            view.cells.insert(cell.coordinate, cell);
        }
        for target in &targets {
            let cell = neutral_cell(*target);
            view.cells.insert(cell.coordinate, cell);
        }
        view.rebuild_chunk_index();

        let update = resolve_push_front(&view, &sources, Axial::new(1, 0), 50);
        let ServerUpdate::Accepted { patches, flow, .. } = update else {
            panic!("connected front should be accepted");
        };
        let infantry_after = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .map(|patch| patch.infantry)
                .expect("every participating cell has a patch")
        };
        assert!(sources.iter().all(|source| infantry_after(*source) == 15));
        assert!(targets.iter().all(|target| infantry_after(*target) == 15));
        assert!(targets.iter().all(|target| {
            patches
                .iter()
                .find(|patch| patch.coordinate == *target)
                .is_some_and(|patch| patch.owner == Some(1))
        }));
        assert_eq!(
            sources
                .iter()
                .chain(&targets)
                .map(|coordinate| infantry_after(*coordinate))
                .sum::<u64>(),
            90
        );
        assert_eq!(flow.expect("push flow").strength, 45);
    }

    #[test]
    fn offline_inward_push_accepts_all_six_axes_and_stops_one_friendly_hex_outside_selection() {
        for direction in Axial::DIRECTIONS {
            let source = Axial::ZERO;
            let target = source + direction;
            let beyond = target + direction;
            let mut view = MatchView::connecting(1);
            view.cells.insert(source, cell(source, 40));
            view.cells.insert(target, cell(target, 10));
            view.cells.insert(beyond, neutral_cell(beyond));
            view.non_capturable_cells.insert(target);
            view.rebuild_chunk_index();

            let ServerUpdate::Accepted {
                patches,
                flow,
                front,
                ..
            } = resolve_push_front(&view, &BTreeSet::from([source]), direction, 50)
            else {
                panic!("friendly target should accept inward Push on {direction:?}");
            };
            let infantry = |coordinate| {
                patches
                    .iter()
                    .find(|patch| patch.coordinate == coordinate)
                    .map(|patch| patch.infantry)
            };
            assert_eq!(infantry(source), Some(20));
            assert_eq!(infantry(target), Some(30));
            assert_eq!(infantry(beyond), None, "inward Push must not sustain");
            assert!(front.is_none(), "friendly reposition is not a combat front");
            let flow = flow.expect("friendly reposition flow");
            assert_eq!(flow.route, vec![source, target]);
            assert!(!flow.attacking);
        }
    }

    #[test]
    fn offline_inward_push_moves_what_fits_and_releases_the_remainder() {
        let source = Axial::ZERO;
        let target = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 40));
        view.cells.insert(target, cell(target, 95));
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted {
            summary, patches, ..
        } = resolve_push_front(&view, &BTreeSet::from([source]), Axial::new(1, 0), 50)
        else {
            panic!("partially full friendly endpoint should resolve best effort");
        };
        let infantry = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .map(|patch| patch.infantry)
        };
        assert_eq!(infantry(source), Some(35));
        assert_eq!(infantry(target), Some(100));
        assert_eq!(patches.iter().map(|patch| patch.infantry).sum::<u64>(), 135);
        assert!(summary.contains("5 repositioned"));
    }

    #[test]
    fn offline_full_retreat_translates_a_column_and_relinquishes_its_trailing_cell_on_all_axes() {
        for direction in Axial::DIRECTIONS {
            let trailing = Axial::ZERO;
            let middle = trailing + direction;
            let boundary = middle + direction;
            let destination = boundary + direction;
            let sources = BTreeSet::from([trailing, middle, boundary]);
            let mut view = MatchView::connecting(1);
            for source in &sources {
                view.cells.insert(*source, cell(*source, 40));
            }
            view.cells.insert(destination, cell(destination, 0));
            view.rebuild_chunk_index();

            let ServerUpdate::Accepted { patches, .. } =
                resolve_push_front(&view, &sources, direction, 100)
            else {
                panic!("friendly column should retreat on {direction:?}");
            };
            let patch = |coordinate| {
                patches
                    .iter()
                    .find(|patch| patch.coordinate == coordinate)
                    .expect("retreat cell patch")
            };
            assert_eq!((patch(trailing).owner, patch(trailing).infantry), (None, 0));
            assert_eq!(patch(middle).infantry, 40);
            assert_eq!(patch(boundary).infantry, 40);
            assert_eq!(patch(destination).infantry, 40);
            assert_eq!(patches.iter().map(|patch| patch.infantry).sum::<u64>(), 120);
        }
    }

    #[test]
    fn offline_retreat_keeps_the_trailing_cell_when_capacity_blocks_the_pipeline() {
        let direction = Axial::new(1, 0);
        let trailing = Axial::ZERO;
        let middle = trailing + direction;
        let boundary = middle + direction;
        let destination = boundary + direction;
        let sources = BTreeSet::from([trailing, middle, boundary]);
        let mut view = MatchView::connecting(1);
        for coordinate in sources.iter().copied().chain([destination]) {
            view.cells.insert(coordinate, cell(coordinate, 100));
        }
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted {
            patches, summary, ..
        } = resolve_push_front(&view, &sources, direction, 100)
        else {
            panic!("blocked friendly column remains a valid best-effort retreat");
        };
        assert!(
            patches
                .iter()
                .all(|patch| patch.owner == Some(1) && patch.infantry == 100)
        );
        assert!(summary.contains("0 repositioned"));
    }

    #[test]
    fn offline_retreat_never_moves_unrelated_allocated_strength() {
        let source = Axial::ZERO;
        let target = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 40));
        view.cells.insert(target, cell(target, 0));
        view.set_retask_projection(RetaskProjection {
            active_order_ids: BTreeSet::from([9]),
            order_strength_by_cell: BTreeMap::from([(9, BTreeMap::from([(source, 20)]))]),
            active_strength_by_cell: BTreeMap::from([(source, 20)]),
            ..Default::default()
        });
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted { patches, .. } =
            resolve_push_front(&view, &BTreeSet::from([source]), Axial::new(1, 0), 100)
        else {
            panic!("unallocated part should still retreat");
        };
        let patch = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .expect("retreat patch")
        };
        assert_eq!((patch(source).owner, patch(source).infantry), (Some(1), 20));
        assert_eq!(patch(target).infantry, 20);
    }

    #[test]
    fn offline_retreat_preserves_an_empty_cell_reserved_by_an_inbound_order() {
        let source = Axial::ZERO;
        let target = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 40));
        view.cells.insert(target, cell(target, 0));
        view.set_retask_projection(RetaskProjection {
            active_order_ids: BTreeSet::from([9]),
            destination_reservations_by_order: BTreeMap::from([(
                9,
                BTreeMap::from([(source, 10)]),
            )]),
            ..Default::default()
        });
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted { patches, .. } =
            resolve_push_front(&view, &BTreeSet::from([source]), Axial::new(1, 0), 100)
        else {
            panic!("reserved source still has a valid movement forecast");
        };
        let source_patch = patches
            .iter()
            .find(|patch| patch.coordinate == source)
            .expect("source patch");
        assert_eq!((source_patch.owner, source_patch.infantry), (Some(1), 0));
    }

    #[test]
    fn offline_retreat_preserves_an_empty_cell_claimed_by_an_inbound_push() {
        let source = Axial::ZERO;
        let target = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 40));
        view.cells.insert(target, cell(target, 0));
        view.set_retask_projection(RetaskProjection {
            destination_claims_by_order: BTreeMap::from([(9, BTreeSet::from([source]))]),
            ..Default::default()
        });
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted { patches, .. } =
            resolve_push_front(&view, &BTreeSet::from([source]), Axial::new(1, 0), 100)
        else {
            panic!("claimed source still has a valid movement forecast");
        };
        let source_patch = patches
            .iter()
            .find(|patch| patch.coordinate == source)
            .expect("source patch");
        assert_eq!((source_patch.owner, source_patch.infantry), (Some(1), 0));
    }

    #[test]
    fn offline_push_front_leaves_a_blocked_middle_lane_in_place() {
        let direction = Axial::new(1, 0);
        let sources = BTreeSet::from([Axial::new(0, -1), Axial::ZERO, Axial::new(0, 1)]);
        let upper_target = Axial::new(1, -1);
        let blocked_gap = Axial::new(1, 0);
        let lower_target = Axial::new(1, 1);
        let mut view = MatchView::connecting(1);
        for &source in &sources {
            view.cells.insert(source, cell(source, 20));
        }
        view.cells.insert(upper_target, neutral_cell(upper_target));
        let mut blocked_target = cell(blocked_gap, 0);
        blocked_target.blocked = true;
        view.cells.insert(blocked_gap, blocked_target);
        view.cells.insert(lower_target, neutral_cell(lower_target));
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted { patches, flow, .. } =
            resolve_push_front(&view, &sources, direction, 50)
        else {
            panic!("the two straight eligible lanes should advance");
        };
        let patch = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .expect("participating cell should be patched")
        };

        assert_eq!(patch(Axial::new(0, -1)).infantry, 10);
        assert_eq!(patch(Axial::new(0, 1)).infantry, 10);
        assert!(patches.iter().all(|patch| patch.coordinate != Axial::ZERO));
        assert_eq!(patch(upper_target).owner, Some(1));
        assert_eq!(patch(lower_target).owner, Some(1));
        assert_eq!(patch(upper_target).infantry, 10);
        assert_eq!(patch(lower_target).infantry, 10);
        assert_eq!(patches.iter().map(|patch| patch.infantry).sum::<u64>(), 40);
        assert_eq!(flow.expect("representative offline flow").strength, 20);
    }

    #[test]
    fn one_offline_push_front_advances_through_successive_layers() {
        let source = Axial::ZERO;
        let direction = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 40));
        for distance in 1..=3 {
            let coordinate = Axial::new(distance, 0);
            view.cells.insert(coordinate, neutral_cell(coordinate));
        }
        view.rebuild_chunk_index();

        let update = resolve_push_front(&view, &BTreeSet::from([source]), direction, 50);
        let ServerUpdate::Accepted {
            summary,
            patches,
            flow,
            ..
        } = update
        else {
            panic!("a clear directional lane should accept a sustained push");
        };

        assert!(summary.contains("3 cells captured"));
        let infantry_after = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .map(|patch| patch.infantry)
                .expect("every occupied layer has a patch")
        };
        assert_eq!(infantry_after(source), 20);
        assert_eq!(infantry_after(Axial::new(1, 0)), 5);
        assert_eq!(infantry_after(Axial::new(2, 0)), 5);
        assert_eq!(infantry_after(Axial::new(3, 0)), 10);
        assert_eq!(patches.iter().map(|patch| patch.infantry).sum::<u64>(), 40);
        assert_eq!(
            flow.expect("sustained flow preview").route,
            vec![
                Axial::ZERO,
                Axial::new(1, 0),
                Axial::new(2, 0),
                Axial::new(3, 0),
            ]
        );
    }

    #[test]
    fn offline_cluster_attack_splits_one_share_across_every_shared_front() {
        let source = Axial::ZERO;
        let north_east = Axial::new(1, -1);
        let east = Axial::new(1, 0);
        let targets = BTreeSet::from([north_east, east]);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 60));
        for &target in &targets {
            view.cells.insert(target, hostile_cell(target, 0));
        }
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted {
            summary,
            patches,
            flow,
            front,
            ..
        } = resolve_attack_clusters(&view, &BTreeSet::from([source]), &targets, 50)
        else {
            panic!("both shared enemy fronts should receive the one committed share");
        };
        let infantry_after = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .map(|patch| patch.infantry)
                .expect("participating attack cell has a patch")
        };

        assert!(summary.contains("30 committed"));
        assert!(summary.contains("2 cells captured"));
        assert_eq!(infantry_after(source), 30);
        assert_eq!(infantry_after(north_east), 15);
        assert_eq!(infantry_after(east), 15);
        assert!(targets.iter().all(|target| {
            patches
                .iter()
                .any(|patch| patch.coordinate == *target && patch.owner == Some(1))
        }));
        assert_eq!(patches.iter().map(|patch| patch.infantry).sum::<u64>(), 60);
        assert!(flow.is_none());
        assert!(front.is_none());
    }

    #[test]
    fn repeated_ten_percent_contextual_actions_compound_on_the_remaining_available_pool() {
        let source = Axial::ZERO;
        let target = Axial::new(1, 0);
        let source_after = |update: ServerUpdate| {
            let ServerUpdate::Accepted { patches, .. } = update else {
                panic!("contextual action should be accepted");
            };
            patches
                .into_iter()
                .find(|patch| patch.coordinate == source)
                .map(|patch| patch.infantry)
                .expect("accepted action patches its participating source")
        };
        let view = |enemy: bool| {
            let mut view = MatchView::connecting(1);
            view.cells.insert(source, cell(source, 100));
            view.cells.insert(
                target,
                if enemy {
                    hostile_cell(target, 0)
                } else {
                    neutral_cell(target)
                },
            );
            view.rebuild_chunk_index();
            view
        };
        let first_expand = view(false);
        let first_attack = view(true);
        let first_expand_remaining = source_after(resolve_expand_clusters(
            &first_expand,
            &BTreeSet::from([source]),
            target,
            10,
        ));
        let first_attack_remaining = source_after(resolve_attack_clusters(
            &first_attack,
            &BTreeSet::from([source]),
            &BTreeSet::from([target]),
            10,
        ));
        assert_eq!(100 - first_expand_remaining, 10);
        assert_eq!(100 - first_attack_remaining, 10);

        let second = |enemy| {
            let mut view = view(enemy);
            view.set_retask_projection(RetaskProjection {
                active_order_ids: BTreeSet::from([41]),
                order_source_cells: BTreeMap::from([(41, BTreeSet::from([source]))]),
                order_strength_by_cell: BTreeMap::from([(41, BTreeMap::from([(source, 10)]))]),
                active_strength_by_cell: BTreeMap::from([(source, 10)]),
                ..Default::default()
            });
            view
        };
        let second_expand = second(false);
        let second_attack = second(true);
        let second_expand_remaining = source_after(resolve_expand_clusters(
            &second_expand,
            &BTreeSet::from([source]),
            target,
            10,
        ));
        let second_attack_remaining = source_after(resolve_attack_clusters(
            &second_attack,
            &BTreeSet::from([source]),
            &BTreeSet::from([target]),
            10,
        ));

        assert_eq!(100 - second_expand_remaining, 9);
        assert_eq!(100 - second_attack_remaining, 9);
        assert_eq!(10 + (100 - second_expand_remaining), 19);
        assert_eq!(10 + (100 - second_attack_remaining), 19);
    }

    #[test]
    fn offline_cluster_attack_merges_shared_target_parents_before_combat() {
        let left = Axial::ZERO;
        let right = Axial::new(1, 0);
        let target = Axial::new(0, 1);
        let sources = BTreeSet::from([left, right]);
        let mut view = MatchView::connecting(1);
        view.cells.insert(left, cell(left, 20));
        view.cells.insert(right, cell(right, 20));
        view.cells.insert(target, hostile_cell(target, 15));
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted {
            summary, patches, ..
        } = resolve_attack_clusters(&view, &sources, &BTreeSet::from([target]), 50)
        else {
            panic!("both boundary parents should merge before resolving target combat");
        };
        let patch = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .expect("participating cell has a patch")
        };

        assert!(summary.contains("20 committed"));
        assert!(summary.contains("15 defender losses"));
        assert!(summary.contains("1 cells captured"));
        assert_eq!(patch(left).infantry, 10);
        assert_eq!(patch(right).infantry, 10);
        assert_eq!((patch(target).owner, patch(target).infantry), (Some(1), 5));
    }

    #[test]
    fn offline_cluster_attack_commits_only_unallocated_participating_sources() {
        let source = Axial::ZERO;
        let remote = Axial::new(8, 0);
        let target = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 40));
        view.cells.insert(remote, cell(remote, 100));
        view.cells.insert(target, hostile_cell(target, 0));
        view.set_retask_projection(RetaskProjection {
            active_order_ids: BTreeSet::from([9]),
            order_strength_by_cell: BTreeMap::from([(9, BTreeMap::from([(source, 20)]))]),
            active_strength_by_cell: BTreeMap::from([(source, 20)]),
            ..Default::default()
        });
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted {
            summary, patches, ..
        } = resolve_attack_clusters(
            &view,
            &BTreeSet::from([source, remote]),
            &BTreeSet::from([target]),
            100,
        )
        else {
            panic!("the unallocated strength at the participating source should attack");
        };

        assert!(summary.contains("20 committed"));
        assert_eq!(
            patches
                .iter()
                .find(|patch| patch.coordinate == source)
                .map(|patch| patch.infantry),
            Some(20)
        );
        assert_eq!(
            patches
                .iter()
                .find(|patch| patch.coordinate == target)
                .map(|patch| (patch.owner, patch.infantry)),
            Some((Some(1), 20))
        );
        assert!(
            patches.iter().all(|patch| patch.coordinate != remote),
            "a selected cluster with no shared front must not commit its share"
        );
    }

    #[test]
    fn offline_cluster_attack_turns_through_its_target_mask_and_never_leaves_it() {
        let source = Axial::ZERO;
        let first = Axial::new(1, 0);
        let around_corner = Axial::new(1, 1);
        let end = Axial::new(0, 2);
        let outside_mask = Axial::new(2, 0);
        let targets = BTreeSet::from([first, around_corner, end]);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 100));
        for &target in &targets {
            view.cells.insert(target, hostile_cell(target, 0));
        }
        view.cells.insert(outside_mask, neutral_cell(outside_mask));
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted {
            summary, patches, ..
        } = resolve_attack_clusters(&view, &BTreeSet::from([source]), &targets, 100)
        else {
            panic!("a bent enemy cluster should accept a target-mask wave");
        };

        assert!(summary.contains("3 cells captured"));
        assert!(targets.iter().all(|target| {
            patches
                .iter()
                .any(|patch| patch.coordinate == *target && patch.owner == Some(1))
        }));
        assert!(patches.iter().all(|patch| patch.coordinate != outside_mask));
        assert_eq!(patches.iter().map(|patch| patch.infantry).sum::<u64>(), 100);
    }

    #[test]
    fn offline_cluster_attack_merges_combat_and_allows_other_fronts_to_advance() {
        let source = Axial::ZERO;
        let defended = Axial::new(1, -1);
        let open = Axial::new(1, 0);
        let targets = BTreeSet::from([defended, open]);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 60));
        view.cells.insert(defended, hostile_cell(defended, 40));
        view.cells.insert(open, hostile_cell(open, 0));
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted {
            summary, patches, ..
        } = resolve_attack_clusters(&view, &BTreeSet::from([source]), &targets, 100)
        else {
            panic!("one defended front must not invalidate the other shared front");
        };
        let patch = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .expect("engaged target has a patch")
        };

        assert!(summary.contains("1 cells captured"));
        assert!(summary.contains("30 defender losses"));
        assert_eq!(
            (patch(defended).owner, patch(defended).infantry),
            (Some(2), 10)
        );
        assert_eq!((patch(open).owner, patch(open).infantry), (Some(1), 30));
    }

    #[test]
    fn offline_cluster_attack_accepts_multiple_complete_contact_components() {
        let left_source = Axial::ZERO;
        let right_source = Axial::new(8, 0);
        let left_target = Axial::new(1, 0);
        let right_target = Axial::new(9, 0);
        let sources = BTreeSet::from([left_source, right_source]);
        let targets = BTreeSet::from([left_target, right_target]);
        let mut view = MatchView::connecting(1);
        for source in sources.iter().copied() {
            view.cells.insert(source, cell(source, 20));
        }
        for target in targets.iter().copied() {
            view.cells.insert(target, hostile_cell(target, 0));
        }
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted { patches, .. } =
            resolve_attack_clusters(&view, &sources, &targets, 50)
        else {
            panic!("each selected source and target component shares a front");
        };

        assert!(targets.iter().all(|target| {
            patches
                .iter()
                .any(|patch| patch.coordinate == *target && patch.owner == Some(1))
        }));
        assert_eq!(patches.iter().map(|patch| patch.infantry).sum::<u64>(), 40);
    }

    #[test]
    fn offline_cluster_attack_rejects_no_front_partial_and_mixed_owner_targets() {
        let source = Axial::ZERO;
        let mut no_front_view = MatchView::connecting(1);
        no_front_view.cells.insert(source, cell(source, 20));
        let distant = Axial::new(4, 0);
        no_front_view
            .cells
            .insert(distant, hostile_cell(distant, 0));
        no_front_view.rebuild_chunk_index();
        assert!(matches!(
            resolve_attack_clusters(
                &no_front_view,
                &BTreeSet::from([source]),
                &BTreeSet::from([distant]),
                50
            ),
            ServerUpdate::Rejected { reason, .. }
                if reason.contains("share no passable front")
        ));

        let first = Axial::new(1, 0);
        let omitted = Axial::new(2, 0);
        let mut partial_view = MatchView::connecting(1);
        partial_view.cells.insert(source, cell(source, 20));
        partial_view.cells.insert(first, hostile_cell(first, 0));
        partial_view.cells.insert(omitted, hostile_cell(omitted, 0));
        partial_view.rebuild_chunk_index();
        assert!(matches!(
            resolve_attack_clusters(
                &partial_view,
                &BTreeSet::from([source]),
                &BTreeSet::from([first]),
                50
            ),
            ServerUpdate::Rejected {
                reason,
                relevant_cell: Some(cell),
                ..
            } if reason.contains("complete enemy clusters") && cell == omitted
        ));

        let other_enemy = Axial::new(0, 1);
        let mut mixed_view = MatchView::connecting(1);
        mixed_view.cells.insert(source, cell(source, 20));
        mixed_view.cells.insert(first, hostile_cell(first, 0));
        let mut third_party = hostile_cell(other_enemy, 0);
        third_party.owner = Some(3);
        mixed_view.cells.insert(other_enemy, third_party);
        mixed_view.rebuild_chunk_index();
        assert!(matches!(
            resolve_attack_clusters(
                &mixed_view,
                &BTreeSet::from([source]),
                &BTreeSet::from([first, other_enemy]),
                50
            ),
            ServerUpdate::Rejected { reason, .. }
                if reason.contains("only one enemy")
        ));
    }

    #[test]
    fn offline_cluster_attack_rejects_a_cliff_blocked_shared_edge() {
        let source = Axial::ZERO;
        let target = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 20));
        let mut cliff = hostile_cell(target, 0);
        cliff.elevation = 3;
        view.cells.insert(target, cliff);
        view.max_elevation_step = 1;
        view.rebuild_chunk_index();

        assert!(matches!(
            resolve_attack_clusters(
                &view,
                &BTreeSet::from([source]),
                &BTreeSet::from([target]),
                50
            ),
            ServerUpdate::Rejected { reason, .. }
                if reason.contains("share no passable front")
        ));
    }

    #[test]
    fn offline_expand_all_splits_one_commitment_across_boundary_forks() {
        let source = Axial::ZERO;
        let north_east = Axial::new(1, -1);
        let east = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 60));
        view.cells.insert(north_east, neutral_cell(north_east));
        view.cells.insert(east, neutral_cell(east));
        view.rebuild_chunk_index();

        let update = resolve_expand_all(&view, &BTreeSet::from([source]), 50);
        let ServerUpdate::Accepted {
            summary, patches, ..
        } = update
        else {
            panic!("a two-edge neutral frontier should accept Expand Perimeter");
        };
        let infantry_after = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .map(|patch| patch.infantry)
                .expect("every participating cell has a patch")
        };

        assert!(summary.contains("30 dispatched"));
        assert_eq!(infantry_after(source), 30);
        assert_eq!(infantry_after(north_east), 15);
        assert_eq!(infantry_after(east), 15);
        assert_eq!(patches.iter().map(|patch| patch.infantry).sum::<u64>(), 60);
    }

    #[test]
    fn offline_expand_all_advances_through_successive_neutral_rings() {
        let source = Axial::ZERO;
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 40));
        for distance in 1..=3 {
            let coordinate = Axial::new(distance, 0);
            view.cells.insert(coordinate, neutral_cell(coordinate));
        }
        view.rebuild_chunk_index();

        let update = resolve_expand_all(&view, &BTreeSet::from([source]), 50);
        let ServerUpdate::Accepted {
            summary,
            patches,
            flow,
            ..
        } = update
        else {
            panic!("a clear neutral corridor should accept the perimeter wave");
        };
        let infantry_after = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .map(|patch| patch.infantry)
                .expect("every occupied layer has a patch")
        };

        assert!(summary.contains("3 neutral cells captured"));
        assert_eq!(infantry_after(source), 20);
        assert_eq!(infantry_after(Axial::new(1, 0)), 5);
        assert_eq!(infantry_after(Axial::new(2, 0)), 5);
        assert_eq!(infantry_after(Axial::new(3, 0)), 10);
        assert_eq!(patches.iter().map(|patch| patch.infantry).sum::<u64>(), 40);
        assert!(flow.is_none(), "a perimeter wave must not emit a fake ray");
    }

    #[test]
    fn offline_expand_all_transits_friendly_ground_without_regarrisoning_it() {
        let source = Axial::ZERO;
        let first_neutral = Axial::new(1, 0);
        let friendly_transit = Axial::new(2, 0);
        let far_neutral = Axial::new(3, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 40));
        view.cells
            .insert(first_neutral, neutral_cell(first_neutral));
        view.cells
            .insert(friendly_transit, cell(friendly_transit, 7));
        view.cells.insert(far_neutral, neutral_cell(far_neutral));
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted {
            summary,
            patches,
            flow,
            ..
        } = resolve_expand_all(&view, &BTreeSet::from([source]), 50)
        else {
            panic!("friendly ground should be transparent to a neutral expansion lane");
        };
        let infantry_after = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .map(|patch| patch.infantry)
        };

        assert!(summary.contains("2 neutral cells captured"));
        assert_eq!(infantry_after(source), Some(20));
        assert_eq!(infantry_after(first_neutral), Some(5));
        assert_eq!(infantry_after(friendly_transit), None);
        assert_eq!(infantry_after(far_neutral), Some(15));
        assert_eq!(patches.iter().map(|patch| patch.infantry).sum::<u64>(), 40);
        assert!(flow.is_none(), "friendly transit is still part of a wave");
    }

    #[test]
    fn expand_all_merges_multiple_boundary_parents_at_a_shared_target() {
        let left = Axial::ZERO;
        let right = Axial::new(1, 0);
        let shared_target = Axial::new(0, 1);
        let mut view = MatchView::connecting(1);
        view.cells.insert(left, cell(left, 20));
        view.cells.insert(right, cell(right, 20));
        view.cells
            .insert(shared_target, neutral_cell(shared_target));
        view.rebuild_chunk_index();
        let sources = BTreeSet::from([left, right]);

        let edges = expand_all_front_edges(&view, &sources).expect("connected neutral frontier");
        assert_eq!(edges.len(), 2);
        assert!(edges.iter().all(|edge| edge.target == shared_target));

        let ServerUpdate::Accepted { patches, .. } = resolve_expand_all(&view, &sources, 50) else {
            panic!("shared perimeter target should merge its incoming strength");
        };
        assert_eq!(patches.iter().map(|patch| patch.infantry).sum::<u64>(), 40);
        assert_eq!(
            patches
                .iter()
                .find(|patch| patch.coordinate == shared_target)
                .map(|patch| patch.infantry),
            Some(20)
        );
    }

    #[test]
    fn offline_expand_all_forms_complete_successive_offset_rings() {
        let source = Axial::ZERO;
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 100));
        for coordinate in hex_disk(2).into_iter().filter(|cell| *cell != source) {
            view.cells.insert(coordinate, neutral_cell(coordinate));
        }
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted {
            summary,
            patches,
            flow,
            ..
        } = resolve_expand_all(&view, &BTreeSet::from([source]), 100)
        else {
            panic!("a two-ring neutral disk should accept the perimeter wave");
        };

        assert!(summary.contains("18 neutral cells captured"));
        assert!(flow.is_none());
        assert_eq!(patches.iter().map(|patch| patch.infantry).sum::<u64>(), 100);
        assert_eq!(
            patches
                .iter()
                .find(|patch| patch.coordinate == source)
                .map(|patch| patch.infantry),
            Some(0)
        );
        assert!(
            hex_disk(2)
                .into_iter()
                .filter(|cell| *cell != source)
                .all(|coordinate| patches
                    .iter()
                    .find(|patch| patch.coordinate == coordinate)
                    .is_some_and(|patch| patch.owner == Some(1) && patch.infantry > 0))
        );
    }

    #[test]
    fn focused_cluster_expand_pulls_each_enclosing_front_toward_the_clicked_pocket() {
        let focus = Axial::ZERO;
        let sources = focus.neighbors().into_iter().collect::<BTreeSet<_>>();
        let outer_ring = hex_disk(2)
            .into_iter()
            .filter(|coordinate| coordinate.distance(focus) == 2)
            .collect::<BTreeSet<_>>();
        let mut view = MatchView::connecting(1);
        view.cells.insert(focus, neutral_cell(focus));
        for &source in &sources {
            view.cells.insert(source, cell(source, 20));
        }
        for &coordinate in &outer_ring {
            view.cells.insert(coordinate, neutral_cell(coordinate));
        }
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted { patches, .. } =
            resolve_expand_clusters(&view, &sources, focus, 100)
        else {
            panic!("a neutral pocket enclosed by the selected cluster should expand");
        };
        let patch = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .expect("every enclosing-wave cell should be patched")
        };

        assert_eq!(patch(focus).owner, Some(1));
        assert_eq!(patch(focus).infantry, 54);
        assert!(outer_ring.iter().all(|coordinate| {
            let outer = patch(*coordinate);
            outer.owner == Some(1) && outer.infantry > 0 && outer.infantry < patch(focus).infantry
        }));
        assert_eq!(patches.iter().map(|patch| patch.infantry).sum::<u64>(), 120);
    }

    #[test]
    fn central_strength_branches_through_the_selected_seed_and_merges_on_ring_one() {
        let selected = hex_disk(1).into_iter().collect::<BTreeSet<_>>();
        let mut view = MatchView::connecting(1);
        for &coordinate in &selected {
            let infantry = if coordinate == Axial::ZERO { 60 } else { 0 };
            view.cells.insert(coordinate, cell(coordinate, infantry));
        }
        for coordinate in hex_disk(2)
            .into_iter()
            .filter(|coordinate| !selected.contains(coordinate))
        {
            view.cells.insert(coordinate, neutral_cell(coordinate));
        }
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted {
            summary, patches, ..
        } = resolve_expand_all(&view, &selected, 100)
        else {
            panic!("central strength should reach every selected boundary branch");
        };

        assert!(summary.contains("12 neutral cells captured"));
        assert_eq!(patches.iter().map(|patch| patch.infantry).sum::<u64>(), 60);
        assert!(
            hex_disk(2)
                .into_iter()
                .filter(|coordinate| coordinate.distance(Axial::ZERO) == 2)
                .all(|coordinate| patches
                    .iter()
                    .find(|patch| patch.coordinate == coordinate)
                    .is_some_and(|patch| patch.owner == Some(1) && patch.infantry > 0))
        );
    }

    #[test]
    fn expand_all_advances_disconnected_source_regions_independently() {
        let left = Axial::ZERO;
        let right = Axial::new(3, 0);
        let targets = [left, right].map(|source| source + Axial::new(0, 1));
        let mut view = MatchView::connecting(1);
        for (source, target) in [left, right].into_iter().zip(targets) {
            view.cells.insert(source, cell(source, 20));
            view.cells.insert(target, neutral_cell(target));
        }
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted { patches, .. } =
            resolve_expand_all(&view, &BTreeSet::from([left, right]), 50)
        else {
            panic!("each disconnected region has its own eligible perimeter");
        };
        assert!(targets.into_iter().all(|target| {
            patches
                .iter()
                .any(|patch| patch.coordinate == target && patch.owner == Some(1))
        }));
    }

    #[test]
    fn offline_push_uses_authoritative_slope_and_capturability_constraints() {
        let source = Axial::ZERO;
        let target = Axial::new(1, 0);
        let sources = BTreeSet::from([source]);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 40));
        let mut elevated_target = neutral_cell(target);
        elevated_target.elevation = 2;
        view.cells.insert(target, elevated_target);
        view.max_elevation_step = 2;
        view.rebuild_chunk_index();

        assert!(matches!(
            resolve_push_front(&view, &sources, Axial::new(1, 0), 50),
            ServerUpdate::Accepted { .. }
        ));

        view.max_elevation_step = 1;
        assert!(matches!(
            resolve_push_front(&view, &sources, Axial::new(1, 0), 50),
            ServerUpdate::Rejected { .. }
        ));

        view.max_elevation_step = 2;
        view.non_capturable_cells.insert(target);
        assert!(matches!(
            resolve_push_front(&view, &sources, Axial::new(1, 0), 50),
            ServerUpdate::Rejected { .. }
        ));
    }

    #[test]
    fn offline_balance_uses_all_available_selected_troops() {
        let left = Axial::ZERO;
        let right = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(left, cell(left, 100));
        view.cells.insert(right, cell(right, 0));
        view.rebuild_chunk_index();

        let update = resolve_redistribution(
            &view,
            &BTreeSet::from([left, right]),
            RedistributionPreset::Balance,
            None,
        );
        let ServerUpdate::Accepted { patches, .. } = update else {
            panic!("full-selection balance should be accepted");
        };
        let target = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .map(|patch| patch.infantry)
        };
        assert_eq!(target(left), Some(50));
        assert_eq!(target(right), Some(50));
    }

    #[test]
    fn offline_cluster_policy_persists_for_the_complete_cluster_and_moves_only_free_troops() {
        let left = Axial::ZERO;
        let right = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(left, cell(left, 100));
        view.cells.insert(right, cell(right, 0));
        view.set_retask_projection(RetaskProjection {
            active_order_ids: BTreeSet::from([7]),
            order_strength_by_cell: BTreeMap::from([(7, BTreeMap::from([(left, 40)]))]),
            active_strength_by_cell: BTreeMap::from([(left, 40)]),
            ..Default::default()
        });
        view.rebuild_chunk_index();

        let selected = BTreeSet::from([left, right]);
        let ServerUpdate::Accepted {
            summary,
            patches,
            flow,
            front,
            ..
        } = resolve_cluster_policy(&view, &selected, ClusterPolicy::Balanced, None)
        else {
            panic!("a valid persistent policy should resolve offline");
        };
        assert!(summary.contains("free troops redistributed"));
        assert!(
            flow.is_none(),
            "offline policy movement stays background-only"
        );
        assert!(
            front.is_none(),
            "friendly policy movement cannot create combat UI"
        );
        assert_eq!(patches.iter().map(|patch| patch.infantry).sum::<u64>(), 100);
        assert!(
            patches
                .iter()
                .find(|patch| patch.coordinate == left)
                .is_some_and(|patch| patch.infantry >= 40),
            "the 40 active infantry must remain physically reserved at their cell"
        );

        // Sparse metadata seeds still close over the complete current cluster,
        // matching the online reducer's authority expansion.
        persist_offline_cluster_policy(
            &mut view,
            &BTreeSet::from([left]),
            ClusterPolicy::Perimeter,
            None,
        );
        for coordinate in [left, right] {
            assert_eq!(
                view.cluster_policy_at(coordinate)
                    .expect("owned cell policy")
                    .kind,
                ClusterPolicy::Perimeter
            );
        }
        let first_revision = view.cluster_policy_at(left).unwrap().revision;
        persist_offline_cluster_policy(
            &mut view,
            &BTreeSet::from([right]),
            ClusterPolicy::Center,
            None,
        );
        assert!(view.cluster_policy_at(left).unwrap().revision > first_revision);
        assert_eq!(
            view.cluster_policy_at(right).unwrap().kind,
            ClusterPolicy::Center
        );
    }

    #[test]
    fn offline_policy_lineage_follows_growth_and_uses_the_newest_policy_on_merge() {
        let left = Axial::ZERO;
        let middle = Axial::new(1, 0);
        let bridge = Axial::new(2, 0);
        let remote = Axial::new(3, 0);
        let mut view = MatchView::connecting(1);
        for coordinate in [left, middle, remote] {
            view.cells.insert(coordinate, cell(coordinate, 0));
        }
        view.rebuild_chunk_index();
        persist_offline_cluster_policy(
            &mut view,
            &BTreeSet::from([left]),
            ClusterPolicy::Perimeter,
            None,
        );
        persist_offline_cluster_policy(
            &mut view,
            &BTreeSet::from([remote]),
            ClusterPolicy::Directional,
            Some(Axial::new(800, -200)),
        );

        // Capturing the bridge merges the clusters. Offline lineage mirrors
        // authority: the newest explicit policy wins the complete component.
        view.cells.insert(bridge, cell(bridge, 0));
        view.rebuild_chunk_index();
        reconcile_offline_cluster_policies(&mut view);

        for coordinate in [left, middle, bridge, remote] {
            let policy = view.cluster_policy_at(coordinate).unwrap();
            assert_eq!(policy.kind, ClusterPolicy::Directional);
            assert_eq!(policy.orientation, Axial::new(800, -200));
        }
    }

    #[test]
    fn accepted_offline_growth_inherits_policy_and_immediately_maintains_the_new_cluster() {
        let left = Axial::ZERO;
        let middle = Axial::new(1, 0);
        let captured = Axial::new(2, 0);
        let mut view = MatchView::connecting(1);
        view.authority = AuthorityState::Offline;
        view.cells.insert(left, cell(left, 90));
        view.cells.insert(middle, cell(middle, 0));
        view.cells.insert(captured, neutral_cell(captured));
        view.rebuild_chunk_index();
        persist_offline_cluster_policy(
            &mut view,
            &BTreeSet::from([left]),
            ClusterPolicy::Balanced,
            None,
        );
        let inherited_revision = view.cluster_policy_at(left).unwrap().revision;

        let mut app = App::new();
        app.add_message::<ServerUpdate>()
            .insert_resource(view)
            .add_systems(Update, apply_server_updates);
        app.world_mut().write_message(ServerUpdate::Accepted {
            command_id: None,
            summary: "Expansion accepted".to_owned(),
            patches: vec![
                CellPatch {
                    coordinate: left,
                    owner: Some(1),
                    infantry: 60,
                },
                CellPatch {
                    coordinate: captured,
                    owner: Some(1),
                    infantry: 30,
                },
            ],
            flow: None,
            front: None,
        });
        app.update();

        let view = app.world().resource::<MatchView>();
        assert_eq!(view.latest_result, "Expansion accepted");
        assert_eq!(
            [left, middle, captured].map(|coordinate| view.cell(coordinate).unwrap().infantry),
            [30, 30, 30]
        );
        assert_eq!(
            view.cluster_policy_at(captured).unwrap(),
            ClusterPolicyView {
                kind: ClusterPolicy::Balanced,
                orientation: Axial::ZERO,
                revision: inherited_revision,
            }
        );
    }

    #[test]
    fn offline_policy_maintenance_freezes_actions_and_reserves_inbound_capacity() {
        let left = Axial::ZERO;
        let right = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.authority = AuthorityState::Offline;
        view.cells.insert(left, cell(left, 100));
        view.cells.insert(right, cell(right, 0));
        view.set_retask_projection(RetaskProjection {
            active_order_ids: BTreeSet::from([7]),
            order_strength_by_cell: BTreeMap::from([(7, BTreeMap::from([(left, 40)]))]),
            active_strength_by_cell: BTreeMap::from([(left, 40)]),
            destination_reservations_by_order: BTreeMap::from([(7, BTreeMap::from([(right, 20)]))]),
            ..Default::default()
        });
        view.rebuild_chunk_index();
        assert!(matches!(
            resolve_cluster_policy(
                &view,
                &BTreeSet::from([left]),
                ClusterPolicy::Balanced,
                None,
            ),
            ServerUpdate::Accepted { .. }
        ));
        persist_offline_cluster_policy(
            &mut view,
            &BTreeSet::from([left]),
            ClusterPolicy::Balanced,
            None,
        );

        let mut app = App::new();
        app.add_message::<ServerUpdate>()
            .insert_resource(view)
            .add_systems(Update, apply_server_updates);
        app.world_mut().write_message(ServerUpdate::Accepted {
            command_id: None,
            summary: "Action accepted".to_owned(),
            // Any accepted local action patch schedules policy maintenance;
            // the unchanged value keeps this test focused on allocation math.
            patches: vec![CellPatch {
                coordinate: left,
                owner: Some(1),
                infantry: 100,
            }],
            flow: None,
            front: None,
        });
        app.update();

        let view = app.world().resource::<MatchView>();
        assert_eq!(view.cell(left).unwrap().infantry, 83);
        assert_eq!(view.cell(right).unwrap().infantry, 17);
        assert_eq!(
            view.cell(left).unwrap().infantry + view.cell(right).unwrap().infantry,
            100
        );
        assert!(view.cell(left).unwrap().infantry >= 40);
        assert_eq!(
            view.cell(right).unwrap().infantry + 20,
            37,
            "the inbound action reservation completes, rather than competes with, the policy target"
        );
    }

    #[test]
    fn unavailable_offline_policy_maintenance_never_rejects_an_accepted_action() {
        let coordinate = Axial::ZERO;
        let mut view = MatchView::connecting(1);
        view.authority = AuthorityState::Offline;
        view.cells.insert(coordinate, cell(coordinate, 10));
        view.cluster_policies.insert(
            coordinate,
            ClusterPolicyView {
                kind: ClusterPolicy::Directional,
                orientation: Axial::ZERO,
                revision: 4,
            },
        );
        view.rebuild_chunk_index();

        let mut app = App::new();
        app.add_message::<ServerUpdate>()
            .insert_resource(view)
            .add_systems(Update, apply_server_updates);
        app.world_mut().write_message(ServerUpdate::Accepted {
            command_id: None,
            summary: "Action remains accepted".to_owned(),
            patches: vec![CellPatch {
                coordinate,
                owner: Some(1),
                infantry: 75,
            }],
            flow: None,
            front: None,
        });
        app.update();

        let view = app.world().resource::<MatchView>();
        assert_eq!(view.cell(coordinate).unwrap().infantry, 75);
        assert_eq!(view.latest_result, "Action remains accepted");
        assert_eq!(view.toast.as_ref().unwrap().kind, ToastKind::Success);
    }

    #[test]
    fn formation_rejects_unrelated_inbound_reservations_and_allows_superseded_ones() {
        let left = Axial::ZERO;
        let right = Axial::new(1, 0);
        let selected = BTreeSet::from([left, right]);
        let mut view = MatchView::connecting(1);
        view.cells.insert(left, cell(left, 80));
        view.cells.insert(right, cell(right, 0));
        view.set_retask_projection(RetaskProjection {
            active_order_ids: BTreeSet::from([7]),
            order_strength_by_cell: BTreeMap::from([(7, BTreeMap::from([(left, 10)]))]),
            active_strength_by_cell: BTreeMap::from([(left, 10)]),
            destination_reservations_by_order: BTreeMap::from([(7, BTreeMap::from([(right, 20)]))]),
            ..Default::default()
        });

        let unrelated = view
            .project_order_selection(&selected, &BTreeSet::new())
            .expect("owned selection");
        assert_eq!(
            projected_redistribution_distribution(
                &view,
                &unrelated,
                CoreDistributionPreset::Balance,
            )
            .expect_err("unrelated inbound reservation must block redistribution"),
            "A selected cell is reserved by another active order"
        );

        let superseded = view
            .project_order_selection(&selected, &BTreeSet::from([7]))
            .expect("active order may be superseded");
        assert!(
            superseded
                .unrelated_destination_reservations_by_cell
                .is_empty()
        );
        projected_redistribution_distribution(&view, &superseded, CoreDistributionPreset::Balance)
            .expect("superseding the reserving order removes the overlap");
    }

    #[test]
    fn formation_balances_each_owned_component_independently() {
        let coordinates = [
            Axial::new(0, 0),
            Axial::new(1, 0),
            Axial::new(5, 0),
            Axial::new(6, 0),
        ];
        let mut view = MatchView::connecting(1);
        for (coordinate, infantry) in coordinates.into_iter().zip([80, 0, 20, 0]) {
            view.cells.insert(coordinate, cell(coordinate, infantry));
        }
        let selected = coordinates.into_iter().collect::<BTreeSet<_>>();
        let projection = view
            .project_order_selection(&selected, &BTreeSet::new())
            .expect("owned sources");

        let plan = projected_redistribution_distribution(
            &view,
            &projection,
            CoreDistributionPreset::Balance,
        )
        .expect("component-local balance");

        assert_eq!(plan.final_strength_by_cell[&Axial::new(0, 0)], 40);
        assert_eq!(plan.final_strength_by_cell[&Axial::new(1, 0)], 40);
        assert_eq!(plan.final_strength_by_cell[&Axial::new(5, 0)], 10);
        assert_eq!(plan.final_strength_by_cell[&Axial::new(6, 0)], 10);
    }

    #[test]
    fn formation_connects_selected_cells_through_an_unselected_owned_corridor() {
        let left = Axial::new(0, 0);
        let corridor = Axial::new(1, 0);
        let right = Axial::new(2, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(left, cell(left, 80));
        view.cells.insert(corridor, cell(corridor, 0));
        view.cells.insert(right, cell(right, 0));
        let selected = BTreeSet::from([left, right]);
        let projection = view
            .project_order_selection(&selected, &BTreeSet::new())
            .expect("owned endpoints");

        let plan = projected_redistribution_distribution(
            &view,
            &projection,
            CoreDistributionPreset::Balance,
        )
        .expect("corridor joins endpoints");

        assert_eq!(plan.final_strength_by_cell[&left], 40);
        assert_eq!(plan.final_strength_by_cell[&right], 40);
        assert!(!plan.final_strength_by_cell.contains_key(&corridor));
    }

    #[test]
    fn offline_reshape_moves_all_available_troops_into_the_drawn_shape() {
        let source = Axial::ZERO;
        let target = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 80));
        view.cells.insert(target, cell(target, 0));
        view.rebuild_chunk_index();

        let update = resolve_reshape_with_retask(
            &view,
            &BTreeSet::from([source]),
            &BTreeSet::from([target]),
            &BTreeSet::new(),
        );
        let ServerUpdate::Accepted { patches, .. } = update else {
            panic!("owned reachable destination shape should be accepted");
        };
        let infantry = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .map(|patch| patch.infantry)
        };
        assert_eq!(infantry(source), Some(0));
        assert_eq!(infantry(target), Some(80));
    }

    #[test]
    fn offline_reshape_expands_beyond_the_source_footprint() {
        let source = Axial::ZERO;
        let targets = BTreeSet::from([Axial::new(1, 0), Axial::new(1, -1), Axial::new(0, -1)]);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 90));
        for &target in &targets {
            view.cells.insert(target, cell(target, 0));
        }
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted { patches, .. } = resolve_reshape_with_retask(
            &view,
            &BTreeSet::from([source]),
            &targets,
            &BTreeSet::new(),
        ) else {
            panic!("a larger connected owned target shape should be accepted");
        };
        let infantry = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .map(|patch| patch.infantry)
        };
        assert_eq!(infantry(source), Some(0));
        assert!(targets.iter().all(|target| infantry(*target) == Some(30)));
    }

    #[test]
    fn offline_reshape_contracts_into_a_target_outside_the_source_footprint() {
        let sources = BTreeSet::from([Axial::ZERO, Axial::new(1, 0), Axial::new(0, 1)]);
        let target = Axial::new(1, 1);
        let mut view = MatchView::connecting(1);
        for &source in &sources {
            view.cells.insert(source, cell(source, 30));
        }
        view.cells.insert(target, cell(target, 0));
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted { patches, .. } = resolve_reshape_with_retask(
            &view,
            &sources,
            &BTreeSet::from([target]),
            &BTreeSet::new(),
        ) else {
            panic!("a smaller connected owned target shape should be accepted");
        };
        let infantry = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .map(|patch| patch.infantry)
        };
        assert!(sources.iter().all(|source| infantry(*source) == Some(0)));
        assert_eq!(infantry(target), Some(90));
    }

    #[test]
    fn offline_reshape_keeps_excess_on_sources_when_the_drawn_shape_is_full() {
        let source = Axial::ZERO;
        let target = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 80));
        let mut constrained_target = cell(target, 0);
        constrained_target.military_capacity = 10;
        view.cells.insert(target, constrained_target);
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted {
            summary, patches, ..
        } = resolve_reshape_with_retask(
            &view,
            &BTreeSet::from([source]),
            &BTreeSet::from([target]),
            &BTreeSet::new(),
        )
        else {
            panic!("an undersized destination should reshape as far as capacity permits");
        };

        let infantry = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .map(|patch| patch.infantry)
        };
        assert_eq!(infantry(source), Some(70));
        assert_eq!(infantry(target), Some(10));
        assert!(summary.contains("10 infantry fit"));
        assert!(summary.contains("70 stay outside"));
    }

    #[test]
    fn offline_reshape_contracts_a_large_selection_best_effort() {
        let sources = (0..12).map(|q| Axial::new(q, 0)).collect::<BTreeSet<_>>();
        let targets = BTreeSet::from([Axial::new(12, 0), Axial::new(13, 0)]);
        let mut view = MatchView::connecting(1);
        for &source in &sources {
            view.cells.insert(source, cell(source, 40));
        }
        for &target in &targets {
            view.cells.insert(target, cell(target, 0));
        }
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted { patches, .. } =
            resolve_reshape_with_retask(&view, &sources, &targets, &BTreeSet::new())
        else {
            panic!("a screenshot-scale contraction should apply best effort");
        };
        let infantry = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .map_or_else(
                    || view.cell(coordinate).map_or(0, |cell| cell.infantry),
                    |p| p.infantry,
                )
        };

        assert!(targets.iter().all(|target| infantry(*target) == 100));
        assert_eq!(
            sources.iter().map(|source| infantry(*source)).sum::<u64>(),
            280
        );
        assert_eq!(patches.iter().map(|patch| patch.infantry).sum::<u64>(), 480);
        assert!(sources.iter().any(|source| infantry(*source) > 0));
    }

    #[test]
    fn reshape_rejects_only_unrelated_reservations_on_drawn_targets() {
        let source = Axial::ZERO;
        let target = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 80));
        view.cells.insert(target, cell(target, 0));
        view.set_retask_projection(RetaskProjection {
            destination_reservations_by_order: BTreeMap::from([(
                7,
                BTreeMap::from([(target, 20)]),
            )]),
            ..Default::default()
        });

        let projection = view
            .project_order_selection(&BTreeSet::from([source]), &BTreeSet::new())
            .expect("owned source");
        assert_eq!(
            projected_shape_distribution(&view, &projection, &BTreeSet::from([target]))
                .expect_err("reserved drawn target must reject"),
            "Destination shape overlaps an inbound destination reserved by another active order"
        );

        view.set_retask_projection(RetaskProjection {
            destination_reservations_by_order: BTreeMap::from([(
                7,
                BTreeMap::from([(source, 20)]),
            )]),
            ..Default::default()
        });
        let source_only_reservation = view
            .project_order_selection(&BTreeSet::from([source]), &BTreeSet::new())
            .expect("owned source");
        projected_shape_distribution(&view, &source_only_reservation, &BTreeSet::from([target]))
            .expect("a reservation outside the drawn targets does not overlap Reshape");
    }

    #[test]
    fn reshape_moves_reachable_components_and_leaves_stranded_components_unchanged() {
        let local_source = Axial::ZERO;
        let local_target = Axial::new(1, 0);
        let stranded_source = Axial::new(5, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(local_source, cell(local_source, 40));
        view.cells.insert(local_target, cell(local_target, 0));
        view.cells
            .insert(stranded_source, cell(stranded_source, 30));
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted {
            summary, patches, ..
        } = resolve_reshape_with_retask(
            &view,
            &BTreeSet::from([local_source, stranded_source]),
            &BTreeSet::from([local_target]),
            &BTreeSet::new(),
        )
        else {
            panic!("the reachable component should proceed independently");
        };

        assert!(
            patches
                .iter()
                .any(|patch| { patch.coordinate == local_source && patch.infantry == 0 })
        );
        assert!(
            patches
                .iter()
                .any(|patch| { patch.coordinate == local_target && patch.infantry == 40 })
        );
        assert!(
            !patches
                .iter()
                .any(|patch| patch.coordinate == stranded_source)
        );
        assert_eq!(
            view.cell(stranded_source).map(|cell| cell.infantry),
            Some(30)
        );
        assert!(summary.contains("30 stay outside"));
    }

    #[test]
    fn reshape_ignores_an_empty_source_component_without_a_destination() {
        let local_source = Axial::ZERO;
        let local_target = Axial::new(1, 0);
        let empty_stranded_source = Axial::new(5, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(local_source, cell(local_source, 40));
        view.cells.insert(local_target, cell(local_target, 0));
        view.cells
            .insert(empty_stranded_source, cell(empty_stranded_source, 0));
        view.rebuild_chunk_index();

        let projection = view
            .project_order_selection(
                &BTreeSet::from([local_source, empty_stranded_source]),
                &BTreeSet::new(),
            )
            .expect("owned sources");

        let plan =
            projected_shape_distribution(&view, &projection, &BTreeSet::from([local_target]))
                .expect("an empty remote component cannot veto reachable movement");
        assert_eq!(plan.final_strength_by_cell[&local_source], 0);
        assert_eq!(plan.final_strength_by_cell[&local_target], 40);
        assert!(plan.excluded.contains(&empty_stranded_source));
    }

    #[test]
    fn reshape_never_moves_troops_to_a_disconnected_target_component() {
        let source = Axial::new(0, 0);
        let local_target = Axial::new(1, 0);
        let remote_target = Axial::new(5, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 80));
        view.cells.insert(local_target, cell(local_target, 0));
        view.cells.insert(remote_target, cell(remote_target, 0));
        let projection = view
            .project_order_selection(&BTreeSet::from([source]), &BTreeSet::new())
            .expect("owned source");

        let plan = projected_shape_distribution(
            &view,
            &projection,
            &BTreeSet::from([local_target, remote_target]),
        )
        .expect("valid local reshape component");

        assert_eq!(plan.final_strength_by_cell[&source], 0);
        assert_eq!(plan.final_strength_by_cell[&local_target], 80);
        assert!(!plan.final_strength_by_cell.contains_key(&remote_target));
        assert!(plan.excluded.contains(&remote_target));
    }

    #[test]
    fn reshape_metrics_include_selected_troops_already_in_the_target_shape() {
        let unchanged = Axial::ZERO;
        let moving_source = Axial::new(10, 0);
        let moving_target = Axial::new(11, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(unchanged, cell(unchanged, 30));
        view.cells.insert(moving_source, cell(moving_source, 40));
        view.cells.insert(moving_target, cell(moving_target, 0));
        view.rebuild_chunk_index();

        let projection = view
            .project_order_selection(
                &BTreeSet::from([unchanged, moving_source]),
                &BTreeSet::new(),
            )
            .expect("owned sources project");
        let plan = projected_shape_distribution(
            &view,
            &projection,
            &BTreeSet::from([unchanged, moving_target]),
        )
        .expect("the moving component reshapes");

        assert_eq!(plan.participating_strength, 70);
        assert_eq!(plan.destination_capacity, 200);
        assert_eq!(plan.destination_strength, 70);
        assert_eq!(plan.outside_strength, 0);
        assert!(plan.excluded.contains(&unchanged));
        assert_eq!(plan.final_strength_by_cell[&moving_source], 0);
        assert_eq!(plan.final_strength_by_cell[&moving_target], 40);
    }

    #[test]
    fn reshape_rejects_when_no_target_shares_a_source_component() {
        let source = Axial::new(0, 0);
        let remote_target = Axial::new(5, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 80));
        view.cells.insert(remote_target, cell(remote_target, 0));
        let projection = view
            .project_order_selection(&BTreeSet::from([source]), &BTreeSet::new())
            .expect("owned source");

        assert_eq!(
            projected_shape_distribution(&view, &projection, &BTreeSet::from([remote_target]),)
                .expect_err("disconnected reshape must reject"),
            "Destination shape does not move any selected troops"
        );
    }

    #[test]
    fn reshape_rejects_a_no_op_destination() {
        let source = Axial::new(0, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(source, cell(source, 80));
        let projection = view
            .project_order_selection(&BTreeSet::from([source]), &BTreeSet::new())
            .expect("owned source");

        assert_eq!(
            projected_shape_distribution(&view, &projection, &BTreeSet::from([source]))
                .expect_err("no-op reshape must reject"),
            "Destination shape does not move any selected troops"
        );
    }

    #[test]
    fn offline_retask_redistributes_selected_packets_and_preserves_unrelated_strength() {
        let left = Axial::ZERO;
        let right = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(left, cell(left, 100));
        view.cells.insert(right, cell(right, 20));
        view.set_retask_projection(RetaskProjection {
            handle_orders: BTreeMap::new(),
            active_order_ids: BTreeSet::from([7, 8]),
            background_policy_order_ids: BTreeSet::new(),
            order_source_cells: BTreeMap::new(),
            order_strength_by_cell: BTreeMap::from([
                (7, BTreeMap::from([(left, 30), (right, 10)])),
                (8, BTreeMap::from([(left, 20)])),
            ]),
            active_strength_by_cell: BTreeMap::from([(left, 50), (right, 10)]),
            destination_reservations_by_order: BTreeMap::new(),
            destination_claims_by_order: BTreeMap::new(),
        });
        view.rebuild_chunk_index();

        let projected = view
            .project_order_selection(&BTreeSet::new(), &BTreeSet::from([7]))
            .expect("selected active order projects");
        assert_eq!(projected.unaffected_strength_by_cell[&left], 20);
        assert_eq!(projected.affected_strength_by_cell[&left], 80);

        let update = resolve_redistribution_with_retask(
            &view,
            &BTreeSet::new(),
            &BTreeSet::from([7]),
            RedistributionPreset::Balance,
            None,
        );
        let ServerUpdate::Accepted { patches, .. } = update else {
            panic!("handle-only redistribution should be accepted");
        };
        let target = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .map(|patch| patch.infantry)
        };
        assert_eq!(target(left), Some(64));
        assert_eq!(target(right), Some(56));
        assert_eq!(patches.iter().map(|patch| patch.infantry).sum::<u64>(), 120);

        assert!(matches!(
            resolve_redistribution_with_retask(
                &view,
                &BTreeSet::new(),
                &BTreeSet::from([9]),
                RedistributionPreset::Balance,
                None,
            ),
            ServerUpdate::Rejected { reason, .. } if reason.contains("no longer active")
        ));
    }

    #[test]
    fn offline_push_leaves_cliff_isolated_sources_in_place() {
        let sources = BTreeSet::from([Axial::ZERO, Axial::new(1, 0), Axial::new(2, 0)]);
        let mut view = MatchView::connecting(1);
        for source in &sources {
            let mut cell = cell(*source, 20);
            if *source == Axial::new(1, 0) {
                cell.elevation = 3;
            }
            view.cells.insert(cell.coordinate, cell);
        }
        let target = Axial::new(3, 0);
        view.cells.insert(target, neutral_cell(target));
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted { patches, .. } =
            resolve_push_front(&view, &sources, Axial::new(1, 0), 50)
        else {
            panic!("the reachable front component should still advance");
        };
        assert!(patches.iter().any(|patch| patch.coordinate == target));
        assert!(patches.iter().all(|patch| patch.coordinate != Axial::ZERO));
    }
}
