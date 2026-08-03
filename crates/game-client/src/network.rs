//! Narrow authority boundary for the native client.
//!
//! Rendering and input emit [`ClientIntent`] and consume [`ServerUpdate`]. The
//! online adapter and offline fixture both translate into this boundary, so
//! camera, selection, overlays, and HUD systems remain transport-agnostic.

use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, BinaryHeap},
};

use bevy::prelude::*;
use hex_core::{
    Axial, Cell, DistributionPreset as CoreDistributionPreset, ForceComposition,
    FrontSelectionError, HexMap, redistribution_targets_with_commitment, selected_front_edges,
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
        FrontSelectionError::DisconnectedFront => {
            "The selected boundary creates separate front arcs"
        }
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
