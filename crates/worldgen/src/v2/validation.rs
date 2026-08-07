use std::collections::{BTreeMap, BTreeSet, VecDeque};

use hex_core::HexDirection;

use super::{GENERATOR_VERSION, LayeredWorld, Surface};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct V2ValidationReport {
    pub total_cells: usize,
    pub land_cells: usize,
    pub lake_cells: usize,
    pub river_cells: usize,
    pub water_bodies: usize,
    pub chunks: usize,
}

/// Validates layer compatibility, hydrology, spawns, connectivity, and chunks.
///
/// # Errors
///
/// Returns the first deterministic structural error found in the generated map.
pub fn validate(world: &LayeredWorld) -> Result<V2ValidationReport, String> {
    validate_manifest(world)?;
    validate_layers(world)?;
    validate_lakes(world)?;
    validate_rivers(world)?;
    validate_spawns(world)?;
    validate_land_connectivity(world)?;
    validate_chunks(world)?;

    let water_bodies = world
        .cells()
        .iter()
        .filter_map(|cell| cell.water_body_id)
        .collect::<BTreeSet<_>>()
        .len();
    Ok(V2ValidationReport {
        total_cells: world.cells().len(),
        land_cells: world
            .cells()
            .iter()
            .filter(|cell| cell.surface == Surface::Land)
            .count(),
        lake_cells: world
            .cells()
            .iter()
            .filter(|cell| cell.surface == Surface::Lake)
            .count(),
        river_cells: world
            .cells()
            .iter()
            .filter(|cell| cell.river.is_some())
            .count(),
        water_bodies,
        chunks: world.chunks_wide() as usize * world.chunks_high() as usize,
    })
}

fn validate_manifest(world: &LayeredWorld) -> Result<(), String> {
    if world.manifest.generator_version != GENERATOR_VERSION {
        return Err(format!(
            "unsupported layered generator version {}",
            world.manifest.generator_version
        ));
    }
    let expected = world.width() as usize * world.height() as usize;
    if world.cells().len() != expected {
        return Err("layered manifest dimensions do not match cell count".to_owned());
    }
    if world.manifest.pipeline.is_empty() {
        return Err("layered manifest has no pass provenance".to_owned());
    }
    let hash = super::pipeline::content_hash(world);
    if hash != world.manifest.content_hash {
        return Err(format!(
            "layered content hash mismatch: manifest {:016x}, computed {hash:016x}",
            world.manifest.content_hash
        ));
    }
    Ok(())
}

fn validate_layers(world: &LayeredWorld) -> Result<(), String> {
    for (cell_id, cell) in world.cells().iter().enumerate() {
        match cell.surface {
            Surface::Land => {
                if cell.water_body_id.is_some() {
                    return Err(format!("land cell {cell_id} belongs to a lake"));
                }
                if cell.gameplay.capturable && !cell.gameplay.passable {
                    return Err(format!("capturable land cell {cell_id} is impassable"));
                }
            }
            Surface::Ocean => {
                if cell.water_body_id.is_some() || cell.river.is_some() {
                    return Err(format!(
                        "ocean cell {cell_id} has inland hydrology metadata"
                    ));
                }
                if cell.gameplay.passable || cell.gameplay.capturable {
                    return Err(format!("ocean cell {cell_id} is playable land"));
                }
            }
            Surface::Lake => {
                if cell.water_body_id.is_none() || cell.river.is_some() {
                    return Err(format!(
                        "lake cell {cell_id} has inconsistent hydrology metadata"
                    ));
                }
                if cell.gameplay.passable || cell.gameplay.capturable {
                    return Err(format!("lake cell {cell_id} is playable land"));
                }
            }
        }
    }
    Ok(())
}

fn validate_lakes(world: &LayeredWorld) -> Result<(), String> {
    let mut cells_by_lake = BTreeMap::<u32, BTreeSet<u32>>::new();
    for (cell_id, cell) in world.cells().iter().enumerate() {
        if let Some(water_body) = cell.water_body_id {
            cells_by_lake
                .entry(water_body)
                .or_default()
                .insert(cell_id as u32);
        }
    }
    for (water_body, cells) in cells_by_lake {
        let Some(&seed) = cells.first() else {
            continue;
        };
        let mut reached = BTreeSet::from([seed]);
        let mut pending = VecDeque::from([seed]);
        while let Some(current) = pending.pop_front() {
            for direction in HexDirection::ALL {
                let Some(neighbor) = world.neighbor_id(current, direction) else {
                    continue;
                };
                if cells.contains(&neighbor) && reached.insert(neighbor) {
                    pending.push_back(neighbor);
                }
            }
        }
        if reached != cells {
            return Err(format!("lake {water_body} is disconnected"));
        }
    }
    Ok(())
}

fn validate_rivers(world: &LayeredWorld) -> Result<(), String> {
    for (cell_id, cell) in world.cells().iter().enumerate() {
        let Some(river) = cell.river else {
            continue;
        };
        if cell.surface != Surface::Land {
            return Err(format!("river cell {cell_id} is not land"));
        }
        let next = world
            .neighbor_id(cell_id as u32, river.outflow)
            .ok_or_else(|| format!("river cell {cell_id} flows outside the map"))?;
        let target = &world.cells()[next as usize];
        if target.surface == Surface::Land && target.river.is_none() {
            return Err(format!(
                "river cell {cell_id} terminates on ordinary land {next}"
            ));
        }
        if let Some(target_river) = target.river {
            let expected_bit = 1 << river.outflow.opposite().index();
            if target_river.inflow_mask & expected_bit == 0 {
                return Err(format!(
                    "river connection {cell_id}->{next} lacks reciprocal inflow"
                ));
            }
            if target_river.discharge < river.discharge {
                return Err(format!(
                    "river discharge decreases from {cell_id} to {next}"
                ));
            }
        }

        let mut visited = BTreeSet::new();
        let mut current = cell_id as u32;
        loop {
            if !visited.insert(current) {
                return Err(format!("river from {cell_id} contains a cycle"));
            }
            let current_cell = &world.cells()[current as usize];
            let Some(segment) = current_cell.river else {
                break;
            };
            let Some(next) = world.neighbor_id(current, segment.outflow) else {
                return Err(format!("river from {cell_id} leaves map bounds"));
            };
            if world.cells()[next as usize].surface != Surface::Land {
                break;
            }
            current = next;
        }
    }
    Ok(())
}

fn validate_spawns(world: &LayeredWorld) -> Result<(), String> {
    if world.manifest.spawn_cells.len() != usize::from(world.manifest.player_count) {
        return Err("layered spawn count does not match player count".to_owned());
    }
    let unique = world
        .manifest
        .spawn_cells
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if unique.len() != world.manifest.spawn_cells.len() {
        return Err("layered spawns are not distinct".to_owned());
    }
    for spawn in &world.manifest.spawn_cells {
        let cell = world
            .cell(*spawn)
            .ok_or_else(|| format!("spawn {spawn:?} is outside the map"))?;
        if !cell.gameplay.capturable || !cell.gameplay.habitable {
            return Err(format!("spawn {spawn:?} is not habitable capturable land"));
        }
    }
    Ok(())
}

fn validate_land_connectivity(world: &LayeredWorld) -> Result<(), String> {
    let Some(seed) = world
        .cells()
        .iter()
        .position(|cell| cell.gameplay.passable)
        .map(|id| id as u32)
    else {
        return Err("layered map has no passable land".to_owned());
    };
    let expected = world
        .cells()
        .iter()
        .filter(|cell| cell.gameplay.passable)
        .count();
    let mut reached = BTreeSet::from([seed]);
    let mut pending = VecDeque::from([seed]);
    while let Some(current) = pending.pop_front() {
        for direction in HexDirection::ALL {
            let Some(neighbor) = world.neighbor_id(current, direction) else {
                continue;
            };
            if world.cells()[neighbor as usize].gameplay.passable && reached.insert(neighbor) {
                pending.push_back(neighbor);
            }
        }
    }
    if reached.len() != expected {
        return Err(format!(
            "layered passable land has disconnected cells: reached {} of {expected}",
            reached.len()
        ));
    }
    Ok(())
}

fn validate_chunks(world: &LayeredWorld) -> Result<(), String> {
    let mut seen = vec![false; world.cells().len()];
    for chunk_r in 0..world.chunks_high() {
        for chunk_q in 0..world.chunks_wide() {
            let chunk = world
                .chunk(hex_core::ChunkCoord {
                    q: chunk_q as i32,
                    r: chunk_r as i32,
                })
                .ok_or_else(|| format!("missing terrain chunk {chunk_q},{chunk_r}"))?;
            for cell in chunk.cells {
                let slot = seen
                    .get_mut(cell.cell_id as usize)
                    .ok_or_else(|| format!("chunk contains invalid cell {}", cell.cell_id))?;
                if *slot {
                    return Err(format!("cell {} appears in multiple chunks", cell.cell_id));
                }
                *slot = true;
                if world.cell(cell.coordinate) != Some(&cell.layers) {
                    return Err(format!(
                        "chunk cell {} differs from its source layers",
                        cell.cell_id
                    ));
                }
            }
        }
    }
    if seen.iter().any(|value| !value) {
        return Err("chunk extraction does not cover every map cell".to_owned());
    }
    Ok(())
}
