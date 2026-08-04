use std::collections::{BTreeMap, BTreeSet};

use crate::{
    coord::Axial,
    map::{Cell, HexMap, MovementConfig, ground_traversal},
};

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Path {
    /// Coordinates from start through goal, inclusive.
    pub cells: Vec<Axial>,
    pub total_cost: u64,
}

/// Deterministic A* routing over ground-traversable cells.
///
/// `can_enter` expresses order-specific policy such as friendly ownership. The
/// hex-distance heuristic is scaled by the cheapest configured edge cost, so
/// it is admissible and consistent. The ordered frontier and coordinate
/// tie-break make the same map and predicate produce the same path regardless
/// of map insertion order.
pub fn shortest_path<F>(
    map: &HexMap,
    start: Axial,
    goal: Axial,
    movement: &MovementConfig,
    can_enter: F,
) -> Option<Path>
where
    F: Fn(&Cell) -> bool,
{
    let start_cell = map.get(start)?;
    let goal_cell = map.get(goal)?;
    if !can_enter(start_cell) || !can_enter(goal_cell) {
        return None;
    }
    if start == goal {
        return Some(Path {
            cells: vec![start],
            total_cost: 0,
        });
    }

    let minimum_edge_cost = u64::from(
        movement
            .level_cost
            .min(movement.uphill_cost)
            .min(movement.downhill_cost),
    );
    let heuristic = |coordinate: Axial| minimum_edge_cost.saturating_mul(coordinate.distance(goal));
    let mut frontier = BTreeSet::from([(heuristic(start), 0_u64, start)]);
    let mut distances = BTreeMap::from([(start, 0_u64)]);
    let mut previous = BTreeMap::<Axial, Axial>::new();
    let mut visited = BTreeSet::new();

    while let Some((_estimated_total, cost, current)) = frontier.pop_first() {
        if !visited.insert(current) {
            continue;
        }
        if current == goal {
            return reconstruct_path(start, goal, cost, &previous);
        }

        let from = map.get(current)?;
        let mut neighbors = current.neighbors();
        neighbors.sort_unstable();
        for neighbor in neighbors {
            if visited.contains(&neighbor) {
                continue;
            }
            let Some(to) = map.get(neighbor) else {
                continue;
            };
            if !can_enter(to) {
                continue;
            }
            let Some(traversal) = ground_traversal(from, to, movement) else {
                continue;
            };
            let Some(candidate) = cost.checked_add(u64::from(traversal.cost)) else {
                continue;
            };
            let best = distances.get(&neighbor).copied().unwrap_or(u64::MAX);
            if candidate < best {
                if best != u64::MAX {
                    frontier.remove(&(best.saturating_add(heuristic(neighbor)), best, neighbor));
                }
                distances.insert(neighbor, candidate);
                previous.insert(neighbor, current);
                frontier.insert((
                    candidate.saturating_add(heuristic(neighbor)),
                    candidate,
                    neighbor,
                ));
            }
        }
    }

    None
}

fn reconstruct_path(
    start: Axial,
    goal: Axial,
    total_cost: u64,
    previous: &BTreeMap<Axial, Axial>,
) -> Option<Path> {
    let mut cells = vec![goal];
    let mut current = goal;
    while current != start {
        current = *previous.get(&current)?;
        cells.push(current);
    }
    cells.reverse();
    Some(Path { cells, total_cost })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::Cell;

    fn insert_hexagon(map: &mut HexMap, reverse: bool) {
        let mut coordinates = vec![
            Axial::new(0, 0),
            Axial::new(1, 0),
            Axial::new(2, 0),
            Axial::new(0, 1),
            Axial::new(1, 1),
            Axial::new(2, 1),
        ];
        if reverse {
            coordinates.reverse();
        }
        for coordinate in coordinates {
            map.insert(Cell::ground(coordinate, 0, Some(7), 100));
        }
    }

    #[test]
    fn finds_the_lowest_cost_path_and_includes_both_ends() {
        let mut map = HexMap::new();
        insert_hexagon(&mut map, false);
        map.get_mut(Axial::new(1, 0)).unwrap().elevation = 1;
        let path = shortest_path(
            &map,
            Axial::new(0, 0),
            Axial::new(2, 0),
            &MovementConfig::default(),
            |_| true,
        )
        .unwrap();

        assert_eq!(path.cells.first(), Some(&Axial::new(0, 0)));
        assert_eq!(path.cells.last(), Some(&Axial::new(2, 0)));
        assert_eq!(path.total_cost, 25);
    }

    #[test]
    fn avoids_cliffs_and_disallowed_ownership() {
        let mut map = HexMap::new();
        insert_hexagon(&mut map, false);
        map.get_mut(Axial::new(1, 0)).unwrap().elevation = 2;
        map.get_mut(Axial::new(1, 1)).unwrap().owner = Some(9);
        let path = shortest_path(
            &map,
            Axial::new(0, 0),
            Axial::new(2, 0),
            &MovementConfig::default(),
            |cell| cell.owner == Some(7),
        );
        assert_eq!(path, None);
    }

    #[test]
    fn path_is_independent_of_map_insertion_order() {
        let mut forward = HexMap::new();
        let mut reverse = HexMap::new();
        insert_hexagon(&mut forward, false);
        insert_hexagon(&mut reverse, true);
        let route = |map: &HexMap| {
            shortest_path(
                map,
                Axial::new(0, 0),
                Axial::new(2, 1),
                &MovementConfig::default(),
                |_| true,
            )
        };
        assert_eq!(route(&forward), route(&reverse));
    }

    #[test]
    fn missing_and_filtered_endpoints_have_no_path() {
        let mut map = HexMap::new();
        map.insert(Cell::ground(Axial::ZERO, 0, Some(1), 100));
        assert_eq!(
            shortest_path(
                &map,
                Axial::ZERO,
                Axial::new(1, 0),
                &MovementConfig::default(),
                |_| true
            ),
            None
        );
        assert_eq!(
            shortest_path(
                &map,
                Axial::ZERO,
                Axial::ZERO,
                &MovementConfig::default(),
                |_| false
            ),
            None
        );
    }
}
