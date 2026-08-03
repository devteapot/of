use std::collections::{BTreeMap, BTreeSet};

use crate::{
    conquest::BASIS_POINTS,
    coord::Axial,
    map::{HexMap, PlayerId, Strength},
};

pub const BALANCE_WEIGHT: u32 = 10_000;
const RADIAL_LOW_WEIGHT: u32 = 5_000;
const RADIAL_HIGH_WEIGHT: u32 = 15_000;

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
    /// Density is greatest near the selection's geometric center.
    CoreLoad,
    /// Density is greatest farthest from the selection's geometric center.
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
            radial_weights(&coordinates, preset)
        }
    }
}

/// Computes radial weights without rounding the selection centroid to a cell.
///
/// For a selection of `n` cube coordinates, `n * coordinate - sum` is the
/// displacement from the fractional centroid with the common denominator
/// removed. Squared cube distance therefore remains integer-only, symmetric,
/// and translation invariant even when the center lies between hexes.
fn radial_weights(
    coordinates: &BTreeSet<Axial>,
    preset: DistributionPreset,
) -> Result<BTreeMap<Axial, u32>, DistributionError> {
    let count =
        i128::try_from(coordinates.len()).map_err(|_| DistributionError::ArithmeticOverflow)?;
    let (sum_x, sum_y, sum_z) = coordinates.iter().try_fold(
        (0_i128, 0_i128, 0_i128),
        |(sum_x, sum_y, sum_z), coordinate| {
            let cube = coordinate.cube();
            Ok::<_, DistributionError>((
                sum_x
                    .checked_add(i128::from(cube.x))
                    .ok_or(DistributionError::ArithmeticOverflow)?,
                sum_y
                    .checked_add(i128::from(cube.y))
                    .ok_or(DistributionError::ArithmeticOverflow)?,
                sum_z
                    .checked_add(i128::from(cube.z))
                    .ok_or(DistributionError::ArithmeticOverflow)?,
            ))
        },
    )?;
    let scores = coordinates
        .iter()
        .map(|&coordinate| {
            let cube = coordinate.cube();
            let delta_x = count
                .checked_mul(i128::from(cube.x))
                .and_then(|value| value.checked_sub(sum_x))
                .ok_or(DistributionError::ArithmeticOverflow)?;
            let delta_y = count
                .checked_mul(i128::from(cube.y))
                .and_then(|value| value.checked_sub(sum_y))
                .ok_or(DistributionError::ArithmeticOverflow)?;
            let delta_z = count
                .checked_mul(i128::from(cube.z))
                .and_then(|value| value.checked_sub(sum_z))
                .ok_or(DistributionError::ArithmeticOverflow)?;
            let score = delta_x
                .checked_mul(delta_x)
                .and_then(|value| value.checked_add(delta_y.checked_mul(delta_y)?))
                .and_then(|value| value.checked_add(delta_z.checked_mul(delta_z)?))
                .and_then(|value| u128::try_from(value).ok())
                .ok_or(DistributionError::ArithmeticOverflow)?;
            Ok((coordinate, score))
        })
        .collect::<Result<BTreeMap<_, _>, DistributionError>>()?;
    let minimum = *scores.values().min().expect("selection is not empty");
    let maximum = *scores.values().max().expect("selection is not empty");
    let span = maximum - minimum;
    let weight_span = u128::from(RADIAL_HIGH_WEIGHT - RADIAL_LOW_WEIGHT);

    scores
        .into_iter()
        .map(|(coordinate, score)| {
            let offset = weight_span
                .checked_mul(score - minimum)
                .ok_or(DistributionError::ArithmeticOverflow)?
                .checked_div(span)
                .unwrap_or(weight_span / 2) as u32;
            let weight = match preset {
                DistributionPreset::CoreLoad => RADIAL_HIGH_WEIGHT - offset,
                DistributionPreset::PerimeterLoad => RADIAL_LOW_WEIGHT + offset,
                DistributionPreset::Balance | DistributionPreset::FrontLoad { .. } => {
                    unreachable!("radial_weights only accepts radial presets")
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
    let mut capacity_total = 0_u64;
    let mut frozen_total = 0_u64;
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
        frozen_total = frozen_total
            .checked_add(frozen)
            .ok_or(DistributionError::ArithmeticOverflow)?;
        capacity_total = capacity_total
            .checked_add(cell.military_capacity)
            .ok_or(DistributionError::ArithmeticOverflow)?;
        capacities.insert(coordinate, cell.military_capacity);
        lower_bounds.insert(coordinate, frozen);
    }

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
        // Adding lower bounds can only reduce the proportional scale for the
        // remaining cells, so a violated lower bound can be fixed permanently.
        // Re-running the ordinary capacity apportionment also lets previously
        // saturated cells fall below capacity when the scale decreases.
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
    fn core_and_perimeter_are_symmetric_about_fractional_centroid() {
        let selection = [
            Axial::new(0, 0),
            Axial::new(1, 0),
            Axial::new(2, 0),
            Axial::new(3, 0),
            Axial::new(4, 0),
        ];
        let core = distribution_weights(selection, DistributionPreset::CoreLoad).unwrap();
        let perimeter = distribution_weights(selection, DistributionPreset::PerimeterLoad).unwrap();

        assert_eq!(core[&Axial::new(0, 0)], RADIAL_LOW_WEIGHT);
        assert_eq!(core[&Axial::new(2, 0)], RADIAL_HIGH_WEIGHT);
        assert_eq!(core[&Axial::new(0, 0)], core[&Axial::new(4, 0)]);
        assert_eq!(core[&Axial::new(1, 0)], core[&Axial::new(3, 0)]);
        for coordinate in selection {
            assert_eq!(
                core[&coordinate] + perimeter[&coordinate],
                RADIAL_LOW_WEIGHT + RADIAL_HIGH_WEIGHT
            );
        }
    }

    #[test]
    fn radial_weights_are_translation_invariant_with_center_between_cells() {
        let original = [
            Axial::new(0, 0),
            Axial::new(1, 0),
            Axial::new(2, -1),
            Axial::new(3, -1),
        ];
        let translated = original.map(|coordinate| coordinate + Axial::new(37, -91));

        for preset in [
            DistributionPreset::CoreLoad,
            DistributionPreset::PerimeterLoad,
        ] {
            let original_weights = distribution_weights(original, preset).unwrap();
            let translated_weights = distribution_weights(translated, preset).unwrap();
            assert_eq!(
                original_weights.values().copied().collect::<Vec<_>>(),
                translated_weights.values().copied().collect::<Vec<_>>()
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
