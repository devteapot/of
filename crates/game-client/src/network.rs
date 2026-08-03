//! Narrow authority boundary for the native client.
//!
//! Rendering and input emit [`ClientIntent`] and consume [`ServerUpdate`]. The
//! online adapter and offline fixture both translate into this boundary, so
//! camera, selection, overlays, and HUD systems remain transport-agnostic.

use std::collections::{BTreeMap, BTreeSet};

use bevy::prelude::*;
use hex_core::Axial;

use crate::{
    geometry::axial_to_plane,
    model::{
        ActiveFlow, ActiveFront, MatchPhase, MatchView, ToastKind, reachability_to_destinations,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RedistributionPreset {
    Balance,
    FrontLoad,
}

#[derive(Message, Clone, Debug)]
pub enum ClientIntent {
    Transfer {
        sources: BTreeSet<Axial>,
        destinations: BTreeSet<Axial>,
        amount_percent: u8,
    },
    Redistribute {
        cells: BTreeSet<Axial>,
        preset: RedistributionPreset,
        direction: Option<Vec2>,
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
            ClientIntent::Transfer {
                sources,
                destinations,
                amount_percent,
            } => resolve_transfer(&view, sources, destinations, *amount_percent),
            ClientIntent::Redistribute {
                cells,
                preset,
                direction,
            } => resolve_redistribution(&view, cells, *preset, *direction),
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

fn resolve_transfer(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    destinations: &BTreeSet<Axial>,
    amount_percent: u8,
) -> ServerUpdate {
    if sources.is_empty() || destinations.is_empty() {
        return rejection("Select at least one source and destination", None);
    }
    if let Some(invalid) = sources
        .iter()
        .find(|coordinate| !view.is_local_owned(**coordinate))
    {
        return rejection("Sources must be owned by the local player", Some(*invalid));
    }
    if let Some(invalid) = destinations.iter().find(|coordinate| {
        view.cell(**coordinate)
            .is_none_or(|cell| cell.is_water() || cell.blocked)
    }) {
        return rejection("Destination is water or impassable", Some(*invalid));
    }
    let primary = *destinations.first().expect("validated destination");
    let primary_set = BTreeSet::from([primary]);
    let reverse = reachability_to_destinations(view, &primary_set);
    let reachable_sources = reverse
        .reachable_sources(sources)
        .into_iter()
        .filter(|source| *source != primary)
        .collect::<BTreeSet<_>>();
    let Some(route) = reverse.route_from_any(&reachable_sources) else {
        return rejection(
            "No traversable route; cliff or water blocks the corridor",
            Some(primary),
        );
    };

    let percent = u64::from(amount_percent.clamp(10, 100));
    let requested_by_source = reachable_sources
        .iter()
        .filter_map(|coordinate| view.cell(*coordinate))
        .map(|cell| (cell.coordinate, cell.infantry * percent / 100))
        .collect::<BTreeMap<_, _>>();
    let requested = requested_by_source
        .values()
        .copied()
        .fold(0_u64, u64::saturating_add);
    if requested == 0 {
        return rejection(
            "Selected sources have no movable infantry",
            sources.first().copied(),
        );
    }

    let destination = view.cell(primary).expect("validated destination");
    let attacking = destination.owner != Some(view.local_player);
    let moved = if attacking {
        requested
    } else {
        requested.min(destination.free_capacity())
    };
    if moved == 0 {
        return rejection("Destination has no free military capacity", Some(primary));
    }

    // Offline commands resolve immediately, so there is no authoritative queue
    // that can safely own strength above the destination's free capacity. Debit
    // exactly the accepted amount, spread proportionally and deterministically
    // across the sorted reachable sources; every unaccepted soldier stays put.
    let mut moved_by_source = BTreeMap::new();
    let mut allocated = 0_u64;
    for (coordinate, available) in &requested_by_source {
        let share = if moved == requested {
            *available
        } else {
            (u128::from(*available) * u128::from(moved) / u128::from(requested)) as u64
        };
        moved_by_source.insert(*coordinate, share);
        allocated = allocated.saturating_add(share);
    }
    let mut remainder = moved.saturating_sub(allocated);
    for (coordinate, available) in &requested_by_source {
        if remainder == 0 {
            break;
        }
        let share = moved_by_source
            .get_mut(coordinate)
            .expect("source allocation was initialized");
        if *share < *available {
            *share += 1;
            remainder -= 1;
        }
    }
    debug_assert_eq!(remainder, 0);

    let mut changed = BTreeMap::<Axial, (Option<u32>, u64)>::new();
    for (source, strength) in moved_by_source {
        let cell = view.cell(source).expect("validated source");
        changed.insert(source, (cell.owner, cell.infantry.saturating_sub(strength)));
    }

    let mut front = None;
    let summary;
    if attacking {
        let defenders = destination.infantry;
        if requested > defenders {
            let survivors = requested
                .saturating_sub(defenders)
                .min(destination.military_capacity);
            changed.insert(primary, (Some(view.local_player), survivors));
            summary = format!(
                "Attack staged · {requested} moved · {} defenders removed · territory captured",
                defenders.min(requested)
            );
        } else {
            changed.insert(primary, (destination.owner, defenders - requested));
            summary = format!("Attack staged · {requested} casualties inflicted · front holds");
        }
        if route.len() >= 2 {
            front = Some(ActiveFront {
                friendly: route[route.len() - 2],
                hostile: primary,
                intensity: (requested as f32 / 100.0).clamp(0.25, 1.0),
                age: 0.0,
            });
        }
    } else {
        changed.insert(primary, (destination.owner, destination.infantry + moved));
        let retained = requested.saturating_sub(moved);
        summary = if retained > 0 {
            format!(
                "Transfer accepted · {moved} infantry · {retained} retained at sources (capacity limit)"
            )
        } else {
            format!(
                "Transfer accepted · {moved} infantry · ETA ≈ {}s",
                route.len() * 2
            )
        };
    }

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
            route,
            strength: moved,
            attacking,
            age: 0.0,
            lifetime: 10.0,
        }),
        front,
    }
}

fn resolve_redistribution(
    view: &MatchView,
    cells: &BTreeSet<Axial>,
    preset: RedistributionPreset,
    direction: Option<Vec2>,
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
    if preset == RedistributionPreset::FrontLoad
        && direction.is_none_or(|value| value.length_squared() < 0.001)
    {
        return rejection("Front-load direction is too short", cells.first().copied());
    }

    let total: u64 = cells
        .iter()
        .filter_map(|coordinate| view.cell(*coordinate))
        .map(|cell| cell.infantry)
        .sum();
    let direction = direction.unwrap_or(Vec2::X).normalize_or_zero();
    let projections: Vec<_> = cells
        .iter()
        .map(|coordinate| (*coordinate, axial_to_plane(*coordinate).dot(direction)))
        .collect();
    let min_projection = projections
        .iter()
        .map(|(_, projection)| *projection)
        .fold(f32::INFINITY, f32::min);
    let max_projection = projections
        .iter()
        .map(|(_, projection)| *projection)
        .fold(f32::NEG_INFINITY, f32::max);
    let span = (max_projection - min_projection).max(0.001);

    let weights: Vec<_> = projections
        .iter()
        .map(|(coordinate, projection)| {
            let cell = view.cell(*coordinate).expect("validated cell");
            let bias = match preset {
                RedistributionPreset::Balance => 1.0,
                RedistributionPreset::FrontLoad => {
                    0.35 + 1.3 * ((*projection - min_projection) / span)
                }
            };
            (*coordinate, cell.military_capacity as f32 * bias)
        })
        .collect();
    let total_weight: f32 = weights.iter().map(|(_, weight)| *weight).sum();
    let mut remaining = total;
    let mut patches = Vec::with_capacity(weights.len());
    for (index, (coordinate, weight)) in weights.iter().enumerate() {
        let cell = view.cell(*coordinate).expect("validated cell");
        let target = if index + 1 == weights.len() {
            remaining
        } else {
            ((total as f32 * *weight / total_weight).round() as u64).min(remaining)
        }
        .min(cell.military_capacity);
        remaining = remaining.saturating_sub(target);
        patches.push(CellPatch {
            coordinate: *coordinate,
            owner: cell.owner,
            infantry: target,
        });
    }

    let label = match preset {
        RedistributionPreset::Balance => "Balance",
        RedistributionPreset::FrontLoad => "Front-load",
    };
    ServerUpdate::Accepted {
        command_id: None,
        summary: format!("{label} redistribution accepted · {total} infantry conserved"),
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

    fn disconnected_transfer_view() -> MatchView {
        let mut view = MatchView::connecting(1);
        for cell in [
            cell(Axial::ZERO, 10),
            cell(Axial::new(10, 0), 20),
            cell(Axial::new(11, 0), 0),
        ] {
            view.cells.insert(cell.coordinate, cell);
        }
        view.rebuild_chunk_index();
        view
    }

    #[test]
    fn offline_transfer_uses_only_sources_that_reach_the_primary_destination() {
        let view = disconnected_transfer_view();
        let isolated = Axial::ZERO;
        let reachable = Axial::new(10, 0);
        let destination = Axial::new(11, 0);
        let update = resolve_transfer(
            &view,
            &BTreeSet::from([isolated, reachable]),
            &BTreeSet::from([destination]),
            100,
        );

        let ServerUpdate::Accepted { patches, flow, .. } = update else {
            panic!("reachable source should produce an accepted transfer");
        };
        assert!(!patches.iter().any(|patch| patch.coordinate == isolated));
        assert_eq!(
            patches
                .iter()
                .find(|patch| patch.coordinate == reachable)
                .map(|patch| patch.infantry),
            Some(0)
        );
        assert_eq!(
            patches
                .iter()
                .find(|patch| patch.coordinate == destination)
                .map(|patch| patch.infantry),
            Some(20)
        );
        let flow = flow.expect("accepted transfer flow");
        assert_eq!(flow.strength, 20);
        assert_eq!(flow.route, vec![reachable, destination]);
    }

    #[test]
    fn offline_transfer_rejects_when_no_source_reaches_the_primary_destination() {
        let view = disconnected_transfer_view();
        let update = resolve_transfer(
            &view,
            &BTreeSet::from([Axial::ZERO]),
            &BTreeSet::from([Axial::new(11, 0)]),
            100,
        );

        assert!(matches!(update, ServerUpdate::Rejected { .. }));
    }

    #[test]
    fn offline_friendly_transfer_retains_strength_that_exceeds_capacity() {
        let first = Axial::ZERO;
        let second = Axial::new(1, 0);
        let destination = Axial::new(2, 0);
        let mut view = MatchView::connecting(1);
        for cell in [cell(first, 60), cell(second, 40), cell(destination, 90)] {
            view.cells.insert(cell.coordinate, cell);
        }
        view.rebuild_chunk_index();

        let update = resolve_transfer(
            &view,
            &BTreeSet::from([first, second]),
            &BTreeSet::from([destination]),
            100,
        );
        let ServerUpdate::Accepted {
            summary,
            patches,
            flow,
            ..
        } = update
        else {
            panic!("capacity-limited friendly transfer should be accepted");
        };

        let infantry_after = |coordinate| {
            patches
                .iter()
                .find(|patch| patch.coordinate == coordinate)
                .map(|patch| patch.infantry)
                .expect("every changed cell has a patch")
        };
        assert_eq!(infantry_after(first), 54);
        assert_eq!(infantry_after(second), 36);
        assert_eq!(infantry_after(destination), 100);
        assert_eq!(
            infantry_after(first) + infantry_after(second) + infantry_after(destination),
            190,
            "friendly transfer must conserve infantry"
        );
        assert!(summary.contains("90 retained at sources"));
        assert_eq!(flow.expect("accepted transfer flow").strength, 10);
    }
}
