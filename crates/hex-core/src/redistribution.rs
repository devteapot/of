use std::collections::{BTreeMap, BTreeSet};

use crate::{
    coord::Axial,
    map::{HexMap, PlayerId, Strength},
};

pub const UNIFORM_ALLOCATION_WEIGHT: u32 = 10_000;

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DistributionError {
    EmptySelection,
    UnknownCell(Axial),
    NotOwned {
        coordinate: Axial,
        owner: PlayerId,
    },
    CurrentStrengthExceedsCapacity {
        coordinate: Axial,
        current: Strength,
        capacity: Strength,
    },
    InfeasibleFrozenStrength {
        frozen: Strength,
        goal: Strength,
    },
    ConstraintCellsMismatch,
    InvalidLowerBound {
        coordinate: Axial,
        lower_bound: Strength,
        current: Strength,
    },
    InsufficientTargetCapacity {
        unassigned: Strength,
    },
    ArithmeticOverflow,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetDistribution {
    pub weights: BTreeMap<Axial, u32>,
    pub targets: BTreeMap<Axial, Strength>,
    pub assigned: Strength,
    pub unassigned: Strength,
}

/// Dense counterpart to [`TargetDistribution`] for authoritative hot paths
/// that already keep component cells in a stable vector. Every output entry is
/// aligned with the corresponding input coordinate/capacity entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DenseTargetDistribution {
    pub weights: Vec<u32>,
    pub targets: Vec<Strength>,
    pub assigned: Strength,
    pub unassigned: Strength,
}

/// Applies precomputed dense weights with capacity-safe largest-remainder
/// apportionment. Used by explicit FrontRebalance placement.
pub fn redistribution_targets_dense_with_weights(
    coordinates: &[Axial],
    capacities: &[Strength],
    total_strength: Strength,
    weights: Vec<u32>,
) -> Result<DenseTargetDistribution, DistributionError> {
    if coordinates.is_empty() {
        return Err(DistributionError::EmptySelection);
    }
    if coordinates.len() != capacities.len() || coordinates.len() != weights.len() {
        return Err(DistributionError::ConstraintCellsMismatch);
    }
    let capacity_total = capacities.iter().try_fold(0_u64, |total, capacity| {
        total
            .checked_add(*capacity)
            .ok_or(DistributionError::ArithmeticOverflow)
    })?;
    let goal = total_strength.min(capacity_total);
    let mut remaining = goal;
    let mut targets = vec![0; coordinates.len()];
    let mut unsaturated = vec![true; coordinates.len()];
    let mut unsaturated_count = coordinates.len();

    while remaining > 0 && unsaturated_count > 0 {
        let total_score = (0..coordinates.len()).try_fold(0_u128, |total, index| {
            if !unsaturated[index] {
                return Ok(total);
            }
            total
                .checked_add(u128::from(capacities[index]) * u128::from(weights[index]))
                .ok_or(DistributionError::ArithmeticOverflow)
        })?;
        if total_score == 0 {
            break;
        }

        let saturated = (0..coordinates.len())
            .filter(|&index| {
                if !unsaturated[index] {
                    return false;
                }
                let capacity = capacities[index];
                let score = u128::from(capacity) * u128::from(weights[index]);
                u128::from(remaining)
                    .checked_mul(score)
                    .is_some_and(|numerator| numerator > u128::from(capacity) * total_score)
            })
            .collect::<Vec<_>>();
        if !saturated.is_empty() {
            for index in saturated {
                targets[index] = capacities[index];
                remaining = remaining
                    .checked_sub(capacities[index])
                    .ok_or(DistributionError::ArithmeticOverflow)?;
                unsaturated[index] = false;
                unsaturated_count -= 1;
            }
            continue;
        }

        let mut floor_sum = 0_u64;
        let mut remainders = Vec::with_capacity(unsaturated_count);
        for index in 0..coordinates.len() {
            if !unsaturated[index] {
                continue;
            }
            let score = u128::from(capacities[index]) * u128::from(weights[index]);
            let numerator = u128::from(remaining)
                .checked_mul(score)
                .ok_or(DistributionError::ArithmeticOverflow)?;
            let floor = Strength::try_from(numerator / total_score)
                .map_err(|_| DistributionError::ArithmeticOverflow)?;
            targets[index] = floor;
            floor_sum = floor_sum
                .checked_add(floor)
                .ok_or(DistributionError::ArithmeticOverflow)?;
            remainders.push((numerator % total_score, coordinates[index], index));
        }
        let leftover = remaining
            .checked_sub(floor_sum)
            .ok_or(DistributionError::ArithmeticOverflow)?;
        remainders.sort_unstable_by(
            |(left_remainder, left_coordinate, _), (right_remainder, right_coordinate, _)| {
                right_remainder
                    .cmp(left_remainder)
                    .then_with(|| left_coordinate.cmp(right_coordinate))
            },
        );
        for &(_, _, index) in remainders.iter().take(leftover as usize) {
            targets[index] += 1;
        }
        remaining = 0;
    }

    let assigned = targets.iter().try_fold(0_u64, |total, target| {
        total
            .checked_add(*target)
            .ok_or(DistributionError::ArithmeticOverflow)
    })?;
    Ok(DenseTargetDistribution {
        weights,
        targets,
        assigned,
        unassigned: total_strength - assigned,
    })
}

/// Apportions strength into preferred cells first, then uses zero-weight cells
/// as deterministic overflow storage.
///
/// This is the best-effort shape-drawing primitive. Positive weights identify
/// the drawn footprint, while zero-weight cells are source cells that may keep
/// strength when the footprint is too small. Frozen lower bounds are always
/// honored. If the preferred cells can hold all non-frozen strength, fallback
/// cells receive only their lower bounds; otherwise every preferred cell is
/// saturated before overflow is balanced across the fallback cells' current
/// affected strength. Consequently a fallback source can only retain or lose
/// strength, never gain it from another excluded source.
pub fn redistribution_targets_with_fallback_constraints(
    map: &HexMap,
    owner: PlayerId,
    weights: BTreeMap<Axial, u32>,
    lower_bounds: BTreeMap<Axial, Strength>,
    total_strength: Strength,
) -> Result<TargetDistribution, DistributionError> {
    let capacities = validated_constraint_capacities(map, owner, &weights, &lower_bounds)?;
    let preferred = weights
        .iter()
        .filter_map(|(&coordinate, &weight)| (weight > 0).then_some(coordinate))
        .collect::<BTreeSet<_>>();
    let fallback = weights
        .iter()
        .filter_map(|(&coordinate, &weight)| (weight == 0).then_some(coordinate))
        .collect::<BTreeSet<_>>();

    let fallback_floor = fallback.iter().try_fold(0_u64, |total, coordinate| {
        total
            .checked_add(lower_bounds[coordinate])
            .ok_or(DistributionError::ArithmeticOverflow)
    })?;
    let frozen_total = lower_bounds.values().try_fold(0_u64, |total, frozen| {
        total
            .checked_add(*frozen)
            .ok_or(DistributionError::ArithmeticOverflow)
    })?;
    let preferred_capacity = preferred.iter().try_fold(0_u64, |total, coordinate| {
        total
            .checked_add(capacities[coordinate])
            .ok_or(DistributionError::ArithmeticOverflow)
    })?;
    let preferred_goal = total_strength
        .checked_sub(fallback_floor)
        .ok_or(DistributionError::InfeasibleFrozenStrength {
            frozen: frozen_total,
            goal: total_strength,
        })?
        .min(preferred_capacity);

    let preferred_distribution = constrained_targets(
        preferred
            .iter()
            .map(|coordinate| (*coordinate, weights[coordinate]))
            .collect(),
        preferred
            .iter()
            .map(|coordinate| (*coordinate, capacities[coordinate]))
            .collect(),
        preferred
            .iter()
            .map(|coordinate| (*coordinate, lower_bounds[coordinate]))
            .collect(),
        preferred_goal,
    )?;
    let fallback_goal = total_strength
        .checked_sub(preferred_distribution.assigned)
        .ok_or(DistributionError::ArithmeticOverflow)?;
    let fallback_distribution = constrained_targets(
        fallback
            .iter()
            .map(|coordinate| (*coordinate, UNIFORM_ALLOCATION_WEIGHT))
            .collect(),
        fallback
            .iter()
            .map(|coordinate| {
                let current = map
                    .get(*coordinate)
                    .expect("constraint validation resolved every coordinate")
                    .force();
                (*coordinate, capacities[coordinate].min(current))
            })
            .collect(),
        fallback
            .iter()
            .map(|coordinate| (*coordinate, lower_bounds[coordinate]))
            .collect(),
        fallback_goal,
    )?;

    let assigned = preferred_distribution
        .assigned
        .checked_add(fallback_distribution.assigned)
        .ok_or(DistributionError::ArithmeticOverflow)?;
    let unassigned = total_strength
        .checked_sub(assigned)
        .ok_or(DistributionError::ArithmeticOverflow)?;
    if unassigned > 0 {
        return Err(DistributionError::InsufficientTargetCapacity { unassigned });
    }
    let mut targets = preferred_distribution.targets;
    targets.extend(fallback_distribution.targets);
    Ok(TargetDistribution {
        weights,
        targets,
        assigned,
        unassigned,
    })
}

fn validated_constraint_capacities(
    map: &HexMap,
    owner: PlayerId,
    weights: &BTreeMap<Axial, u32>,
    lower_bounds: &BTreeMap<Axial, Strength>,
) -> Result<BTreeMap<Axial, Strength>, DistributionError> {
    if weights.is_empty() {
        return Err(DistributionError::EmptySelection);
    }
    if weights.keys().ne(lower_bounds.keys()) {
        return Err(DistributionError::ConstraintCellsMismatch);
    }
    let mut capacities = BTreeMap::new();
    for coordinate in weights.keys().copied() {
        let cell = map
            .get(coordinate)
            .ok_or(DistributionError::UnknownCell(coordinate))?;
        if cell.owner != Some(owner) {
            return Err(DistributionError::NotOwned { coordinate, owner });
        }
        let current = cell.force();
        if current > cell.military_capacity {
            return Err(DistributionError::CurrentStrengthExceedsCapacity {
                coordinate,
                current,
                capacity: cell.military_capacity,
            });
        }
        let lower_bound = lower_bounds[&coordinate];
        if lower_bound > current {
            return Err(DistributionError::InvalidLowerBound {
                coordinate,
                lower_bound,
                current,
            });
        }
        capacities.insert(coordinate, cell.military_capacity);
    }
    Ok(capacities)
}

fn constrained_targets(
    weights: BTreeMap<Axial, u32>,
    capacities: BTreeMap<Axial, Strength>,
    lower_bounds: BTreeMap<Axial, Strength>,
    total_strength: Strength,
) -> Result<TargetDistribution, DistributionError> {
    let capacity_total = capacities.values().try_fold(0_u64, |total, capacity| {
        total
            .checked_add(*capacity)
            .ok_or(DistributionError::ArithmeticOverflow)
    })?;
    let frozen_total = lower_bounds.values().try_fold(0_u64, |total, frozen| {
        total
            .checked_add(*frozen)
            .ok_or(DistributionError::ArithmeticOverflow)
    })?;
    let goal = total_strength.min(capacity_total);
    if frozen_total > goal {
        return Err(DistributionError::InfeasibleFrozenStrength {
            frozen: frozen_total,
            goal,
        });
    }
    let mut targets: BTreeMap<_, _> = weights.keys().map(|&coordinate| (coordinate, 0)).collect();
    let mut active = weights.keys().copied().collect::<BTreeSet<_>>();
    let mut remaining = goal;
    loop {
        if active.is_empty() {
            break;
        }
        let provisional = weighted_targets(&weights, &capacities, &active, remaining)?;
        let violations = active
            .iter()
            .filter(|coordinate| provisional[coordinate] < lower_bounds[coordinate])
            .copied()
            .collect::<Vec<_>>();
        if violations.is_empty() {
            for (coordinate, target) in provisional {
                targets.insert(coordinate, target);
                remaining = remaining
                    .checked_sub(target)
                    .ok_or(DistributionError::ArithmeticOverflow)?;
            }
            break;
        }
        for coordinate in violations {
            let lower_bound = lower_bounds[&coordinate];
            targets.insert(coordinate, lower_bound);
            remaining = remaining.checked_sub(lower_bound).ok_or(
                DistributionError::InfeasibleFrozenStrength {
                    frozen: frozen_total,
                    goal,
                },
            )?;
            active.remove(&coordinate);
        }
    }

    let assigned = targets.values().try_fold(0_u64, |total, target| {
        total
            .checked_add(*target)
            .ok_or(DistributionError::ArithmeticOverflow)
    })?;
    debug_assert_eq!(assigned, goal - remaining);
    Ok(TargetDistribution {
        weights,
        targets,
        assigned,
        unassigned: total_strength - assigned,
    })
}

/// Apportions `goal` across `active` cells using caller-provided weights and
/// military capacities. This full-participation water fill is isolated so
/// commitment lower bounds can repeatedly constrain it.
fn weighted_targets(
    weights: &BTreeMap<Axial, u32>,
    capacities: &BTreeMap<Axial, Strength>,
    active: &BTreeSet<Axial>,
    goal: Strength,
) -> Result<BTreeMap<Axial, Strength>, DistributionError> {
    let mut remaining = goal;
    let mut targets = active
        .iter()
        .map(|&coordinate| (coordinate, 0))
        .collect::<BTreeMap<_, _>>();
    let mut unsaturated = active.clone();

    while remaining > 0 && !unsaturated.is_empty() {
        let total_score = unsaturated.iter().try_fold(0_u128, |total, coordinate| {
            total
                .checked_add(u128::from(capacities[coordinate]) * u128::from(weights[coordinate]))
                .ok_or(DistributionError::ArithmeticOverflow)
        })?;
        if total_score == 0 {
            break;
        }

        let saturated = unsaturated
            .iter()
            .filter_map(|coordinate| {
                let capacity = capacities[coordinate];
                let score = u128::from(capacity) * u128::from(weights[coordinate]);
                let numerator = u128::from(remaining).checked_mul(score)?;
                (numerator > u128::from(capacity) * total_score).then_some(*coordinate)
            })
            .collect::<Vec<_>>();
        if !saturated.is_empty() {
            for coordinate in saturated {
                let capacity = capacities[&coordinate];
                targets.insert(coordinate, capacity);
                remaining = remaining
                    .checked_sub(capacity)
                    .ok_or(DistributionError::ArithmeticOverflow)?;
                unsaturated.remove(&coordinate);
            }
            continue;
        }

        let mut floor_sum = 0_u64;
        let mut remainders = Vec::with_capacity(unsaturated.len());
        for &coordinate in &unsaturated {
            let score = u128::from(capacities[&coordinate]) * u128::from(weights[&coordinate]);
            let numerator = u128::from(remaining)
                .checked_mul(score)
                .ok_or(DistributionError::ArithmeticOverflow)?;
            let floor = Strength::try_from(numerator / total_score)
                .map_err(|_| DistributionError::ArithmeticOverflow)?;
            let remainder = numerator % total_score;
            targets.insert(coordinate, floor);
            floor_sum = floor_sum
                .checked_add(floor)
                .ok_or(DistributionError::ArithmeticOverflow)?;
            remainders.push((remainder, coordinate));
        }

        let leftover = remaining
            .checked_sub(floor_sum)
            .ok_or(DistributionError::ArithmeticOverflow)?;
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
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{Cell, ForceComposition};

    fn line(capacities: &[u64]) -> HexMap {
        let mut map = HexMap::new();
        for (q, &capacity) in capacities.iter().enumerate() {
            map.insert(Cell::ground(Axial::new(q as i32, 0), 0, Some(1), capacity));
        }
        map
    }

    fn occupied_line(capacities: &[u64], strengths: &[u64]) -> HexMap {
        assert_eq!(capacities.len(), strengths.len());
        let mut map = line(capacities);
        for (q, &strength) in strengths.iter().enumerate() {
            map.get_mut(Axial::new(q as i32, 0))
                .expect("line cell")
                .forces = ForceComposition::infantry(strength);
        }
        map
    }

    #[test]
    fn dense_weighted_targets_equalize_by_capacity_when_weights_match() {
        let coordinates = [Axial::new(0, 0), Axial::new(1, 0)];
        let capacities = [100_u64, 200];
        let result = redistribution_targets_dense_with_weights(
            &coordinates,
            &capacities,
            150,
            vec![UNIFORM_ALLOCATION_WEIGHT, UNIFORM_ALLOCATION_WEIGHT],
        )
        .unwrap();
        assert_eq!(result.targets, vec![50, 100]);
        assert_eq!(result.assigned, 150);
        assert_eq!(result.unassigned, 0);
    }

    #[test]
    fn dense_weighted_targets_respect_capacity_and_weight_bias() {
        let coordinates = [Axial::new(0, 0), Axial::new(1, 0), Axial::new(2, 0)];
        let capacities = [100_u64, 100, 100];
        let result = redistribution_targets_dense_with_weights(
            &coordinates,
            &capacities,
            150,
            vec![1, 2, 3],
        )
        .unwrap();
        assert_eq!(result.targets, vec![25, 50, 75]);
        assert_eq!(result.assigned, 150);
    }

    #[test]
    fn fallback_constraints_drain_sources_when_the_drawn_shape_fits() {
        let map = occupied_line(&[100, 100, 100], &[45, 45, 0]);
        let source_a = Axial::new(0, 0);
        let source_b = Axial::new(1, 0);
        let target = Axial::new(2, 0);

        let result = redistribution_targets_with_fallback_constraints(
            &map,
            1,
            BTreeMap::from([
                (source_a, 0),
                (source_b, 0),
                (target, UNIFORM_ALLOCATION_WEIGHT),
            ]),
            BTreeMap::from([(source_a, 0), (source_b, 0), (target, 0)]),
            90,
        )
        .unwrap();

        assert_eq!(result.targets[&source_a], 0);
        assert_eq!(result.targets[&source_b], 0);
        assert_eq!(result.targets[&target], 90);
        assert_eq!(result.assigned, 90);
        assert_eq!(result.unassigned, 0);
    }

    #[test]
    fn fallback_constraints_saturate_the_shape_then_conserve_overflow_on_sources() {
        let map = occupied_line(&[100, 60, 50], &[80, 40, 0]);
        let source_a = Axial::new(0, 0);
        let source_b = Axial::new(1, 0);
        let target = Axial::new(2, 0);

        let result = redistribution_targets_with_fallback_constraints(
            &map,
            1,
            BTreeMap::from([
                (source_a, 0),
                (source_b, 0),
                (target, UNIFORM_ALLOCATION_WEIGHT),
            ]),
            BTreeMap::from([(source_a, 0), (source_b, 0), (target, 0)]),
            120,
        )
        .unwrap();

        assert_eq!(result.targets[&target], 50);
        assert_eq!(result.targets[&source_a], 47);
        assert_eq!(result.targets[&source_b], 23);
        assert!(result.targets[&source_a] <= 80);
        assert!(result.targets[&source_b] <= 40);
        assert_eq!(result.targets.values().sum::<u64>(), 120);
        assert!(
            result.targets.iter().all(|(coordinate, strength)| *strength
                <= map.get(*coordinate).unwrap().military_capacity)
        );
    }

    #[test]
    fn fallback_constraints_preserve_frozen_strength_during_overflow() {
        let map = occupied_line(&[100, 100, 20], &[60, 20, 0]);
        let source_a = Axial::new(0, 0);
        let source_b = Axial::new(1, 0);
        let target = Axial::new(2, 0);

        let result = redistribution_targets_with_fallback_constraints(
            &map,
            1,
            BTreeMap::from([
                (source_a, 0),
                (source_b, 0),
                (target, UNIFORM_ALLOCATION_WEIGHT),
            ]),
            BTreeMap::from([(source_a, 45), (source_b, 0), (target, 0)]),
            80,
        )
        .unwrap();

        assert_eq!(result.targets[&target], 20);
        assert!(result.targets[&source_a] >= 45);
        assert_eq!(result.targets.values().sum::<u64>(), 80);
    }

    #[test]
    fn fallback_constraints_reject_only_when_all_capacity_is_insufficient() {
        let map = occupied_line(&[20, 20], &[20, 0]);
        let source = Axial::new(0, 0);
        let target = Axial::new(1, 0);

        assert_eq!(
            redistribution_targets_with_fallback_constraints(
                &map,
                1,
                BTreeMap::from([(source, 0), (target, UNIFORM_ALLOCATION_WEIGHT)]),
                BTreeMap::from([(source, 0), (target, 0)]),
                50,
            ),
            Err(DistributionError::InsufficientTargetCapacity { unassigned: 10 })
        );
    }

    #[test]
    fn fallback_constraints_exhaustively_preserve_strength_and_prefer_the_shape() {
        let source_a = Axial::new(0, 0);
        let source_b = Axial::new(1, 0);
        let target = Axial::new(2, 0);
        for strength_a in 0..=12 {
            for strength_b in 0..=12 {
                for target_capacity in 0..=30 {
                    let total = strength_a + strength_b;
                    let map = occupied_line(
                        &[strength_a + 7, strength_b + 11, target_capacity],
                        &[strength_a, strength_b, 0],
                    );
                    let result = redistribution_targets_with_fallback_constraints(
                        &map,
                        1,
                        BTreeMap::from([
                            (source_a, 0),
                            (source_b, 0),
                            (target, UNIFORM_ALLOCATION_WEIGHT),
                        ]),
                        BTreeMap::from([(source_a, 0), (source_b, 0), (target, 0)]),
                        total,
                    )
                    .unwrap();

                    assert_eq!(result.targets.values().sum::<u64>(), total);
                    assert_eq!(result.targets[&target], total.min(target_capacity));
                    assert!(result.targets[&source_a] <= strength_a);
                    assert!(result.targets[&source_b] <= strength_b);
                    assert_eq!(result.assigned, total);
                    assert_eq!(result.unassigned, 0);
                }
            }
        }
    }
}
