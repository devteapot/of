//! Deterministic front-rebalance target selection for the rebalance phase.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use hex_core::{Axial, StrategicExterior, strategic_front_index_for_seed, strategic_fronts};

/// One map cell used for owned-component and strategic-front derivation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MapCell {
    pub cell_id: u32,
    pub q: i32,
    pub r: i32,
    pub owner: u16,
    pub elevation: i16,
    pub passable: bool,
    pub capturable: bool,
}

/// Exact reducer payload for one seat's front-rebalance command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrontRebalanceCommand {
    /// Complete owned traversable component cell IDs (sorted ascending).
    pub component_cells: Vec<u32>,
    pub source_front_seed: u32,
    pub target_front_seed: u32,
}

/// Per-player plan: issue a command or skip when the topology is unusable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrontRebalancePlan {
    Ready(FrontRebalanceCommand),
    Skipped(&'static str),
}

/// Derive one complete owned component and two distinct strategic front seeds.
///
/// Component connectivity matches the authoritative elevation-step rules used by
/// cluster selection. Strategic fronts use [`hex_core::strategic_fronts`]. When
/// the player lacks two usable fronts, returns [`FrontRebalancePlan::Skipped`]
/// instead of producing an invalid command payload.
pub fn plan_front_rebalance_for_player(
    cells: &[MapCell],
    player_id: u16,
    max_elevation_step: u8,
) -> FrontRebalancePlan {
    let components = owned_components(cells, player_id, max_elevation_step);
    let Some(component) = select_component(&components) else {
        return FrontRebalancePlan::Skipped("no owned traversable component");
    };

    let by_coord = cells
        .iter()
        .map(|cell| (Axial::new(cell.q, cell.r), *cell))
        .collect::<BTreeMap<_, _>>();
    let component_coords = component
        .iter()
        .filter_map(|cell_id| {
            cells
                .iter()
                .find(|cell| cell.cell_id == *cell_id)
                .map(|cell| Axial::new(cell.q, cell.r))
        })
        .collect::<BTreeSet<_>>();
    if component_coords.len() != component.len() {
        return FrontRebalancePlan::Skipped("component cells missing terrain coordinates");
    }

    let cell_ids_by_coord = component_coords
        .iter()
        .filter_map(|coord| by_coord.get(coord).map(|cell| (*coord, cell.cell_id)))
        .collect::<BTreeMap<_, _>>();

    let Ok(fronts) = strategic_fronts(component_coords.iter().copied(), |source, target| {
        classify_exterior(source, target, player_id, max_elevation_step, &by_coord)
    }) else {
        return FrontRebalancePlan::Skipped("component has no boundary");
    };
    if fronts.len() < 2 {
        return FrontRebalancePlan::Skipped("fewer than two strategic fronts");
    }

    let mut resolvable = Vec::new();
    for (index, _) in fronts.iter().enumerate() {
        if let Some(seed) = seed_for_front(&fronts, index, &cell_ids_by_coord) {
            resolvable.push((index, seed));
        }
    }
    if resolvable.len() < 2 {
        return FrontRebalancePlan::Skipped("could not resolve two distinct front seeds");
    }

    let (_, source_front_seed) = resolvable[0];
    let (_, target_front_seed) = resolvable[1];
    if source_front_seed == target_front_seed {
        return FrontRebalancePlan::Skipped("front seeds collapsed to the same cell");
    }

    let mut component_cells = component.iter().copied().collect::<Vec<_>>();
    component_cells.sort_unstable();
    FrontRebalancePlan::Ready(FrontRebalanceCommand {
        component_cells,
        source_front_seed,
        target_front_seed,
    })
}

fn owned_components(
    cells: &[MapCell],
    player_id: u16,
    max_elevation_step: u8,
) -> Vec<BTreeSet<u32>> {
    let owned = cells
        .iter()
        .filter(|cell| cell.owner == player_id && cell.passable)
        .map(|cell| (Axial::new(cell.q, cell.r), (cell.cell_id, cell.elevation)))
        .collect::<BTreeMap<_, _>>();
    let mut remaining = owned.keys().copied().collect::<BTreeSet<_>>();
    let mut result = Vec::new();
    while let Some(seed) = remaining.pop_first() {
        let mut pending = VecDeque::from([seed]);
        let mut component = BTreeSet::new();
        while let Some(current) = pending.pop_front() {
            component.insert(owned[&current].0);
            let current_elevation = owned[&current].1;
            for neighbor in current.neighbors() {
                let Some((_, neighbor_elevation)) = owned.get(&neighbor) else {
                    continue;
                };
                let elevation_delta =
                    (i32::from(current_elevation) - i32::from(*neighbor_elevation)).unsigned_abs();
                if elevation_delta <= u32::from(max_elevation_step) && remaining.remove(&neighbor) {
                    pending.push_back(neighbor);
                }
            }
        }
        result.push(component);
    }
    result.sort_by_key(|component| {
        (
            std::cmp::Reverse(component.len()),
            component.iter().next().copied().unwrap_or(u32::MAX),
        )
    });
    result
}

fn select_component(components: &[BTreeSet<u32>]) -> Option<&BTreeSet<u32>> {
    components.first()
}

fn classify_exterior(
    source: Axial,
    target: Axial,
    player_id: u16,
    max_elevation_step: u8,
    by_coord: &BTreeMap<Axial, MapCell>,
) -> StrategicExterior {
    let Some(source_cell) = by_coord.get(&source) else {
        return StrategicExterior::Ignored;
    };
    let Some(target_cell) = by_coord.get(&target) else {
        return StrategicExterior::Ignored;
    };
    if !target_cell.passable || !target_cell.capturable {
        return StrategicExterior::Ignored;
    }
    let elevation_delta =
        (i32::from(source_cell.elevation) - i32::from(target_cell.elevation)).unsigned_abs();
    if elevation_delta > u32::from(max_elevation_step) {
        return StrategicExterior::Ignored;
    }
    if target_cell.owner == player_id {
        return StrategicExterior::Ignored;
    }
    if target_cell.owner == 0 {
        StrategicExterior::Neutral
    } else {
        StrategicExterior::Opponent(u32::from(target_cell.owner))
    }
}

fn seed_for_front(
    fronts: &[hex_core::StrategicFront],
    index: usize,
    cell_ids_by_coord: &BTreeMap<Axial, u32>,
) -> Option<u32> {
    let mut candidates = fronts
        .get(index)?
        .source_cells()
        .into_iter()
        .filter_map(|coord| {
            cell_ids_by_coord
                .get(&coord)
                .map(|cell_id| (*cell_id, coord))
        })
        .collect::<Vec<_>>();
    candidates.sort_unstable_by_key(|(cell_id, _)| *cell_id);
    candidates.dedup_by_key(|(cell_id, _)| *cell_id);
    for (cell_id, coord) in candidates {
        if strategic_front_index_for_seed(fronts, coord) == Some(index) {
            return Some(cell_id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(
        cell_id: u32,
        q: i32,
        r: i32,
        owner: u16,
        elevation: i16,
        passable: bool,
        capturable: bool,
    ) -> MapCell {
        MapCell {
            cell_id,
            q,
            r,
            owner,
            elevation,
            passable,
            capturable,
        }
    }

    fn surround_line(cells: &mut Vec<MapCell>, owned: &[(u32, i32, i32)], next_id: &mut u32) {
        let owned_coords = owned
            .iter()
            .map(|(_, q, r)| Axial::new(*q, *r))
            .collect::<BTreeSet<_>>();
        for &(_, q, r) in owned {
            for neighbor in Axial::new(q, r).neighbors() {
                if owned_coords.contains(&neighbor) {
                    continue;
                }
                if cells
                    .iter()
                    .any(|cell| cell.q == neighbor.q && cell.r == neighbor.r)
                {
                    continue;
                }
                cells.push(cell(*next_id, neighbor.q, neighbor.r, 0, 0, true, true));
                *next_id += 1;
            }
        }
    }

    #[test]
    fn single_owned_cell_cannot_supply_two_distinct_seeds() {
        let mut cells = vec![cell(1, 0, 0, 1, 0, true, true)];
        let mut next_id = 2;
        surround_line(&mut cells, &[(1, 0, 0)], &mut next_id);
        // One hostile neighbor so strategic fronts split, but only one owned cell.
        if let Some(neighbor) = cells.iter_mut().find(|cell| cell.cell_id == 2) {
            neighbor.owner = 2;
        }
        let plan = plan_front_rebalance_for_player(&cells, 1, 1);
        assert!(
            matches!(
                plan,
                FrontRebalancePlan::Skipped(
                    "front seeds collapsed to the same cell"
                        | "could not resolve two distinct front seeds"
                )
            ),
            "single owned cell must skip, got {plan:?}"
        );
    }

    #[test]
    fn multi_cell_blob_picks_two_front_seeds_deterministically() {
        // Owned strip with a single enemy contact on the west end. Hostile vs
        // neutral arcs expose different source cells, so seeds differ.
        let owned = [(10_u32, 0_i32, 0_i32), (11, 1, 0), (12, 2, 0)];
        let mut cells = owned
            .iter()
            .map(|(id, q, r)| cell(*id, *q, *r, 1, 0, true, true))
            .collect::<Vec<_>>();
        cells.push(cell(20, -1, 0, 2, 0, true, true));
        let mut next_id = 30;
        surround_line(&mut cells, &owned, &mut next_id);

        let first = plan_front_rebalance_for_player(&cells, 1, 1);
        let second = plan_front_rebalance_for_player(&cells, 1, 1);
        assert_eq!(first, second);
        let FrontRebalancePlan::Ready(command) = first else {
            panic!("expected ready plan, got {first:?}");
        };
        assert_eq!(command.component_cells, vec![10, 11, 12]);
        assert_ne!(command.source_front_seed, command.target_front_seed);
        assert!(command.component_cells.contains(&command.source_front_seed));
        assert!(command.component_cells.contains(&command.target_front_seed));
    }

    #[test]
    fn player_without_territory_is_skipped() {
        let cells = [cell(1, 0, 0, 2, 0, true, true)];
        assert_eq!(
            plan_front_rebalance_for_player(&cells, 1, 1),
            FrontRebalancePlan::Skipped("no owned traversable component")
        );
    }

    #[test]
    fn prefers_largest_component_deterministically() {
        let small = [(1_u32, 10_i32, 10_i32)];
        let large = [(100_u32, 0_i32, 0_i32), (101, 1, 0), (102, 2, 0)];
        let mut cells = Vec::new();
        for (id, q, r) in small.into_iter().chain(large) {
            cells.push(cell(id, q, r, 1, 0, true, true));
        }
        cells.push(cell(3, 9, 10, 2, 0, true, true));
        cells.push(cell(200, -1, 0, 2, 0, true, true));
        let mut next_id = 300;
        surround_line(&mut cells, &small, &mut next_id);
        surround_line(&mut cells, &large, &mut next_id);

        let FrontRebalancePlan::Ready(command) = plan_front_rebalance_for_player(&cells, 1, 1)
        else {
            panic!("expected ready plan");
        };
        assert_eq!(command.component_cells, vec![100, 101, 102]);
    }
}
