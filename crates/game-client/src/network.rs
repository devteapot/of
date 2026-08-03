//! Narrow authority boundary for the native client.
//!
//! Rendering and input emit [`ClientIntent`] and consume [`ServerUpdate`]. The
//! online adapter and offline fixture both translate into this boundary, so
//! camera, selection, overlays, and HUD systems remain transport-agnostic.

use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque},
};

use bevy::prelude::*;
use hex_core::{
    Axial, Cell, DirectedFrontEdge, DistributionPreset as CoreDistributionPreset, ForceComposition,
    FrontSelectionError, HexMap, redistribution_targets_with_commitment, selected_all_front_edges,
    selected_front_edges,
};

use crate::{
    geometry::axial_to_plane,
    model::{ActiveFlow, ActiveFront, MatchPhase, MatchView, ToastKind},
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
    PushFront {
        sources: BTreeSet<Axial>,
        direction: Axial,
        commitment_percent: u8,
    },
    ExpandAll {
        sources: BTreeSet<Axial>,
        commitment_percent: u8,
    },
    CancelExpandAll {
        sources: BTreeSet<Axial>,
    },
    CancelPush {
        sources: BTreeSet<Axial>,
        direction: Axial,
    },
    Redistribute {
        cells: BTreeSet<Axial>,
        preset: RedistributionPreset,
        direction: Option<Vec2>,
        amount_percent: u8,
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
    view: Res<MatchView>,
    mut updates: MessageWriter<ServerUpdate>,
) {
    for intent in intents.read() {
        let update = match intent {
            ClientIntent::PushFront {
                sources,
                direction,
                commitment_percent,
            } => resolve_push_front(&view, sources, *direction, *commitment_percent),
            ClientIntent::ExpandAll {
                sources,
                commitment_percent,
            } => resolve_expand_all(&view, sources, *commitment_percent),
            ClientIntent::CancelExpandAll { .. } => ServerUpdate::Accepted {
                command_id: None,
                summary: "Matching active Expand All operations stopped".to_owned(),
                patches: Vec::new(),
                flow: None,
                front: None,
            },
            ClientIntent::CancelPush { .. } => ServerUpdate::Accepted {
                command_id: None,
                summary: "Matching active Push Front operations stopped".to_owned(),
                patches: Vec::new(),
                flow: None,
                front: None,
            },
            ClientIntent::Redistribute {
                cells,
                preset,
                direction,
                amount_percent,
            } => resolve_redistribution(&view, cells, *preset, *direction, *amount_percent),
            ClientIntent::SetMobilization { target } => ServerUpdate::MobilizationChanged {
                command_id: None,
                target: target.clamp(0.0, 1.0),
            },
        };
        updates.write(update);
    }
}

pub fn apply_server_updates(mut updates: MessageReader<ServerUpdate>, mut view: ResMut<MatchView>) {
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

fn resolve_push_front(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    direction: Axial,
    commitment_percent: u8,
) -> ServerUpdate {
    if let Some(invalid) = sources.iter().find(|coordinate| {
        view.cell(**coordinate).is_none_or(|cell| {
            !view.is_local_owned(**coordinate) || !cell.is_land() || cell.blocked
        })
    }) {
        return rejection("Push sources must be owned passable ground", Some(*invalid));
    }

    let edges = match selected_front_edges(sources, direction, |source, target| {
        let Some(source) = view.cell(source) else {
            return false;
        };
        view.cell(target).is_some_and(|target| {
            target.is_land()
                && !target.blocked
                && target.owner != Some(view.local_player)
                && (i32::from(source.elevation) - i32::from(target.elevation)).unsigned_abs() <= 1
        })
    }) {
        Ok(edges) => edges,
        Err(error) => return rejection(front_error_message(error), sources.first().copied()),
    };

    let front_sources = edges
        .iter()
        .map(|edge| edge.source)
        .collect::<BTreeSet<_>>();
    let assignments = selected_front_assignments(view, sources, &front_sources);
    if assignments.len() != sources.len() {
        let relevant = sources
            .iter()
            .find(|source| !assignments.contains_key(source))
            .copied();
        return rejection(
            "Selected troops cannot reach the front inside the selected region",
            relevant,
        );
    }

    let percentage = u64::from(commitment_percent.clamp(10, 100));
    let requested_by_source = sources
        .iter()
        .filter_map(|coordinate| view.cell(*coordinate))
        .map(|cell| {
            (
                cell.coordinate,
                cell.infantry.saturating_mul(percentage) / 100,
            )
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
    let mut changed = BTreeMap::<Axial, (Option<u32>, u64)>::new();
    let mut committed_by_boundary = BTreeMap::<Axial, u64>::new();
    for (source, source_request) in &requested_by_source {
        let boundary = assignments
            .get(source)
            .expect("every selected source was assigned to the front");
        let committed = committed_by_boundary.entry(*boundary).or_default();
        *committed = committed.saturating_add(*source_request);
        let cell = view.cell(*source).expect("push source was validated");
        changed.insert(
            *source,
            (cell.owner, cell.infantry.saturating_sub(*source_request)),
        );
    }
    let committed = committed_by_boundary.values().copied().sum();
    if committed == 0 {
        return rejection(
            "The selected front has no infantry to commit",
            target_by_boundary.values().next().copied(),
        );
    }

    let mut captured = 0_u32;
    let mut defender_losses = 0_u64;
    let mut representative_route = Vec::new();
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
                || destination.owner == Some(view.local_player)
                || (i32::from(from.elevation) - i32::from(destination.elevation)).unsigned_abs() > 1
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
        }
    }

    let first_edge = edges[0];
    let summary = format!(
        "Sustained Push Front accepted · {committed} committed · {captured} cells captured · {defender_losses} defender losses"
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

/// Every passable selected-to-neutral edge around a selected region.
///
/// A target is deliberately neutral-only: Expand All grows into unclaimed
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
                && (i32::from(source_cell.elevation) - i32::from(target_cell.elevation))
                    .unsigned_abs()
                    <= 1
        })
    })
}

pub(crate) const MAX_WAVE_PREVIEW_RINGS: u16 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExpandWaveError {
    Front(FrontSelectionError),
    InternalRoute,
}

#[derive(Clone, Debug, Default)]
struct ExpandWaveTopology {
    initial_edges: Vec<DirectedFrontEdge>,
    selected_depth: BTreeMap<Axial, u16>,
    outside_depth: BTreeMap<Axial, u16>,
    outgoing: BTreeMap<Axial, Vec<Axial>>,
    parents: BTreeMap<Axial, Vec<Axial>>,
    truncated: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ExpandWaveForecast {
    pub initial_edges: Vec<DirectedFrontEdge>,
    pub reached_depth: BTreeMap<Axial, u16>,
    pub max_internal_depth: u16,
    pub strength_upper_bound: u64,
    pub first_ring_capacity: u64,
    pub truncated: bool,
}

fn build_expand_wave_topology(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    max_rings: Option<u16>,
) -> Result<ExpandWaveTopology, ExpandWaveError> {
    let initial_edges = expand_all_front_edges(view, sources).map_err(ExpandWaveError::Front)?;
    let boundary = initial_edges
        .iter()
        .map(|edge| edge.source)
        .collect::<BTreeSet<_>>();
    let selected_depth = selected_depths_to_boundary(view, sources, &boundary);
    if selected_depth.len() != sources.len() {
        return Err(ExpandWaveError::InternalRoute);
    }

    let mut topology = ExpandWaveTopology {
        initial_edges: initial_edges.clone(),
        selected_depth,
        ..Default::default()
    };

    // Inside the selected seed, strength moves down every shortest local
    // depth. A central pool can therefore branch, and equal-depth routes merge
    // naturally before reaching the outside perimeter.
    for &source in sources {
        let depth = topology.selected_depth[&source];
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
                && (i32::from(from.elevation) - i32::from(to.elevation)).unsigned_abs() <= 1
        })
}

fn wave_continuation_target_is_eligible(view: &MatchView, from: Axial, target: Axial) -> bool {
    wave_edge_is_traversable(view, from, target)
        && view
            .cell(target)
            .is_some_and(|cell| cell.owner.is_none() || cell.owner == Some(view.local_player))
}

fn boundary_strength_pools(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    commitment_percent: u8,
    topology: &ExpandWaveTopology,
) -> (u64, BTreeMap<Axial, u64>, BTreeMap<Axial, u64>) {
    let percentage = u64::from(commitment_percent.clamp(10, 100));
    let requested_by_source = sources
        .iter()
        .filter_map(|coordinate| view.cell(*coordinate))
        .map(|cell| {
            (
                cell.coordinate,
                cell.infantry.saturating_mul(percentage) / 100,
            )
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
            distribute_evenly(
                amount,
                topology
                    .outgoing
                    .get(&coordinate)
                    .map_or(&[][..], Vec::as_slice),
                &mut pools,
            );
        }
    }
    (requested, requested_by_source, pools)
}

fn distribute_evenly(total: u64, targets: &[Axial], incoming: &mut BTreeMap<Axial, u64>) {
    if total == 0 || targets.is_empty() {
        return;
    }
    let count = u64::try_from(targets.len()).expect("wave branch count fits u64");
    let base = total / count;
    let remainder = total % count;
    for (index, &target) in targets.iter().enumerate() {
        let share =
            base + u64::from(u64::try_from(index).expect("wave branch index fits u64") < remainder);
        let target_pool = incoming.entry(target).or_default();
        *target_pool = target_pool.saturating_add(share);
    }
}

pub(crate) fn forecast_expand_wave(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    commitment_percent: u8,
    max_rings: u16,
) -> Result<ExpandWaveForecast, ExpandWaveError> {
    let topology = build_expand_wave_topology(view, sources, Some(max_rings))?;
    let (strength_upper_bound, _, boundary_pools) =
        boundary_strength_pools(view, sources, commitment_percent, &topology);
    let mut incoming = BTreeMap::new();
    for (boundary, amount) in boundary_pools {
        distribute_evenly(
            amount,
            topology
                .outgoing
                .get(&boundary)
                .map_or(&[][..], Vec::as_slice),
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
                distribute_evenly(mobile, children, &mut incoming);
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
        truncated: forecast_truncated,
    })
}

fn resolve_expand_all(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    commitment_percent: u8,
) -> ServerUpdate {
    if sources.is_empty() {
        return rejection("Expand All selection is empty", None);
    }
    if let Some(invalid) = sources.iter().find(|coordinate| {
        view.cell(**coordinate).is_none_or(|cell| {
            !view.is_local_owned(**coordinate) || !cell.is_land() || cell.blocked
        })
    }) {
        return rejection(
            "Expand All sources must be owned passable ground",
            Some(*invalid),
        );
    }

    let topology = match build_expand_wave_topology(view, sources, None) {
        Ok(topology) => topology,
        Err(ExpandWaveError::Front(error)) => {
            return rejection(expand_error_message(error), sources.first().copied());
        }
        Err(ExpandWaveError::InternalRoute) => {
            return rejection(
                "Every selected source must reach the perimeter inside the selection",
                sources.first().copied(),
            );
        }
    };
    let (committed, requested_by_source, boundary_pools) =
        boundary_strength_pools(view, sources, commitment_percent, &topology);
    if committed == 0 {
        return rejection(
            "Selected sources have no infantry to dispatch",
            sources.first().copied(),
        );
    }

    let mut changed = BTreeMap::<Axial, (Option<u32>, u64)>::new();
    for (source, source_request) in &requested_by_source {
        let cell = view.cell(*source).expect("expand source was validated");
        changed.insert(
            *source,
            (cell.owner, cell.infantry.saturating_sub(*source_request)),
        );
    }

    let mut incoming = BTreeMap::new();
    for (boundary, amount) in boundary_pools {
        distribute_evenly(
            amount,
            topology
                .outgoing
                .get(&boundary)
                .map_or(&[][..], Vec::as_slice),
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
                distribute_evenly(mobile, children, &mut incoming);
            }
        }
    }

    settle_wave_strength(view, &topology, &mut changed, &terminal_strength);

    ServerUpdate::Accepted {
        command_id: None,
        summary: format!(
            "Expand All accepted · {committed} dispatched at {}% · {captured} neutral cells captured",
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
) -> BTreeMap<Axial, Axial> {
    // Mirror the authoritative reducer's reverse route tree: every selected
    // source is assigned to one stable nearest boundary, so rear troops cannot
    // teleport their commitment to an unrelated section of a wide front.
    let mut labels = BTreeMap::<Axial, (u64, Axial)>::new();
    let mut pending = BinaryHeap::<Reverse<(u64, Axial, Axial)>>::new();
    for &boundary in front_sources {
        labels.insert(boundary, (0, boundary));
        pending.push(Reverse((0, boundary, boundary)));
    }

    while let Some(Reverse((cost, boundary, current))) = pending.pop() {
        if labels.get(&current) != Some(&(cost, boundary)) {
            continue;
        }
        let Some(current_cell) = view.cell(current) else {
            continue;
        };
        let mut neighbors = current.neighbors();
        neighbors.sort_unstable();
        for neighbor in neighbors {
            if !sources.contains(&neighbor) {
                continue;
            }
            let Some(neighbor_cell) = view.cell(neighbor) else {
                continue;
            };
            if !current_cell.is_land()
                || !neighbor_cell.is_land()
                || current_cell.blocked
                || neighbor_cell.blocked
                || (i32::from(current_cell.elevation) - i32::from(neighbor_cell.elevation))
                    .unsigned_abs()
                    > 1
            {
                continue;
            }
            let step_cost = if neighbor_cell.elevation < current_cell.elevation {
                15
            } else {
                10
            };
            let candidate = (cost.saturating_add(step_cost), boundary);
            if labels
                .get(&neighbor)
                .is_none_or(|existing| candidate < *existing)
            {
                labels.insert(neighbor, candidate);
                pending.push(Reverse((candidate.0, candidate.1, neighbor)));
            }
        }
    }

    labels
        .into_iter()
        .map(|(coordinate, (_, boundary))| (coordinate, boundary))
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

const fn front_error_message(error: FrontSelectionError) -> &'static str {
    match error {
        FrontSelectionError::EmptySelection => "Push selection is empty",
        FrontSelectionError::DisconnectedSelection => "Push selection must be connected",
        FrontSelectionError::InvalidDirection => "Push direction is invalid",
        FrontSelectionError::NoEligibleFront => "No non-owned passable front faces that direction",
    }
}

const fn expand_error_message(error: FrontSelectionError) -> &'static str {
    match error {
        FrontSelectionError::EmptySelection => "Expand All selection is empty",
        FrontSelectionError::DisconnectedSelection => {
            "Expand All selection must be one connected region"
        }
        FrontSelectionError::NoEligibleFront => "The selection has no passable neutral frontier",
        FrontSelectionError::InvalidDirection => "Expand All frontier is invalid",
    }
}

fn resolve_redistribution(
    view: &MatchView,
    cells: &BTreeSet<Axial>,
    preset: RedistributionPreset,
    direction: Option<Vec2>,
    amount_percent: u8,
) -> ServerUpdate {
    if cells.len() < 2 {
        return rejection(
            "Redistribution needs at least two owned hexes",
            cells.first().copied(),
        );
    }
    if let Some(invalid) = cells
        .iter()
        .find(|coordinate| !view.is_local_owned(**coordinate))
    {
        return rejection(
            "Redistribution region contains an unowned hex",
            Some(*invalid),
        );
    }
    let core_preset = match preset {
        RedistributionPreset::Balance => CoreDistributionPreset::Balance,
        RedistributionPreset::FrontLoad => {
            let Some(direction) = direction.and_then(world_direction_to_axial) else {
                return rejection("Front-load direction is too short", cells.first().copied());
            };
            CoreDistributionPreset::front_load(direction)
        }
        RedistributionPreset::CoreLoad => CoreDistributionPreset::CoreLoad,
        RedistributionPreset::PerimeterLoad => CoreDistributionPreset::PerimeterLoad,
    };

    let mut map = HexMap::new();
    let mut total = 0_u64;
    for &coordinate in cells {
        let cell = view.cell(coordinate).expect("selection was validated");
        if !cell.is_land() || cell.blocked {
            return rejection(
                "Redistribution needs owned passable ground",
                Some(coordinate),
            );
        }
        total = total.saturating_add(cell.infantry);
        map.insert(Cell {
            coordinate,
            terrain: cell.terrain,
            elevation: cell.elevation,
            capturable: true,
            habitable: true,
            owner: cell.owner,
            civilian_population: cell.civilians,
            civilian_capacity: cell.civilians,
            forces: ForceComposition::infantry(cell.infantry),
            military_capacity: cell.military_capacity,
        });
    }
    let amount_bps = u32::from(amount_percent.clamp(10, 100)) * 100;
    let Ok(distribution) = redistribution_targets_with_commitment(
        &map,
        view.local_player,
        cells.iter().copied(),
        total,
        core_preset,
        amount_bps,
    ) else {
        return rejection(
            "This redistribution cannot be resolved",
            cells.first().copied(),
        );
    };
    let patches = distribution
        .targets
        .into_iter()
        .map(|(coordinate, infantry)| CellPatch {
            coordinate,
            owner: Some(view.local_player),
            infantry,
        })
        .collect();

    let label = match preset {
        RedistributionPreset::Balance => "Balance",
        RedistributionPreset::FrontLoad => "Front-load",
        RedistributionPreset::CoreLoad => "Core-load",
        RedistributionPreset::PerimeterLoad => "Perimeter-load",
    };
    ServerUpdate::Accepted {
        command_id: None,
        summary: format!(
            "{label} redistribution accepted · {}% participation · {total} infantry conserved",
            amount_percent.clamp(10, 100)
        ),
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

fn world_direction_to_axial(direction: Vec2) -> Option<Axial> {
    if direction.length_squared() < 0.001 {
        return None;
    }
    let direction = direction.normalize();
    Axial::DIRECTIONS.into_iter().max_by(|left, right| {
        axial_to_plane(*left)
            .normalize()
            .dot(direction)
            .total_cmp(&axial_to_plane(*right).normalize().dot(direction))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CellView;
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

    fn hex_disk(radius: i32) -> Vec<Axial> {
        (-radius..=radius)
            .flat_map(|q| (-radius..=radius).map(move |r| Axial::new(q, r)))
            .filter(|coordinate| coordinate.distance(Axial::ZERO) <= radius as u64)
            .collect()
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
    fn offline_push_front_feeds_disconnected_boundary_arcs_independently() {
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
        view.cells.insert(blocked_gap, cell(blocked_gap, 0));
        view.cells.insert(lower_target, neutral_cell(lower_target));
        view.rebuild_chunk_index();

        let ServerUpdate::Accepted { patches, flow, .. } =
            resolve_push_front(&view, &sources, direction, 50)
        else {
            panic!("separate eligible boundary arcs should share one connected corridor");
        };
        let patch = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .expect("participating cell should be patched")
        };

        assert!(sources.iter().all(|source| patch(*source).infantry == 10));
        assert_eq!(patch(upper_target).owner, Some(1));
        assert_eq!(patch(lower_target).owner, Some(1));
        let mut arc_strengths = [patch(upper_target).infantry, patch(lower_target).infantry];
        arc_strengths.sort_unstable();
        assert_eq!(arc_strengths, [10, 20]);
        assert_eq!(patches.iter().map(|patch| patch.infantry).sum::<u64>(), 60);
        assert_eq!(flow.expect("representative offline flow").strength, 30);
    }

    #[test]
    fn one_offline_push_command_advances_through_successive_layers() {
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
            panic!("a two-edge neutral frontier should accept Expand All");
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
    fn expand_all_rejects_disconnected_source_regions() {
        let left = Axial::ZERO;
        let right = Axial::new(3, 0);
        let mut view = MatchView::connecting(1);
        for source in [left, right] {
            view.cells.insert(source, cell(source, 20));
            let target = source + Axial::new(0, 1);
            view.cells.insert(target, neutral_cell(target));
        }
        view.rebuild_chunk_index();

        assert!(matches!(
            resolve_expand_all(&view, &BTreeSet::from([left, right]), 50),
            ServerUpdate::Rejected { reason, .. }
                if reason.contains("one connected region")
        ));
    }

    #[test]
    fn offline_balance_respects_per_cell_participation_percentage() {
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
            25,
        );
        let ServerUpdate::Accepted { patches, .. } = update else {
            panic!("percentage balance should be accepted");
        };
        let target = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .map(|patch| patch.infantry)
        };
        assert_eq!(target(left), Some(75));
        assert_eq!(target(right), Some(25));
    }

    #[test]
    fn offline_push_rejects_a_cliff_inside_the_selected_corridor() {
        let sources = BTreeSet::from([Axial::ZERO, Axial::new(1, 0), Axial::new(2, 0)]);
        let mut view = MatchView::connecting(1);
        for source in &sources {
            let mut cell = cell(*source, 20);
            if *source == Axial::new(1, 0) {
                cell.elevation = 3;
            }
            view.cells.insert(cell.coordinate, cell);
        }
        let target = neutral_cell(Axial::new(3, 0));
        view.cells.insert(target.coordinate, target);
        view.rebuild_chunk_index();

        assert!(matches!(
            resolve_push_front(&view, &sources, Axial::new(1, 0), 50),
            ServerUpdate::Rejected { .. }
        ));
    }
}
