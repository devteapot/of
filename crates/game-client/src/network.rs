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
    model::{ActiveFlow, ActiveFront, MatchPhase, MatchView, ToastKind, find_route},
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
    let Some(route) = find_route(view, sources, destinations) else {
        return rejection(
            "No traversable route; cliff or water blocks the corridor",
            destinations.first().copied(),
        );
    };

    let percent = u64::from(amount_percent.clamp(10, 100));
    let requested: u64 = sources
        .iter()
        .filter_map(|coordinate| view.cell(*coordinate))
        .map(|cell| cell.infantry * percent / 100)
        .sum();
    if requested == 0 {
        return rejection(
            "Selected sources have no movable infantry",
            sources.first().copied(),
        );
    }

    let mut changed = BTreeMap::<Axial, (Option<u32>, u64)>::new();
    for source in sources {
        let cell = view.cell(*source).expect("validated source");
        let moved = cell.infantry * percent / 100;
        changed.insert(*source, (cell.owner, cell.infantry.saturating_sub(moved)));
    }

    let primary = *destinations.first().expect("validated destination");
    let destination = view.cell(primary).expect("validated destination");
    let attacking = destination.owner != Some(view.local_player);
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
        let free = destination.free_capacity();
        let arriving = requested.min(free);
        changed.insert(
            primary,
            (destination.owner, destination.infantry + arriving),
        );
        let queued = requested.saturating_sub(arriving);
        summary = if queued > 0 {
            format!("Transfer accepted · {arriving} arriving · {queued} queued at bottleneck")
        } else {
            format!(
                "Transfer accepted · {arriving} infantry · ETA ≈ {}s",
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
            strength: requested,
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
