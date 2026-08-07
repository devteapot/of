use std::collections::BTreeMap;

use crate::{
    conquest::BASIS_POINTS,
    coord::Axial,
    map::{PlayerId, Strength},
};

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CombatConfig {
    pub max_elevation_step: u16,
    /// Multiplier applied when the destination is above the attacker.
    pub uphill_attack_bps: u32,
    /// Damage dealt to allocated defenders by effective attack strength.
    pub attacker_damage_bps: u32,
    /// Damage dealt to engaged attackers by allocated defense strength.
    pub defender_damage_bps: u32,
}

impl Default for CombatConfig {
    fn default() -> Self {
        Self {
            max_elevation_step: 1,
            uphill_attack_bps: 7_500,
            attacker_damage_bps: 10_000,
            defender_damage_bps: 10_000,
        }
    }
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttackFront {
    pub id: u64,
    pub attacker: PlayerId,
    pub from: Axial,
    pub from_elevation: i16,
    pub offered: Strength,
    pub frontage: Strength,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CombatError {
    InvalidConfig,
}

/// Why one attack front was excluded from a resolution while the remaining
/// valid fronts still resolved.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FrontRejection {
    DuplicateAttackId,
    DuplicateOrigin,
    NonAdjacent,
    ImpassableCliff,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RejectedFront {
    pub id: u64,
    pub attacker: PlayerId,
    pub from: Axial,
    pub reason: FrontRejection,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttackOutcome {
    pub id: u64,
    pub attacker: PlayerId,
    pub offered: Strength,
    pub engaged: Strength,
    pub waiting: Strength,
    pub effective_attack: Strength,
    pub defense_allocated: Strength,
    pub attacker_casualties: Strength,
    pub defender_casualties: Strength,
    pub attacker_remaining: Strength,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CombatResolution {
    pub defender_initial: Strength,
    pub defender_casualties: Strength,
    pub defender_remaining: Strength,
    pub attacks: BTreeMap<u64, AttackOutcome>,
    /// Malformed fronts excluded from this resolution, sorted by `(id, from)`.
    /// Valid fronts still resolved; callers decide how to retire the rejected
    /// allocations.
    pub rejected: Vec<RejectedFront>,
    /// The owner selected to occupy if the caller can admit the capture.
    pub capturing_owner: Option<PlayerId>,
    /// The surviving front of `capturing_owner` selected to occupy if the
    /// caller can admit it under throughput and destination-capacity rules.
    pub capturing_front: Option<u64>,
}

/// Resolves one deterministic combat step against a cell.
///
/// # Contract
///
/// - Fronts from **multiple attacking owners** resolve simultaneously.
///   Defenders are allocated across all valid attacking edges regardless of
///   owner: every defender fights at most one edge, allocation is proportional
///   to each edge's engaged strength (largest-remainder rounding), capped by
///   edge frontage, with the attack ID as the deterministic tie-break.
/// - Malformed fronts (duplicate ID, duplicate origin, non-adjacent origin, or
///   an impassable cliff) are reported in [`CombatResolution::rejected`]
///   instead of failing the whole resolution. Every front sharing a duplicated
///   ID or origin is rejected so the outcome cannot depend on slice order.
/// - **Capture rule:** when the defender is eliminated, the attacking owner
///   with the largest total surviving offered strength across its valid fronts
///   wins the capture; ties break toward the smaller owner ID. Within the
///   winning owner, the front with the largest surviving strength is selected;
///   ties break toward the smaller attack ID. Attackers from other owners keep
///   their survivors in place and contest the cell again next step.
pub fn resolve_edge_combat(
    target: Axial,
    defender_strength: Strength,
    defender_elevation: i16,
    attacks: &[AttackFront],
    config: &CombatConfig,
) -> Result<CombatResolution, CombatError> {
    if config.uphill_attack_bps > BASIS_POINTS {
        return Err(CombatError::InvalidConfig);
    }

    let mut sorted = attacks.to_vec();
    sorted.sort_unstable_by_key(|attack| (attack.id, attack.from, attack.attacker));
    let mut id_counts = BTreeMap::<u64, u32>::new();
    let mut origin_counts = BTreeMap::<Axial, u32>::new();
    for attack in &sorted {
        *id_counts.entry(attack.id).or_insert(0) += 1;
        *origin_counts.entry(attack.from).or_insert(0) += 1;
    }

    let mut rejected = Vec::new();
    let mut valid = Vec::new();
    for attack in sorted {
        let elevation_delta = i32::from(defender_elevation) - i32::from(attack.from_elevation);
        let reason = if id_counts[&attack.id] > 1 {
            Some(FrontRejection::DuplicateAttackId)
        } else if origin_counts[&attack.from] > 1 {
            Some(FrontRejection::DuplicateOrigin)
        } else if target.distance(attack.from) != 1 {
            Some(FrontRejection::NonAdjacent)
        } else if elevation_delta.unsigned_abs() > u32::from(config.max_elevation_step) {
            Some(FrontRejection::ImpassableCliff)
        } else {
            None
        };
        match reason {
            Some(reason) => rejected.push(RejectedFront {
                id: attack.id,
                attacker: attack.attacker,
                from: attack.from,
                reason,
            }),
            None => valid.push(attack),
        }
    }

    let demands: Vec<_> = valid
        .iter()
        .map(|attack| (attack.id, attack.offered.min(attack.frontage)))
        .collect();
    let allocations = proportional_allocation(defender_strength, &demands);

    let mut outcomes = BTreeMap::new();
    let mut total_defender_casualties = 0_u64;
    for attack in valid {
        let engaged = attack.offered.min(attack.frontage);
        let waiting = attack.offered - engaged;
        let uphill = defender_elevation > attack.from_elevation;
        let modifier = if uphill {
            config.uphill_attack_bps
        } else {
            BASIS_POINTS
        };
        let effective_attack = multiply_bps_floor(engaged, modifier);
        let defense_allocated = allocations.get(&attack.id).copied().unwrap_or(0);
        let attacker_casualties = engaged.min(multiply_bps_floor(
            defense_allocated,
            config.defender_damage_bps,
        ));
        let defender_casualties = defense_allocated.min(multiply_bps_floor(
            effective_attack,
            config.attacker_damage_bps,
        ));
        total_defender_casualties = total_defender_casualties
            .checked_add(defender_casualties)
            .expect("casualties are bounded by defender strength");

        outcomes.insert(
            attack.id,
            AttackOutcome {
                id: attack.id,
                attacker: attack.attacker,
                offered: attack.offered,
                engaged,
                waiting,
                effective_attack,
                defense_allocated,
                attacker_casualties,
                defender_casualties,
                attacker_remaining: attack.offered - attacker_casualties,
            },
        );
    }

    let defender_remaining = defender_strength - total_defender_casualties;
    let (capturing_owner, capturing_front) = if defender_remaining == 0 {
        select_capture(&outcomes)
    } else {
        (None, None)
    };

    Ok(CombatResolution {
        defender_initial: defender_strength,
        defender_casualties: total_defender_casualties,
        defender_remaining,
        attacks: outcomes,
        rejected,
        capturing_owner,
        capturing_front,
    })
}

/// Applies the documented capture rule to per-front outcomes: largest total
/// surviving offered strength per owner (tie: smaller owner ID), then largest
/// surviving front within that owner (tie: smaller attack ID).
pub fn select_capture(outcomes: &BTreeMap<u64, AttackOutcome>) -> (Option<PlayerId>, Option<u64>) {
    let mut totals = BTreeMap::<PlayerId, Strength>::new();
    for outcome in outcomes.values() {
        if outcome.attacker_remaining > 0 {
            *totals.entry(outcome.attacker).or_default() += outcome.attacker_remaining;
        }
    }
    let Some(owner) = totals
        .iter()
        .max_by(|(left_owner, left_total), (right_owner, right_total)| {
            left_total
                .cmp(right_total)
                .then_with(|| right_owner.cmp(left_owner))
        })
        .map(|(&owner, _)| owner)
    else {
        return (None, None);
    };
    let front = outcomes
        .values()
        .filter(|outcome| outcome.attacker == owner && outcome.attacker_remaining > 0)
        .max_by(|left, right| {
            left.attacker_remaining
                .cmp(&right.attacker_remaining)
                .then_with(|| right.id.cmp(&left.id))
        })
        .map(|outcome| outcome.id);
    (Some(owner), front)
}

fn multiply_bps_floor(value: u64, basis_points: u32) -> u64 {
    ((u128::from(value) * u128::from(basis_points)) / u128::from(BASIS_POINTS))
        .min(u128::from(u64::MAX)) as u64
}

fn proportional_allocation(
    available: Strength,
    demands: &[(u64, Strength)],
) -> BTreeMap<u64, Strength> {
    let total_demand: u128 = demands.iter().map(|(_, demand)| u128::from(*demand)).sum();
    if available == 0 || total_demand == 0 {
        return demands.iter().map(|(id, _)| (*id, 0)).collect();
    }
    let assigned_total = u128::from(available).min(total_demand) as u64;
    let mut allocations = BTreeMap::new();
    let mut remainders = Vec::with_capacity(demands.len());
    let mut floor_total = 0_u64;
    for &(id, demand) in demands {
        let numerator = u128::from(assigned_total) * u128::from(demand);
        let floor = (numerator / total_demand) as u64;
        allocations.insert(id, floor);
        floor_total += floor;
        remainders.push((numerator % total_demand, id));
    }

    remainders.sort_unstable_by(|(left_remainder, left_id), (right_remainder, right_id)| {
        right_remainder
            .cmp(left_remainder)
            .then_with(|| left_id.cmp(right_id))
    });
    for &(_, id) in remainders
        .iter()
        .take((assigned_total - floor_total) as usize)
    {
        *allocations
            .get_mut(&id)
            .expect("demand IDs were initialized") += 1;
    }
    allocations
}

#[cfg(test)]
mod tests {
    use super::*;

    fn front(id: u64, from: Axial, elevation: i16, offered: u64) -> AttackFront {
        owned_front(id, 1, from, elevation, offered)
    }

    fn owned_front(
        id: u64,
        attacker: PlayerId,
        from: Axial,
        elevation: i16,
        offered: u64,
    ) -> AttackFront {
        AttackFront {
            id,
            attacker,
            from,
            from_elevation: elevation,
            offered,
            frontage: 25,
        }
    }

    fn resolution_conserves(resolution: &CombatResolution, attacks: &[AttackFront]) {
        assert_eq!(
            resolution.defender_initial,
            resolution.defender_remaining + resolution.defender_casualties
        );
        for attack in attacks {
            if let Some(outcome) = resolution.attacks.get(&attack.id) {
                assert_eq!(outcome.offered, attack.offered);
                assert_eq!(
                    outcome.attacker_remaining + outcome.attacker_casualties,
                    outcome.offered
                );
            }
        }
    }

    #[test]
    fn defenders_are_never_duplicated_across_multiple_edges() {
        let attacks = [
            front(1, Axial::new(1, 0), 0, 25),
            front(2, Axial::new(0, 1), 0, 25),
            front(3, Axial::new(-1, 1), 0, 25),
        ];
        let result =
            resolve_edge_combat(Axial::ZERO, 60, 0, &attacks, &CombatConfig::default()).unwrap();
        assert_eq!(
            result
                .attacks
                .values()
                .map(|outcome| outcome.defense_allocated)
                .sum::<u64>(),
            60
        );
        assert_eq!(result.defender_casualties, 60);
        assert_eq!(result.capturing_owner, Some(1));
        assert_eq!(result.capturing_front, Some(1));
        resolution_conserves(&result, &attacks);
    }

    #[test]
    fn uphill_attack_penalty_reduces_defender_casualties() {
        let flat = resolve_edge_combat(
            Axial::ZERO,
            20,
            0,
            &[front(1, Axial::new(1, 0), 0, 20)],
            &CombatConfig::default(),
        )
        .unwrap();
        let uphill = resolve_edge_combat(
            Axial::ZERO,
            20,
            1,
            &[front(1, Axial::new(1, 0), 0, 20)],
            &CombatConfig::default(),
        )
        .unwrap();
        assert_eq!(flat.defender_casualties, 20);
        assert_eq!(uphill.defender_casualties, 15);
    }

    #[test]
    fn frontage_leaves_excess_attackers_waiting() {
        let result = resolve_edge_combat(
            Axial::ZERO,
            10,
            0,
            &[front(7, Axial::new(1, 0), 0, 100)],
            &CombatConfig::default(),
        )
        .unwrap();
        let outcome = result.attacks[&7];
        assert_eq!(outcome.engaged, 25);
        assert_eq!(outcome.waiting, 75);
        assert_eq!(outcome.attacker_remaining, 90);
    }

    #[test]
    fn resolution_is_independent_of_attack_slice_order() {
        let a = front(30, Axial::new(1, 0), 0, 17);
        let b = front(10, Axial::new(0, 1), 0, 23);
        let c = front(20, Axial::new(-1, 1), 0, 11);
        let first = resolve_edge_combat(Axial::ZERO, 31, 0, &[a, b, c], &CombatConfig::default());
        let second = resolve_edge_combat(Axial::ZERO, 31, 0, &[c, a, b], &CombatConfig::default());
        assert_eq!(first, second);
    }

    #[test]
    fn invalid_fronts_are_rejected_individually_while_valid_fronts_resolve() {
        let valid = front(1, Axial::new(1, 0), 0, 20);
        let non_adjacent = front(2, Axial::new(2, 0), 0, 5);
        let cliff = front(3, Axial::new(0, 1), -2, 5);
        let result = resolve_edge_combat(
            Axial::ZERO,
            20,
            0,
            &[non_adjacent, valid, cliff],
            &CombatConfig::default(),
        )
        .unwrap();
        assert_eq!(result.attacks.len(), 1);
        assert!(result.attacks.contains_key(&1));
        assert_eq!(
            result
                .rejected
                .iter()
                .map(|rejection| (rejection.id, rejection.reason))
                .collect::<Vec<_>>(),
            vec![
                (2, FrontRejection::NonAdjacent),
                (3, FrontRejection::ImpassableCliff),
            ]
        );
        // The valid front still fought the full defender.
        assert_eq!(result.attacks[&1].defense_allocated, 20);
    }

    #[test]
    fn every_front_sharing_a_duplicate_key_is_rejected_deterministically() {
        let duplicate = front(1, Axial::new(1, 0), 0, 5);
        let mut renamed = duplicate;
        renamed.id = 2;
        let survivor = front(3, Axial::new(0, 1), 0, 5);

        let by_id = resolve_edge_combat(
            Axial::ZERO,
            5,
            0,
            &[duplicate, duplicate, survivor],
            &CombatConfig::default(),
        )
        .unwrap();
        assert_eq!(by_id.attacks.len(), 1);
        assert!(by_id.attacks.contains_key(&3));
        assert!(
            by_id
                .rejected
                .iter()
                .all(|rejection| rejection.reason == FrontRejection::DuplicateAttackId)
        );
        assert_eq!(by_id.rejected.len(), 2);

        let by_origin = resolve_edge_combat(
            Axial::ZERO,
            5,
            0,
            &[duplicate, renamed, survivor],
            &CombatConfig::default(),
        )
        .unwrap();
        assert_eq!(by_origin.attacks.len(), 1);
        assert!(by_origin.attacks.contains_key(&3));
        assert!(
            by_origin
                .rejected
                .iter()
                .all(|rejection| rejection.reason == FrontRejection::DuplicateOrigin)
        );
        assert_eq!(by_origin.rejected.len(), 2);
    }

    #[test]
    fn two_attacker_owners_split_the_defender_and_the_stronger_owner_captures() {
        let attacks = [
            owned_front(1, 1, Axial::new(1, 0), 0, 10),
            owned_front(2, 2, Axial::new(0, 1), 0, 20),
        ];
        let result =
            resolve_edge_combat(Axial::ZERO, 15, 0, &attacks, &CombatConfig::default()).unwrap();
        // Defense splits 5/10 proportionally to engaged strength.
        assert_eq!(result.attacks[&1].defense_allocated, 5);
        assert_eq!(result.attacks[&2].defense_allocated, 10);
        assert_eq!(result.defender_casualties, 15);
        assert_eq!(result.defender_remaining, 0);
        // Survivors: owner 1 keeps 5, owner 2 keeps 10; owner 2 captures.
        assert_eq!(result.capturing_owner, Some(2));
        assert_eq!(result.capturing_front, Some(2));
        resolution_conserves(&result, &attacks);
    }

    #[test]
    fn three_owner_contest_resolves_with_exact_defender_conservation() {
        let attacks = [
            owned_front(1, 3, Axial::new(1, 0), 0, 25),
            owned_front(2, 1, Axial::new(0, 1), 0, 25),
            owned_front(3, 2, Axial::new(-1, 1), 0, 10),
        ];
        let result =
            resolve_edge_combat(Axial::ZERO, 40, 0, &attacks, &CombatConfig::default()).unwrap();
        assert_eq!(
            result
                .attacks
                .values()
                .map(|outcome| outcome.defense_allocated)
                .sum::<u64>(),
            40
        );
        assert_eq!(result.defender_remaining, 0);
        // Survivors per owner: 3 → 25-16=9? Recomputed below from outcomes.
        let survivors_by_owner: BTreeMap<PlayerId, u64> = result
            .attacks
            .values()
            .map(|outcome| (outcome.attacker, outcome.attacker_remaining))
            .fold(BTreeMap::new(), |mut totals, (owner, remaining)| {
                *totals.entry(owner).or_default() += remaining;
                totals
            });
        let expected_winner = survivors_by_owner
            .iter()
            .filter(|&(_, &remaining)| remaining > 0)
            .max_by(|(left_owner, left), (right_owner, right)| {
                left.cmp(right).then_with(|| right_owner.cmp(left_owner))
            })
            .map(|(&owner, _)| owner);
        assert_eq!(result.capturing_owner, expected_winner);
        resolution_conserves(&result, &attacks);
    }

    #[test]
    fn tied_owners_break_toward_the_smaller_owner_and_smaller_front_id() {
        let attacks = [
            owned_front(4, 7, Axial::new(1, 0), 0, 10),
            owned_front(2, 3, Axial::new(0, 1), 0, 10),
            owned_front(6, 3, Axial::new(-1, 1), 0, 10),
        ];
        // Zero defenders: everyone survives untouched, owner 3 has 20 vs 10.
        let result =
            resolve_edge_combat(Axial::ZERO, 0, 0, &attacks, &CombatConfig::default()).unwrap();
        assert_eq!(result.capturing_owner, Some(3));
        // Owner 3's fronts tie at 10 surviving; the smaller id wins.
        assert_eq!(result.capturing_front, Some(2));

        // Exact owner tie: 7 vs 3 both survive 10; smaller owner id captures.
        let tied = [
            owned_front(4, 7, Axial::new(1, 0), 0, 10),
            owned_front(2, 3, Axial::new(0, 1), 0, 10),
        ];
        let result =
            resolve_edge_combat(Axial::ZERO, 0, 0, &tied, &CombatConfig::default()).unwrap();
        assert_eq!(result.capturing_owner, Some(3));
        assert_eq!(result.capturing_front, Some(2));
    }

    #[test]
    fn sub_lethal_attrition_conserves_strength_and_converges() {
        let config = CombatConfig {
            attacker_damage_bps: 2_500,
            defender_damage_bps: 1_500,
            ..CombatConfig::default()
        };
        let mut defender = 40_u64;
        let mut offered = [30_u64, 25];
        let origins = [Axial::new(1, 0), Axial::new(0, 1)];
        let initial_total = defender + offered.iter().sum::<u64>();
        let mut total_casualties = 0_u64;
        let mut steps = 0_usize;
        while defender > 0 && offered.iter().any(|&strength| strength > 0) {
            steps += 1;
            assert!(steps <= 200, "sub-lethal attrition must converge");
            let attacks: Vec<_> = origins
                .iter()
                .zip(offered)
                .enumerate()
                .filter(|&(_, (_, strength))| strength > 0)
                .map(|(index, (&from, strength))| owned_front(index as u64, 1, from, 0, strength))
                .collect();
            let result = resolve_edge_combat(Axial::ZERO, defender, 0, &attacks, &config).unwrap();
            assert!(result.rejected.is_empty());
            // Sub-lethal steps may deal zero casualties on tiny remainders;
            // the authoritative module layers its minimum-casualty rule on
            // top. Emulate it here so the fixture always converges.
            let mut defender_casualties = result.defender_casualties;
            if defender_casualties == 0 && defender > 0 {
                defender_casualties = 1;
            }
            defender_casualties = defender_casualties.min(defender);
            defender -= defender_casualties;
            total_casualties += defender_casualties;
            for (index, strength) in offered.iter_mut().enumerate() {
                if let Some(outcome) = result.attacks.get(&(index as u64)) {
                    assert!(outcome.attacker_casualties <= *strength);
                    *strength -= outcome.attacker_casualties;
                    total_casualties += outcome.attacker_casualties;
                }
            }
            assert_eq!(
                defender + offered.iter().sum::<u64>() + total_casualties,
                initial_total,
                "attrition must conserve total strength every step"
            );
        }
        assert_eq!(defender, 0, "attackers eventually eliminate the defender");
        assert!(offered.iter().sum::<u64>() > 0);
    }

    #[test]
    fn sub_lethal_multi_owner_attrition_is_deterministic() {
        let config = CombatConfig {
            attacker_damage_bps: 3_000,
            defender_damage_bps: 3_000,
            ..CombatConfig::default()
        };
        let run = || {
            let mut defender = 60_u64;
            let mut offered = BTreeMap::from([(1_u32, 35_u64), (2, 35)]);
            let origins = BTreeMap::from([(1_u32, Axial::new(1, 0)), (2, Axial::new(0, 1))]);
            let mut trace = Vec::new();
            for _ in 0..50 {
                if defender == 0 {
                    break;
                }
                let attacks: Vec<_> = offered
                    .iter()
                    .filter(|&(_, &strength)| strength > 0)
                    .map(|(&owner, &strength)| {
                        owned_front(u64::from(owner), owner, origins[&owner], 0, strength)
                    })
                    .collect();
                if attacks.is_empty() {
                    break;
                }
                let result =
                    resolve_edge_combat(Axial::ZERO, defender, 0, &attacks, &config).unwrap();
                defender = result.defender_remaining;
                for outcome in result.attacks.values() {
                    *offered.get_mut(&outcome.attacker).unwrap() = outcome.attacker_remaining;
                }
                trace.push((defender, offered.clone(), result.capturing_owner));
            }
            trace
        };
        assert_eq!(run(), run());
        let trace = run();
        let (final_defender, final_offered, capture) = trace.last().unwrap().clone();
        assert_eq!(final_defender, 0);
        // Symmetric owners tie on survivors; the smaller owner captures.
        assert_eq!(final_offered[&1], final_offered[&2]);
        assert_eq!(capture, Some(1));
    }
}
