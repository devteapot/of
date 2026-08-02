use std::collections::{BTreeMap, BTreeSet};

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
    DuplicateAttackId(u64),
    DuplicateOrigin(Axial),
    MixedAttackerOwners,
    NonAdjacent(Axial),
    ImpassableCliff(Axial),
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttackOutcome {
    pub id: u64,
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
    pub attacker: Option<PlayerId>,
    pub defender_initial: Strength,
    pub defender_casualties: Strength,
    pub defender_remaining: Strength,
    pub attacks: BTreeMap<u64, AttackOutcome>,
    /// The surviving front selected to occupy if the caller can admit it under
    /// throughput and destination-capacity rules.
    pub capturing_front: Option<u64>,
}

/// Resolves one deterministic combat step against a cell.
///
/// Every defender is allocated to at most one attack edge. Allocation is
/// proportional to engaged strength, capped by each edge's frontage, and uses
/// attack ID as the integer-remainder tie-break.
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

    let mut ids = BTreeSet::new();
    let mut origins = BTreeSet::new();
    let mut attacker = None;
    for attack in attacks {
        if !ids.insert(attack.id) {
            return Err(CombatError::DuplicateAttackId(attack.id));
        }
        if !origins.insert(attack.from) {
            return Err(CombatError::DuplicateOrigin(attack.from));
        }
        if target.distance(attack.from) != 1 {
            return Err(CombatError::NonAdjacent(attack.from));
        }
        let elevation_delta = i32::from(defender_elevation) - i32::from(attack.from_elevation);
        if elevation_delta.unsigned_abs() > u32::from(config.max_elevation_step) {
            return Err(CombatError::ImpassableCliff(attack.from));
        }
        match attacker {
            None => attacker = Some(attack.attacker),
            Some(existing) if existing != attack.attacker => {
                return Err(CombatError::MixedAttackerOwners);
            }
            Some(_) => {}
        }
    }

    let mut sorted = attacks.to_vec();
    sorted.sort_unstable_by_key(|attack| attack.id);
    let demands: Vec<_> = sorted
        .iter()
        .map(|attack| (attack.id, attack.offered.min(attack.frontage)))
        .collect();
    let allocations = proportional_allocation(defender_strength, &demands);

    let mut outcomes = BTreeMap::new();
    let mut total_defender_casualties = 0_u64;
    for attack in sorted {
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
    let capturing_front = (defender_remaining == 0)
        .then(|| {
            outcomes
                .values()
                .filter(|outcome| outcome.attacker_remaining > 0)
                .max_by(|left, right| {
                    left.attacker_remaining
                        .cmp(&right.attacker_remaining)
                        .then_with(|| right.id.cmp(&left.id))
                })
                .map(|outcome| outcome.id)
        })
        .flatten();

    Ok(CombatResolution {
        attacker,
        defender_initial: defender_strength,
        defender_casualties: total_defender_casualties,
        defender_remaining,
        attacks: outcomes,
        capturing_front,
    })
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
        AttackFront {
            id,
            attacker: 1,
            from,
            from_elevation: elevation,
            offered,
            frontage: 25,
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
        assert_eq!(result.capturing_front, Some(1));
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
    fn invalid_edges_and_duplicate_fronts_are_rejected() {
        let duplicate = front(1, Axial::new(1, 0), 0, 5);
        assert_eq!(
            resolve_edge_combat(
                Axial::ZERO,
                5,
                0,
                &[duplicate, duplicate],
                &CombatConfig::default()
            ),
            Err(CombatError::DuplicateAttackId(1))
        );
        assert_eq!(
            resolve_edge_combat(
                Axial::ZERO,
                5,
                0,
                &[front(1, Axial::new(2, 0), 0, 5)],
                &CombatConfig::default()
            ),
            Err(CombatError::NonAdjacent(Axial::new(2, 0)))
        );
        assert_eq!(
            resolve_edge_combat(
                Axial::ZERO,
                5,
                2,
                &[front(1, Axial::new(1, 0), 0, 5)],
                &CombatConfig::default()
            ),
            Err(CombatError::ImpassableCliff(Axial::new(1, 0)))
        );
    }
}
