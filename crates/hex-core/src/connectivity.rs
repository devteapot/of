use std::collections::{BTreeSet, VecDeque};

use crate::{
    coord::Axial,
    map::{HexMap, MovementConfig, PlayerId, ground_traversal},
};

/// Splits a selected set into deterministic ground-traversable components.
///
/// Missing cells and impassable cells are omitted. Coordinates within each
/// component and the component list itself are returned in ascending order.
pub fn connected_components<I>(
    map: &HexMap,
    selection: I,
    movement: &MovementConfig,
) -> Vec<Vec<Axial>>
where
    I: IntoIterator<Item = Axial>,
{
    let eligible: BTreeSet<_> = selection
        .into_iter()
        .filter(|coordinate| {
            map.get(*coordinate)
                .is_some_and(|cell| cell.terrain.ground_passable())
        })
        .collect();
    let mut remaining = eligible.clone();
    let mut result = Vec::new();

    while let Some(seed) = remaining.pop_first() {
        let mut queue = VecDeque::from([seed]);
        let mut component = vec![seed];

        while let Some(current) = queue.pop_front() {
            let from = map
                .get(current)
                .expect("eligible coordinates were checked above");
            let mut neighbors = current.neighbors();
            neighbors.sort_unstable();
            for neighbor in neighbors {
                if !remaining.contains(&neighbor) || !eligible.contains(&neighbor) {
                    continue;
                }
                let to = map
                    .get(neighbor)
                    .expect("eligible coordinates were checked above");
                if ground_traversal(from, to, movement).is_none() {
                    continue;
                }
                remaining.remove(&neighbor);
                queue.push_back(neighbor);
                component.push(neighbor);
            }
        }

        component.sort_unstable();
        result.push(component);
    }

    result.sort_unstable_by_key(|component| component[0]);
    result
}

pub fn owned_components(
    map: &HexMap,
    owner: PlayerId,
    movement: &MovementConfig,
) -> Vec<Vec<Axial>> {
    connected_components(
        map,
        map.cells()
            .filter(|cell| cell.owner == Some(owner))
            .map(|cell| cell.coordinate),
        movement,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::Cell;

    #[test]
    fn lost_corridor_creates_independent_owned_components() {
        let mut map = HexMap::new();
        for q in 0..5 {
            map.insert(Cell::ground(Axial::new(q, 0), 0, Some(1), 100));
        }
        assert_eq!(
            owned_components(&map, 1, &MovementConfig::default()).len(),
            1
        );

        map.get_mut(Axial::new(2, 0)).unwrap().owner = Some(2);
        assert_eq!(
            owned_components(&map, 1, &MovementConfig::default()),
            vec![
                vec![Axial::new(0, 0), Axial::new(1, 0)],
                vec![Axial::new(3, 0), Axial::new(4, 0)]
            ]
        );
    }

    #[test]
    fn cliffs_split_a_selected_region() {
        let mut map = HexMap::new();
        map.insert(Cell::ground(Axial::new(0, 0), 0, Some(1), 100));
        map.insert(Cell::ground(Axial::new(1, 0), 2, Some(1), 100));
        map.insert(Cell::ground(Axial::new(2, 0), 0, Some(1), 100));
        let components = connected_components(&map, map.coordinates(), &MovementConfig::default());
        assert_eq!(components.len(), 3);
    }

    #[test]
    fn output_is_sorted_and_ignores_missing_or_water_cells() {
        let mut map = HexMap::new();
        map.insert(Cell::ground(Axial::new(1, 0), 0, Some(1), 100));
        map.insert(Cell::ground(Axial::new(0, 0), 0, Some(1), 100));
        map.insert(Cell::water(Axial::new(2, 0), 0));
        let components = connected_components(
            &map,
            [
                Axial::new(99, 99),
                Axial::new(2, 0),
                Axial::new(1, 0),
                Axial::new(0, 0),
            ],
            &MovementConfig::default(),
        );
        assert_eq!(components, vec![vec![Axial::new(0, 0), Axial::new(1, 0)]]);
    }
}
