//! Deterministic, snapshot-valid expansion planning for benchmark workers.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use hex_core::Axial;

use crate::front_rebalance::MapCell;

/// Exact reducer inputs derived from the current owned component and perimeter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExpansionCommand {
    pub source_seed: u32,
    pub focus: u32,
}

/// Per-player plan: issue a command or skip a player whose frontier is exhausted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExpansionPlan {
    Ready(ExpansionCommand),
    Skipped(&'static str),
}

/// Plans expansion from the current component containing `source_seed`.
///
/// The focus is selected from the component's traversable neutral perimeter,
/// biased toward `objective_cell`. This is stronger than selecting an arbitrary
/// globally-neutral focus: it guarantees that the worker snapshot satisfies the
/// reducer's focus and eligible-front preconditions. The reducer remains the
/// authority because another command or simulation tick may change ownership
/// after this snapshot.
pub fn plan_expansion_for_player(
    cells: &[MapCell],
    player_id: u16,
    source_seed: u32,
    objective_cell: u32,
    max_elevation_step: u8,
) -> ExpansionPlan {
    let by_id = cells
        .iter()
        .map(|cell| (cell.cell_id, *cell))
        .collect::<BTreeMap<_, _>>();
    let by_coord = cells
        .iter()
        .map(|cell| (Axial::new(cell.q, cell.r), *cell))
        .collect::<BTreeMap<_, _>>();
    let Some(seed) = by_id.get(&source_seed).copied() else {
        return ExpansionPlan::Skipped("expansion source seed is missing");
    };
    if seed.owner != player_id || !seed.passable {
        return ExpansionPlan::Skipped("expansion source seed is not owned passable ground");
    }

    let seed_coord = Axial::new(seed.q, seed.r);
    let mut component = BTreeSet::from([seed_coord]);
    let mut pending = VecDeque::from([seed_coord]);
    while let Some(current) = pending.pop_front() {
        let current_cell = by_coord[&current];
        for neighbor in current.neighbors() {
            let Some(neighbor_cell) = by_coord.get(&neighbor) else {
                continue;
            };
            if neighbor_cell.owner != player_id || !neighbor_cell.passable {
                continue;
            }
            if current_cell.elevation.abs_diff(neighbor_cell.elevation)
                > u16::from(max_elevation_step)
            {
                continue;
            }
            if component.insert(neighbor) {
                pending.push_back(neighbor);
            }
        }
    }

    let objective = by_id
        .get(&objective_cell)
        .map_or(seed_coord, |cell| Axial::new(cell.q, cell.r));
    let mut candidates = BTreeMap::<u32, Axial>::new();
    for source in component {
        let source_cell = by_coord[&source];
        for target in source.neighbors() {
            let Some(target_cell) = by_coord.get(&target) else {
                continue;
            };
            if target_cell.owner != 0 || !target_cell.passable || !target_cell.capturable {
                continue;
            }
            if source_cell.elevation.abs_diff(target_cell.elevation) > u16::from(max_elevation_step)
            {
                continue;
            }
            candidates.insert(target_cell.cell_id, target);
        }
    }
    let Some((focus, _)) = candidates
        .into_iter()
        .min_by_key(|(cell_id, coordinate)| (coordinate.distance(objective), *cell_id))
    else {
        return ExpansionPlan::Skipped("owned component has no neutral traversable perimeter");
    };

    ExpansionPlan::Ready(ExpansionCommand { source_seed, focus })
}

/// A valid worker snapshot can become stale while concurrent reducer requests
/// and scheduled simulation ticks are serialized. These two reducer messages
/// mean the player's topology changed and the workload may be replanned safely.
pub fn is_retryable_topology_rejection(message: &str) -> bool {
    message.contains("cluster expand focus must be unclaimed passable ground")
        || message.contains("the selected regions have no adjacent neutral passable ground")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(cell_id: u32, q: i32, r: i32, owner: u16) -> MapCell {
        MapCell {
            cell_id,
            q,
            r,
            owner,
            elevation: 0,
            passable: true,
            capturable: true,
            infantry: 10,
            military_capacity: 100,
        }
    }

    #[test]
    fn focus_is_on_current_component_perimeter_and_biased_to_objective() {
        let cells = [
            cell(10, 0, 0, 1),
            cell(11, 1, 0, 1),
            cell(20, -1, 0, 0),
            cell(21, 2, 0, 0),
            cell(30, 5, 0, 2),
        ];
        assert_eq!(
            plan_expansion_for_player(&cells, 1, 10, 30, 1),
            ExpansionPlan::Ready(ExpansionCommand {
                source_seed: 10,
                focus: 21,
            })
        );
    }

    #[test]
    fn neutral_ground_beside_a_different_component_is_not_selected() {
        let cells = [
            cell(10, 0, 0, 1),
            cell(11, 10, 0, 1),
            cell(20, 11, 0, 0),
            cell(30, 12, 0, 2),
        ];
        assert_eq!(
            plan_expansion_for_player(&cells, 1, 10, 30, 1),
            ExpansionPlan::Skipped("owned component has no neutral traversable perimeter")
        );
    }

    #[test]
    fn elevation_disconnected_neutral_perimeter_is_skipped() {
        let mut target = cell(20, 1, 0, 0);
        target.elevation = 2;
        let cells = [cell(10, 0, 0, 1), target];
        assert!(matches!(
            plan_expansion_for_player(&cells, 1, 10, 20, 1),
            ExpansionPlan::Skipped(_)
        ));
    }

    #[test]
    fn only_topology_staleness_is_retryable() {
        assert!(is_retryable_topology_rejection(
            "required expansion command was rejected: cluster expand focus must be unclaimed passable ground"
        ));
        assert!(is_retryable_topology_rejection(
            "the selected regions have no adjacent neutral passable ground"
        ));
        assert!(!is_retryable_topology_rejection(
            "expand source cell is not owned passable ground"
        ));
    }
}
