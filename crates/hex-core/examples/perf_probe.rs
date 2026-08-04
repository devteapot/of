//! Scratch profiling probe for pure hex-core kernels at realistic V1 scales.
//! Not part of the shipping build: `cargo run -p hex-core --release --example perf_probe`.

use std::collections::BTreeSet;
use std::time::Instant;

use hex_core::{
    Axial, Cell, DistributionPreset, HexMap, LogisticsConfig, MovementConfig, MovementIntent,
    redistribution_targets, shortest_path,
};

/// Builds a solid hex disk of owned cells: a realistic "one big cluster".
fn disk_map(radius: i32, capacity: u64, fill_bps: u64) -> HexMap {
    let mut map = HexMap::new();
    for q in -radius..=radius {
        for r in -radius..=radius {
            let coordinate = Axial::new(q, r);
            if Axial::ZERO.distance(coordinate) <= radius as u64 {
                let mut cell = Cell::ground(coordinate, 0, Some(1), capacity);
                cell.forces.infantry = capacity * fill_bps / 10_000;
                map.insert(cell);
            }
        }
    }
    map
}

fn bench_shortest_path() {
    let movement = MovementConfig::default();
    for radius in [10_i32, 25, 60] {
        let map = disk_map(radius, 100, 5_000);
        let cells = map.coordinates().len();
        // Worst case for one pair: opposite ends of the disk.
        let start = Axial::new(-radius, radius);
        let goal = Axial::new(radius, -radius);
        let t = Instant::now();
        let path = shortest_path(&map, start, goal, &movement, |cell| cell.owner == Some(1));
        let single = t.elapsed();
        assert!(path.is_some());

        // All-pairs perimeter→center like plan_transfer's S×D fan-out, sampled.
        let perimeter: Vec<_> = map
            .coordinates()
            .filter(|c| c.distance(Axial::ZERO) == radius as u64)
            .take(24)
            .collect();
        let t = Instant::now();
        let mut found = 0;
        for source in &perimeter {
            for goal in &perimeter {
                if shortest_path(&map, *source, *goal, &movement, |cell| {
                    cell.owner == Some(1)
                })
                .is_some()
                {
                    found += 1;
                }
            }
        }
        let pairs = perimeter.len() * perimeter.len();
        let batch = t.elapsed();
        println!(
            "shortest_path  radius={radius:>3} cells={cells:>6}  single={single:>9.2?}  \
             {pairs} pairs={batch:>8.2?} ({:.2?}/pair, found={found})",
            batch / pairs as u32
        );
    }
}

fn bench_redistribution() {
    for radius in [5_i32, 10, 20, 40, 60] {
        let map = disk_map(radius, 100, 8_000);
        let cells = map.coordinates().len();
        let total = map.total_force();
        for preset in [
            DistributionPreset::Balance,
            DistributionPreset::CoreLoad,
            DistributionPreset::front_load(Axial::new(1, 0)),
        ] {
            let t = Instant::now();
            let result = redistribution_targets(&map, 1, map.coordinates(), total, preset).unwrap();
            let elapsed = t.elapsed();
            assert_eq!(result.assigned, total);
            println!(
                "redistribution radius={radius:>3} cells={cells:>6}  {preset:?} => {elapsed:>9.2?}"
            );
        }
    }
}

fn bench_movement_step() {
    let movement = MovementConfig::default();
    let logistics = LogisticsConfig {
        default_military_capacity: 100,
        default_edge_throughput: 25,
        default_combat_frontage: 25,
    };
    for (radius, intent_count) in [(30_i32, 500_u32), (30, 2_000), (30, 8_000)] {
        let map_cells = disk_map(radius, 100, 5_000);
        // Pipeline intents along +q: each occupied cell pushes into its neighbor.
        let coords: Vec<_> = map_cells.coordinates().collect();
        let coord_set: BTreeSet<_> = coords.iter().copied().collect();
        let mut intents = Vec::new();
        let mut id = 1_u64;
        'outer: for &from in &coords {
            for direction in Axial::DIRECTIONS {
                let to = from + direction;
                if coord_set.contains(&to) {
                    intents.push(MovementIntent {
                        id,
                        priority: 0,
                        owner: 1,
                        from,
                        to,
                        requested: 10,
                    });
                    id += 1;
                    if intents.len() >= intent_count as usize {
                        break 'outer;
                    }
                }
            }
        }
        let mut map = map_cells;
        let t = Instant::now();
        let result = hex_core::movement_step(&mut map, &intents, &movement, &logistics).unwrap();
        let elapsed = t.elapsed();
        println!(
            "movement_step  cells={:>6} intents={:>5} moved={:>7} => {elapsed:>9.2?}",
            map.cells().len(),
            intents.len(),
            result.moved_total
        );
    }
}

fn bench_commitment() {
    for radius in [10_i32, 20, 40, 60] {
        let map = disk_map(radius, 100, 8_000);
        let cells = map.coordinates().len();
        let total = map.total_force();
        let t = Instant::now();
        let result = hex_core::redistribution_targets_with_commitment(
            &map,
            1,
            map.coordinates(),
            total,
            DistributionPreset::CoreLoad,
            5_000,
        )
        .unwrap();
        let elapsed = t.elapsed();
        assert_eq!(result.assigned + result.unassigned, total);
        println!("commitment50  radius={radius:>3} cells={cells:>6} => {elapsed:>9.2?}");
    }
}

fn main() {
    println!("== shortest_path (deterministic A*) ==");
    bench_shortest_path();
    println!("== redistribution_targets ==");
    bench_redistribution();
    println!("== movement_step ==");
    bench_movement_step();
    println!("== redistribution_targets_with_commitment (CoreLoad, 50%) ==");
    bench_commitment();
}
