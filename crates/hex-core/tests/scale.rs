//! Larger-map kernel guard: correctness and exact conservation at 64x64.
//!
//! These tests primarily assert behavior at scale (conservation, capacity
//! safety, determinism). They deliberately avoid timing assertions so CI
//! machine variance cannot flake them; a pathological slowdown still surfaces
//! as a test-suite timeout.

use hex_core::{
    Axial, Cell, ForceComposition, HexMap, LogisticsConfig, MovementConfig, MovementIntent,
    TransferRequest, movement_step, plan_transfer, redistribution_targets_dense_with_weights,
};

const SIZE: i32 = 64;
const CAPACITY: u64 = 100;

/// Deterministic LCG so the fixture is broad without an RNG dependency.
fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state
}

/// A fully owned 64x64 map with gentle one-step elevation bands (never a
/// cliff) and pseudo-random garrisons within capacity.
fn build_map(seed: u64) -> HexMap {
    let mut state = seed;
    let mut map = HexMap::new();
    for q in 0..SIZE {
        for r in 0..SIZE {
            let elevation = ((q / 8 + r / 8) % 2) as i16;
            let mut cell = Cell::ground(Axial::new(q, r), elevation, Some(1), CAPACITY);
            cell.forces = ForceComposition::infantry(lcg(&mut state) % (CAPACITY + 1));
            map.insert(cell);
        }
    }
    map
}

#[test]
fn movement_step_conserves_strength_and_capacity_across_ticks_at_scale() {
    let mut map = build_map(0x5eed_0001);
    let movement = MovementConfig::default();
    let logistics = LogisticsConfig::default();
    let initial_total = map.total_force();
    let mut state = 0x5eed_0002_u64;

    for tick in 0..10_u64 {
        let mut intents = Vec::new();
        for q in 0..SIZE {
            for r in 0..SIZE {
                let from = Axial::new(q, r);
                // Alternate eastward and south-eastward waves across ticks.
                let to = if tick % 2 == 0 {
                    Axial::new(q + 1, r)
                } else {
                    Axial::new(q, r + 1)
                };
                intents.push(MovementIntent {
                    id: intents.len() as u64 + 1,
                    priority: (lcg(&mut state) % 4) as u32,
                    owner: 1,
                    from,
                    to,
                    requested: lcg(&mut state) % 40,
                });
            }
        }
        let step = movement_step(&mut map, &intents, &movement, &logistics)
            .expect("large-map movement step succeeds");
        assert_eq!(step.strength_before, step.strength_after, "tick {tick}");
        assert_eq!(map.total_force(), initial_total, "tick {tick}");
        assert!(
            map.cells()
                .all(|cell| cell.force() <= cell.military_capacity),
            "tick {tick} violated a capacity"
        );
    }
}

#[test]
fn plan_transfer_routes_column_to_column_deterministically_at_scale() {
    let map = build_map(0x5eed_0003);
    let sources: Vec<Axial> = (0..SIZE).map(|r| Axial::new(0, r)).collect();
    let destinations: Vec<Axial> = (0..SIZE).map(|r| Axial::new(SIZE - 1, r)).collect();
    let source_total: u64 = sources.iter().map(|&c| map.get(c).unwrap().force()).sum();
    let free_total: u64 = destinations
        .iter()
        .map(|&c| map.get(c).unwrap().free_military_capacity())
        .sum();
    let request = TransferRequest {
        owner: 1,
        sources,
        destinations,
        amount: 100_000,
    };
    let movement = MovementConfig::default();

    let plan = plan_transfer(&map, &request, &movement).expect("large-map plan succeeds");
    // The component is fully connected, so the plan saturates the binding
    // constraint exactly.
    assert_eq!(
        plan.planned,
        request.amount.min(source_total).min(free_total)
    );
    assert_eq!(plan.planned + plan.unplanned, plan.requested);
    assert_eq!(
        plan.legs.iter().map(|leg| leg.amount).sum::<u64>(),
        plan.planned
    );
    assert!(plan.unreachable_sources.is_empty());
    assert!(plan.unreachable_destinations.is_empty());
    for leg in &plan.legs {
        assert_eq!(leg.path.cells.first(), Some(&leg.source));
        assert_eq!(leg.path.cells.last(), Some(&leg.destination));
        for pair in leg.path.cells.windows(2) {
            assert_eq!(pair[0].distance(pair[1]), 1);
        }
    }

    let repeat = plan_transfer(&map, &request, &movement).expect("plan repeats");
    assert_eq!(plan, repeat, "planning must be deterministic");
}

#[test]
fn dense_redistribution_is_exact_and_capacity_safe_at_scale() {
    let map = build_map(0x5eed_0004);
    let coordinates: Vec<Axial> = map.coordinates().collect();
    let capacities: Vec<u64> = coordinates
        .iter()
        .map(|&c| map.get(c).unwrap().military_capacity)
        .collect();
    let mut state = 0x5eed_0005_u64;
    let weights: Vec<u32> = coordinates
        .iter()
        .map(|_| (lcg(&mut state) % 7 + 1) as u32)
        .collect();
    let total_strength = map.total_force();

    let distribution = redistribution_targets_dense_with_weights(
        &coordinates,
        &capacities,
        total_strength,
        weights,
    )
    .expect("large-map redistribution succeeds");
    assert_eq!(
        distribution.assigned + distribution.unassigned,
        total_strength
    );
    assert_eq!(
        distribution.targets.iter().sum::<u64>(),
        distribution.assigned
    );
    for (target, capacity) in distribution.targets.iter().zip(&capacities) {
        assert!(target <= capacity);
    }
}
