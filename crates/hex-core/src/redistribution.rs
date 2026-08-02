use std::collections::{BTreeMap, BTreeSet};

use crate::{
    coord::Axial,
    map::{HexMap, PlayerId, Strength},
};

pub const BALANCE_WEIGHT: u32 = 10_000;

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DistributionPreset {
    /// Equal target occupancy ratio (`strength / capacity`).
    Balance,
    /// Density increases along an axial-space orientation vector.
    FrontLoad {
        direction: Axial,
        rear_weight: u32,
        front_weight: u32,
    },
}

impl DistributionPreset {
    pub const fn front_load(direction: Axial) -> Self {
        Self::FrontLoad {
            direction,
            rear_weight: 5_000,
            front_weight: 15_000,
        }
    }
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DistributionError {
    EmptySelection,
    ZeroDirection,
    InvalidWeights,
    UnknownCell(Axial),
    NotOwned { coordinate: Axial, owner: PlayerId },
    ArithmeticOverflow,
}

/// Produces stable relative density weights for a preview heatmap.
///
/// Front-load projection uses the Euclidean dot product of the two axial basis
/// vectors, multiplied by two to remain integer-only. Normalizing by the
/// selection's minimum and maximum makes the result translation invariant.
pub fn distribution_weights<I>(
    selection: I,
    preset: DistributionPreset,
) -> Result<BTreeMap<Axial, u32>, DistributionError>
where
    I: IntoIterator<Item = Axial>,
{
    let coordinates: BTreeSet<_> = selection.into_iter().collect();
    if coordinates.is_empty() {
        return Err(DistributionError::EmptySelection);
    }

    match preset {
        DistributionPreset::Balance => Ok(coordinates
            .into_iter()
            .map(|coordinate| (coordinate, BALANCE_WEIGHT))
            .collect()),
        DistributionPreset::FrontLoad {
            direction,
            rear_weight,
            front_weight,
        } => {
            if direction == Axial::ZERO {
                return Err(DistributionError::ZeroDirection);
            }
            if rear_weight == 0 || front_weight < rear_weight {
                return Err(DistributionError::InvalidWeights);
            }

            let projections: BTreeMap<_, _> = coordinates
                .into_iter()
                .map(|coordinate| (coordinate, axial_dot_twice(coordinate, direction)))
                .collect();
            let minimum = *projections.values().min().expect("selection is not empty");
            let maximum = *projections.values().max().expect("selection is not empty");
            let span = maximum - minimum;
            let weight_span = u128::from(front_weight - rear_weight);

            projections
                .into_iter()
                .map(|(coordinate, projection)| {
                    let weight = if span == 0 {
                        u128::from(rear_weight) + weight_span / 2
                    } else {
                        u128::from(rear_weight)
                            + weight_span * (projection - minimum) as u128 / span as u128
                    };
                    Ok((coordinate, weight as u32))
                })
                .collect()
        }
    }
}

fn axial_dot_twice(coordinate: Axial, direction: Axial) -> i128 {
    let q = i128::from(coordinate.q);
    let r = i128::from(coordinate.r);
    let dq = i128::from(direction.q);
    let dr = i128::from(direction.r);
    2 * q * dq + q * dr + r * dq + 2 * r * dr
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetDistribution {
    pub weights: BTreeMap<Axial, u32>,
    pub targets: BTreeMap<Axial, Strength>,
    pub assigned: Strength,
    pub unassigned: Strength,
}

/// Converts density weights into capacity-safe integer strength targets.
///
/// The result assigns `min(total_strength, selected capacity)` exactly. Cells
/// that would exceed capacity are saturated first; remaining strength is
/// apportioned by largest remainder with coordinate tie-breaking.
pub fn redistribution_targets<I>(
    map: &HexMap,
    owner: PlayerId,
    selection: I,
    total_strength: Strength,
    preset: DistributionPreset,
) -> Result<TargetDistribution, DistributionError>
where
    I: IntoIterator<Item = Axial>,
{
    let weights = distribution_weights(selection, preset)?;
    let mut capacities = BTreeMap::new();
    let mut capacity_total = 0_u64;
    for coordinate in weights.keys().copied() {
        let cell = map
            .get(coordinate)
            .ok_or(DistributionError::UnknownCell(coordinate))?;
        if cell.owner != Some(owner) {
            return Err(DistributionError::NotOwned { coordinate, owner });
        }
        capacity_total = capacity_total
            .checked_add(cell.military_capacity)
            .ok_or(DistributionError::ArithmeticOverflow)?;
        capacities.insert(coordinate, cell.military_capacity);
    }

    let goal = total_strength.min(capacity_total);
    let mut remaining = goal;
    let mut targets: BTreeMap<_, _> = weights.keys().map(|&coordinate| (coordinate, 0)).collect();
    let mut active: BTreeSet<_> = capacities
        .iter()
        .filter_map(|(&coordinate, &capacity)| (capacity > 0).then_some(coordinate))
        .collect();

    while remaining > 0 && !active.is_empty() {
        let total_score: u128 = active
            .iter()
            .map(|coordinate| u128::from(capacities[coordinate]) * u128::from(weights[coordinate]))
            .sum();
        if total_score == 0 {
            break;
        }

        let saturated: Vec<_> = active
            .iter()
            .copied()
            .filter(|coordinate| {
                let capacity = capacities[coordinate];
                let score = u128::from(capacity) * u128::from(weights[coordinate]);
                u128::from(remaining) * score > u128::from(capacity) * total_score
            })
            .collect();

        if !saturated.is_empty() {
            for coordinate in saturated {
                let capacity = capacities[&coordinate];
                targets.insert(coordinate, capacity);
                remaining -= capacity;
                active.remove(&coordinate);
            }
            continue;
        }

        let mut floor_sum = 0_u64;
        let mut remainders = Vec::with_capacity(active.len());
        for &coordinate in &active {
            let score = u128::from(capacities[&coordinate]) * u128::from(weights[&coordinate]);
            let numerator = u128::from(remaining) * score;
            let floor = (numerator / total_score) as u64;
            let remainder = numerator % total_score;
            targets.insert(coordinate, floor);
            floor_sum += floor;
            remainders.push((remainder, coordinate));
        }

        let leftover = remaining - floor_sum;
        remainders.sort_unstable_by(
            |(left_remainder, left_coordinate), (right_remainder, right_coordinate)| {
                right_remainder
                    .cmp(left_remainder)
                    .then_with(|| left_coordinate.cmp(right_coordinate))
            },
        );
        for &(_, coordinate) in remainders.iter().take(leftover as usize) {
            *targets
                .get_mut(&coordinate)
                .expect("all active targets were initialized") += 1;
        }
        remaining = 0;
    }

    let assigned = goal - remaining;
    Ok(TargetDistribution {
        weights,
        targets,
        assigned,
        unassigned: total_strength - assigned,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::Cell;

    fn line(capacities: &[u64]) -> HexMap {
        let mut map = HexMap::new();
        for (q, &capacity) in capacities.iter().enumerate() {
            map.insert(Cell::ground(Axial::new(q as i32, 0), 0, Some(1), capacity));
        }
        map
    }

    #[test]
    fn balance_equalizes_occupancy_ratio_not_raw_strength() {
        let map = line(&[100, 200]);
        let result =
            redistribution_targets(&map, 1, map.coordinates(), 150, DistributionPreset::Balance)
                .unwrap();
        assert_eq!(result.targets[&Axial::new(0, 0)], 50);
        assert_eq!(result.targets[&Axial::new(1, 0)], 100);
        assert_eq!(result.assigned, 150);
    }

    #[test]
    fn orientation_selects_which_side_is_the_front() {
        let map = line(&[100, 100, 100]);
        let east = redistribution_targets(
            &map,
            1,
            map.coordinates(),
            150,
            DistributionPreset::front_load(Axial::new(1, 0)),
        )
        .unwrap();
        assert_eq!(
            east.targets.values().copied().collect::<Vec<_>>(),
            vec![25, 50, 75]
        );

        let west = redistribution_targets(
            &map,
            1,
            map.coordinates(),
            150,
            DistributionPreset::front_load(Axial::new(-1, 0)),
        )
        .unwrap();
        assert_eq!(
            west.targets.values().copied().collect::<Vec<_>>(),
            vec![75, 50, 25]
        );
    }

    #[test]
    fn weights_are_translation_invariant() {
        let original = distribution_weights(
            [Axial::new(0, 0), Axial::new(1, 0), Axial::new(2, 0)],
            DistributionPreset::front_load(Axial::new(1, 0)),
        )
        .unwrap();
        let translated = distribution_weights(
            [
                Axial::new(50, -20),
                Axial::new(51, -20),
                Axial::new(52, -20),
            ],
            DistributionPreset::front_load(Axial::new(1, 0)),
        )
        .unwrap();
        assert_eq!(
            original.values().copied().collect::<Vec<_>>(),
            translated.values().copied().collect::<Vec<_>>()
        );
    }

    #[test]
    fn targets_conserve_strength_and_never_exceed_capacity() {
        let map = line(&[3, 7, 11, 19]);
        for total in 0..=50 {
            for preset in [
                DistributionPreset::Balance,
                DistributionPreset::front_load(Axial::new(1, -1)),
            ] {
                let result =
                    redistribution_targets(&map, 1, map.coordinates(), total, preset).unwrap();
                assert_eq!(result.assigned + result.unassigned, total);
                assert_eq!(result.targets.values().sum::<u64>(), result.assigned);
                for (coordinate, target) in result.targets {
                    assert!(target <= map.get(coordinate).unwrap().military_capacity);
                }
            }
        }
    }

    #[test]
    fn selection_and_direction_errors_are_explicit() {
        assert_eq!(
            distribution_weights([], DistributionPreset::Balance),
            Err(DistributionError::EmptySelection)
        );
        assert_eq!(
            distribution_weights([Axial::ZERO], DistributionPreset::front_load(Axial::ZERO)),
            Err(DistributionError::ZeroDirection)
        );
    }
}
