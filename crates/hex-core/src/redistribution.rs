use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::{
    conquest::BASIS_POINTS,
    coord::Axial,
    map::{HexMap, PlayerId, Strength},
};

pub const BALANCE_WEIGHT: u32 = 10_000;
const DEPTH_LOW_WEIGHT: u32 = 5_000;
const DEPTH_HIGH_WEIGHT: u32 = 15_000;

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
    /// Density increases with distance from the selection's exposed boundary.
    CoreLoad,
    /// Density decreases with distance from the selection's exposed boundary.
    PerimeterLoad,
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
    InvalidCommitmentBps(u32),
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
        DistributionPreset::CoreLoad | DistributionPreset::PerimeterLoad => {
            boundary_depth_weights(&coordinates, preset)
        }
    }
}

/// Measures topological depth from the exact exposed boundary of a selection.
/// Every cell adjacent to any unselected coordinate seeds depth zero; a stable
/// multi-source BFS then assigns inward layers. Concavities and holes therefore
/// influence loading directly instead of being approximated by a centroid.
fn boundary_depth_weights(
    coordinates: &BTreeSet<Axial>,
    preset: DistributionPreset,
) -> Result<BTreeMap<Axial, u32>, DistributionError> {
    let mut depths = BTreeMap::<Axial, u64>::new();
    let mut pending = VecDeque::new();
    for &coordinate in coordinates {
        if coordinate
            .neighbors()
            .into_iter()
            .any(|neighbor| !coordinates.contains(&neighbor))
        {
            depths.insert(coordinate, 0);
            pending.push_back(coordinate);
        }
    }

    while let Some(coordinate) = pending.pop_front() {
        let next_depth = depths[&coordinate]
            .checked_add(1)
            .ok_or(DistributionError::ArithmeticOverflow)?;
        for neighbor in coordinate.neighbors() {
            if coordinates.contains(&neighbor) && !depths.contains_key(&neighbor) {
                depths.insert(neighbor, next_depth);
                pending.push_back(neighbor);
            }
        }
    }
    debug_assert_eq!(depths.len(), coordinates.len());
    let max_depth = depths
        .values()
        .copied()
        .max()
        .expect("a finite non-empty selection has an exposed boundary");
    if max_depth == 0 {
        return Ok(coordinates
            .iter()
            .map(|&coordinate| (coordinate, BALANCE_WEIGHT))
            .collect());
    }

    let weight_span = u128::from(DEPTH_HIGH_WEIGHT - DEPTH_LOW_WEIGHT);
    depths
        .into_iter()
        .map(|(coordinate, depth)| {
            let offset = weight_span
                .checked_mul(u128::from(depth))
                .ok_or(DistributionError::ArithmeticOverflow)?
                / u128::from(max_depth);
            let offset =
                u32::try_from(offset).map_err(|_| DistributionError::ArithmeticOverflow)?;
            let weight = match preset {
                DistributionPreset::CoreLoad => DEPTH_LOW_WEIGHT + offset,
                DistributionPreset::PerimeterLoad => DEPTH_HIGH_WEIGHT - offset,
                DistributionPreset::Balance | DistributionPreset::FrontLoad { .. } => {
                    unreachable!("boundary_depth_weights only accepts depth presets")
                }
            };
            Ok((coordinate, weight))
        })
        .collect()
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
    redistribution_targets_with_commitment(
        map,
        owner,
        selection,
        total_strength,
        preset,
        BASIS_POINTS,
    )
}

/// Produces capacity-safe targets while limiting how much of each current
/// stack may participate.
///
/// `amount_bps` selects at most that share (rounded down) of every current
/// stack. The remainder becomes a frozen lower bound for that cell. The full
/// strength pool is then apportioned by the preset subject to those lower
/// bounds and normal military capacities. This lets a partial command move
/// strength into a cell freely while never moving more than the selected share
/// out of any source.
pub fn redistribution_targets_with_commitment<I>(
    map: &HexMap,
    owner: PlayerId,
    selection: I,
    total_strength: Strength,
    preset: DistributionPreset,
    amount_bps: u32,
) -> Result<TargetDistribution, DistributionError>
where
    I: IntoIterator<Item = Axial>,
{
    if amount_bps > BASIS_POINTS {
        return Err(DistributionError::InvalidCommitmentBps(amount_bps));
    }
    let weights = distribution_weights(selection, preset)?;
    let mut capacities = BTreeMap::new();
    let mut lower_bounds = BTreeMap::new();
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
        let participating =
            (u128::from(current) * u128::from(amount_bps) / u128::from(BASIS_POINTS)) as Strength;
        let frozen = current - participating;
        capacities.insert(coordinate, cell.military_capacity);
        lower_bounds.insert(coordinate, frozen);
    }

    constrained_targets(weights, capacities, lower_bounds, total_strength)
}

/// Apportions strength with caller-supplied density weights and per-cell frozen
/// lower bounds.
///
/// This is the shape-drawing primitive: target cells receive positive weight,
/// source-only cells receive zero weight, and the lower bound limits how much
/// of each source may leave. Unlike broad preset redistribution, a drawn shape
/// is exact: every unit of `total_strength` must fit or the operation is
/// rejected. All constrained cells must be owned map cells and the weight and
/// lower-bound maps must have identical keys.
pub fn redistribution_targets_with_constraints(
    map: &HexMap,
    owner: PlayerId,
    weights: BTreeMap<Axial, u32>,
    lower_bounds: BTreeMap<Axial, Strength>,
    total_strength: Strength,
) -> Result<TargetDistribution, DistributionError> {
    let capacities = validated_constraint_capacities(map, owner, &weights, &lower_bounds)?;
    let distribution = constrained_targets(weights, capacities, lower_bounds, total_strength)?;
    if distribution.unassigned > 0 {
        return Err(DistributionError::InsufficientTargetCapacity {
            unassigned: distribution.unassigned,
        });
    }
    Ok(distribution)
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
///
/// Unlike [`redistribution_targets_with_constraints`], insufficient capacity
/// in the preferred cells is not an error. The operation fails only when the
/// complete preferred-plus-fallback set cannot conserve `total_strength`.
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
            .map(|coordinate| (*coordinate, BALANCE_WEIGHT))
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

/// Apportions `goal` across `active` cells using preset density scores and
/// military capacities. This is the original full-participation water fill,
/// isolated so commitment lower bounds can repeatedly constrain it.
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

    fn hex_disk(radius: i32) -> BTreeSet<Axial> {
        let mut selection = BTreeSet::new();
        for q in -radius..=radius {
            for r in -radius..=radius {
                let coordinate = Axial::new(q, r);
                if Axial::ZERO.distance(coordinate) <= radius as u64 {
                    selection.insert(coordinate);
                }
            }
        }
        selection
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
    fn commitment_freezes_the_unselected_share_of_each_stack() {
        let map = occupied_line(&[100, 100], &[100, 0]);
        for (amount_bps, expected) in [
            (0, [100, 0]),
            (2_500, [75, 25]),
            (5_000, [50, 50]),
            (10_000, [50, 50]),
        ] {
            let result = redistribution_targets_with_commitment(
                &map,
                1,
                map.coordinates(),
                100,
                DistributionPreset::Balance,
                amount_bps,
            )
            .unwrap();
            assert_eq!(
                result.targets.values().copied().collect::<Vec<_>>(),
                expected,
                "unexpected targets at {amount_bps} bps"
            );
        }
    }

    #[test]
    fn commitment_respects_asymmetric_capacity_and_source_bounds() {
        let map = occupied_line(&[100, 200], &[100, 0]);
        let cases = [
            (0, [100, 0]),
            (2_500, [75, 25]),
            (5_000, [50, 50]),
            (10_000, [33, 67]),
        ];
        for (amount_bps, expected) in cases {
            let result = redistribution_targets_with_commitment(
                &map,
                1,
                map.coordinates(),
                100,
                DistributionPreset::Balance,
                amount_bps,
            )
            .unwrap();
            assert_eq!(
                result.targets.values().copied().collect::<Vec<_>>(),
                expected
            );
            assert_eq!(result.targets.values().sum::<u64>(), 100);
            assert!(result.targets[&Axial::new(0, 0)] >= 100 - amount_bps as u64 / 100);
        }
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
    fn solid_disk_loads_by_exact_boundary_depth() {
        let selection = hex_disk(2);
        let core =
            distribution_weights(selection.iter().copied(), DistributionPreset::CoreLoad).unwrap();
        let perimeter =
            distribution_weights(selection.iter().copied(), DistributionPreset::PerimeterLoad)
                .unwrap();

        for coordinate in selection {
            let distance = Axial::ZERO.distance(coordinate);
            let (expected_core, expected_perimeter) = match distance {
                0 => (DEPTH_HIGH_WEIGHT, DEPTH_LOW_WEIGHT),
                1 => (BALANCE_WEIGHT, BALANCE_WEIGHT),
                2 => (DEPTH_LOW_WEIGHT, DEPTH_HIGH_WEIGHT),
                _ => unreachable!("radius-two disk has only three inward layers"),
            };
            assert_eq!(core[&coordinate], expected_core);
            assert_eq!(perimeter[&coordinate], expected_perimeter);
        }
    }

    #[test]
    fn a_notch_is_boundary_and_depth_weights_are_translation_invariant() {
        let mut original = hex_disk(3);
        for q in 1..=3 {
            original.remove(&Axial::new(q, 0));
        }
        let translation = Axial::new(37, -91);
        let translated = original
            .iter()
            .map(|&coordinate| coordinate + translation)
            .collect::<BTreeSet<_>>();

        for preset in [
            DistributionPreset::CoreLoad,
            DistributionPreset::PerimeterLoad,
        ] {
            let original_weights = distribution_weights(original.iter().copied(), preset).unwrap();
            let translated_weights =
                distribution_weights(translated.iter().copied(), preset).unwrap();
            for &coordinate in &original {
                assert_eq!(
                    original_weights[&coordinate],
                    translated_weights[&(coordinate + translation)]
                );
            }
        }

        let core =
            distribution_weights(original.iter().copied(), DistributionPreset::CoreLoad).unwrap();
        assert_eq!(core[&Axial::ZERO], DEPTH_LOW_WEIGHT);
        assert!(core.values().any(|&weight| weight > DEPTH_LOW_WEIGHT));
    }

    #[test]
    fn thin_and_single_cell_selections_use_the_midpoint_weight() {
        let thin = (0..=6).map(|q| Axial::new(q, 0)).collect::<BTreeSet<_>>();
        let single = BTreeSet::from([Axial::new(-4, 9)]);

        for selection in [&thin, &single] {
            for preset in [
                DistributionPreset::CoreLoad,
                DistributionPreset::PerimeterLoad,
            ] {
                let weights = distribution_weights(selection.iter().copied(), preset).unwrap();
                assert!(weights.values().all(|&weight| weight == BALANCE_WEIGHT));
            }
        }
    }

    #[test]
    fn core_and_perimeter_weights_are_complementary_at_every_depth() {
        let mut selection = hex_disk(4);
        selection.remove(&Axial::new(4, 0));
        selection.remove(&Axial::new(3, 0));
        let core =
            distribution_weights(selection.iter().copied(), DistributionPreset::CoreLoad).unwrap();
        let perimeter =
            distribution_weights(selection.iter().copied(), DistributionPreset::PerimeterLoad)
                .unwrap();

        for coordinate in selection {
            assert_eq!(
                core[&coordinate] + perimeter[&coordinate],
                DEPTH_LOW_WEIGHT + DEPTH_HIGH_WEIGHT
            );
        }
    }

    #[test]
    fn targets_conserve_strength_and_never_exceed_capacity() {
        let map = line(&[3, 7, 11, 19]);
        for total in 0..=50 {
            for preset in [
                DistributionPreset::Balance,
                DistributionPreset::front_load(Axial::new(1, -1)),
                DistributionPreset::CoreLoad,
                DistributionPreset::PerimeterLoad,
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
    fn committed_targets_conserve_and_stay_within_all_bounds() {
        let capacities = [3, 7, 11, 19];
        let strengths = [3, 5, 0, 12];
        let map = occupied_line(&capacities, &strengths);
        let total = strengths.into_iter().sum();

        for amount_bps in [0, 1, 2_500, 5_000, 9_999, 10_000] {
            for preset in [
                DistributionPreset::Balance,
                DistributionPreset::front_load(Axial::new(1, -1)),
                DistributionPreset::CoreLoad,
                DistributionPreset::PerimeterLoad,
            ] {
                let result = redistribution_targets_with_commitment(
                    &map,
                    1,
                    map.coordinates(),
                    total,
                    preset,
                    amount_bps,
                )
                .unwrap();
                assert_eq!(result.assigned, total);
                assert_eq!(result.unassigned, 0);
                assert_eq!(result.targets.values().sum::<u64>(), total);
                for (index, coordinate) in map.coordinates().enumerate() {
                    let movable = u128::from(strengths[index]) * u128::from(amount_bps)
                        / u128::from(BASIS_POINTS);
                    let frozen = strengths[index] - movable as u64;
                    assert!(result.targets[&coordinate] >= frozen);
                    assert!(result.targets[&coordinate] <= capacities[index]);
                }
            }
        }
    }

    #[test]
    fn constrained_targets_move_only_the_unfrozen_share_into_a_drawn_shape() {
        let map = occupied_line(&[100, 100, 100], &[80, 0, 0]);
        let source = Axial::new(0, 0);
        let middle = Axial::new(1, 0);
        let target = Axial::new(2, 0);
        let result = redistribution_targets_with_constraints(
            &map,
            1,
            BTreeMap::from([(source, 0), (middle, 0), (target, BALANCE_WEIGHT)]),
            BTreeMap::from([(source, 40), (middle, 0), (target, 0)]),
            80,
        )
        .unwrap();

        assert_eq!(result.targets[&source], 40);
        assert_eq!(result.targets[&middle], 0);
        assert_eq!(result.targets[&target], 40);
        assert_eq!(result.assigned, 80);
        assert_eq!(result.unassigned, 0);
        assert_eq!(result.targets.values().sum::<u64>(), 80);
    }

    #[test]
    fn constrained_targets_reject_a_shape_that_cannot_hold_the_movable_share() {
        let map = occupied_line(&[100, 20], &[80, 0]);
        let source = Axial::new(0, 0);
        let target = Axial::new(1, 0);

        assert_eq!(
            redistribution_targets_with_constraints(
                &map,
                1,
                BTreeMap::from([(source, 0), (target, BALANCE_WEIGHT)]),
                BTreeMap::from([(source, 40), (target, 0)]),
                80,
            ),
            Err(DistributionError::InsufficientTargetCapacity { unassigned: 20 })
        );
    }

    #[test]
    fn constrained_targets_reject_a_nonempty_movable_share_without_a_target() {
        let map = occupied_line(&[100], &[80]);
        let source = Axial::new(0, 0);

        assert_eq!(
            redistribution_targets_with_constraints(
                &map,
                1,
                BTreeMap::from([(source, 0)]),
                BTreeMap::from([(source, 79)]),
                80,
            ),
            Err(DistributionError::InsufficientTargetCapacity { unassigned: 1 })
        );
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
            BTreeMap::from([(source_a, 0), (source_b, 0), (target, BALANCE_WEIGHT)]),
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
            BTreeMap::from([(source_a, 0), (source_b, 0), (target, BALANCE_WEIGHT)]),
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
            BTreeMap::from([(source_a, 0), (source_b, 0), (target, BALANCE_WEIGHT)]),
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
                BTreeMap::from([(source, 0), (target, BALANCE_WEIGHT)]),
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
                        BTreeMap::from([(source_a, 0), (source_b, 0), (target, BALANCE_WEIGHT)]),
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

        let map = occupied_line(&[100], &[100]);
        assert_eq!(
            redistribution_targets_with_commitment(
                &map,
                1,
                map.coordinates(),
                100,
                DistributionPreset::Balance,
                BASIS_POINTS + 1,
            ),
            Err(DistributionError::InvalidCommitmentBps(BASIS_POINTS + 1))
        );
        assert_eq!(
            redistribution_targets_with_commitment(
                &map,
                1,
                map.coordinates(),
                50,
                DistributionPreset::Balance,
                0,
            ),
            Err(DistributionError::InfeasibleFrozenStrength {
                frozen: 100,
                goal: 50,
            })
        );
    }
}
