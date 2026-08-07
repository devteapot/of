use std::{
    cmp::Reverse,
    collections::{BTreeSet, BinaryHeap, VecDeque},
};

use hex_core::{Axial, HexDirection};

use super::{
    Biome, GameplayCell, Landform, LayeredWorld, RiverCell, Surface, TerrainTags, WorldLayer,
    noise::{fractal_noise, mix},
    pipeline::{ElevationWrite, PassReport, WorldPass, WorldPatch},
};

pub struct ContinentPass;
pub struct CoastConnectivityPass;
pub struct MountainPass;
pub struct LakeBasinPass;
pub struct HydrologyPass;
pub struct LandformPass;
pub struct BiomePass;
pub struct GameplayPass;
pub struct SpawnPass;

fn report(changed_cells: usize, notes: impl IntoIterator<Item = String>) -> PassReport {
    PassReport {
        changed_cells,
        notes: notes.into_iter().collect(),
        ..PassReport::default()
    }
}

impl WorldPass for ContinentPass {
    fn name(&self) -> &'static str {
        "continent"
    }

    fn writes(&self) -> &'static [WorldLayer] {
        &[WorldLayer::Elevation, WorldLayer::Surface, WorldLayer::Tags]
    }

    fn run(&self, world: &LayeredWorld, seed: u64) -> Result<WorldPatch, String> {
        let width = world.width();
        let height = world.height();
        let base_scale = u32::from(world.manifest.macro_cell_size).saturating_mul(4);
        let mut elevations = Vec::with_capacity(world.cells().len());
        let mut surfaces = Vec::with_capacity(world.cells().len());
        let mut land_mask = vec![false; world.cells().len()];
        for row in 0..height {
            for column in 0..width {
                let id = row * width + column;
                let x = (i64::from(column) * 2 + 1) * 1_024 / i64::from(width) - 1_024;
                let y = (i64::from(row) * 2 + 1) * 1_024 / i64::from(height) - 1_024;
                let radial = (x * x + y * y + x * y / 3) / 1_024;
                let noise = i64::from(fractal_noise(seed, column, row, base_scale));
                let elevation = 440_i64 - radial / 2 + noise * 9 / 32;
                let elevation = i16::try_from(elevation.clamp(-1_024, 1_024))
                    .map_err(|_| "continent elevation overflow".to_owned())?;
                let surface = if elevation > 0 {
                    Surface::Land
                } else {
                    Surface::Ocean
                };
                land_mask[id as usize] = surface == Surface::Land;
                elevations.push((id, ElevationWrite::Set(elevation)));
                surfaces.push((id, surface));
            }
        }

        let mut tags = Vec::new();
        for id in 0..world.cells().len() as u32 {
            if !land_mask[id as usize] {
                continue;
            }
            if HexDirection::ALL.into_iter().any(|direction| {
                world
                    .neighbor_id(id, direction)
                    .is_some_and(|neighbor| !land_mask[neighbor as usize])
            }) {
                tags.push((id, TerrainTags(TerrainTags::COAST)));
            }
        }
        let land = land_mask.iter().filter(|value| **value).count();
        Ok(WorldPatch {
            elevation: elevations,
            surfaces,
            tags,
            report: report(
                world.cells().len(),
                [format!(
                    "created {land} land cells with interpolated multi-scale coast noise"
                )],
            ),
            ..WorldPatch::default()
        })
    }
}

impl WorldPass for CoastConnectivityPass {
    fn name(&self) -> &'static str {
        "coast-connectivity"
    }

    fn reads(&self) -> &'static [WorldLayer] {
        &[WorldLayer::Elevation, WorldLayer::Surface]
    }

    fn writes(&self) -> &'static [WorldLayer] {
        &[WorldLayer::Elevation, WorldLayer::Surface]
    }

    #[allow(clippy::too_many_lines)]
    fn run(&self, world: &LayeredWorld, _seed: u64) -> Result<WorldPatch, String> {
        let count = world.cells().len();
        let mut labels = vec![usize::MAX; count];
        let mut components = Vec::<Vec<u32>>::new();
        for seed in 0..count as u32 {
            if labels[seed as usize] != usize::MAX
                || world.cells()[seed as usize].surface != Surface::Land
            {
                continue;
            }
            let label = components.len();
            labels[seed as usize] = label;
            let mut cells = Vec::new();
            let mut pending = VecDeque::from([seed]);
            while let Some(current) = pending.pop_front() {
                cells.push(current);
                for direction in HexDirection::ALL {
                    let Some(neighbor) = world.neighbor_id(current, direction) else {
                        continue;
                    };
                    if labels[neighbor as usize] == usize::MAX
                        && world.cells()[neighbor as usize].surface == Surface::Land
                    {
                        labels[neighbor as usize] = label;
                        pending.push_back(neighbor);
                    }
                }
            }
            components.push(cells);
        }
        if components.len() <= 1 {
            return Ok(WorldPatch {
                report: report(0, ["coast already forms one connected landmass".to_owned()]),
                ..WorldPatch::default()
            });
        }
        let main = components
            .iter()
            .enumerate()
            .max_by_key(|(_, cells)| cells.len())
            .map(|(index, _)| index)
            .ok_or_else(|| "coast connectivity has no land component".to_owned())?;

        // One multi-source search finds the shortest ocean route from every
        // island to the main continent. Short gaps become land bridges; remote
        // specks are returned to ocean instead of producing implausible causeways.
        let mut previous = vec![None; count];
        let mut distance = vec![u32::MAX; count];
        let mut pending = VecDeque::new();
        for &cell_id in &components[main] {
            distance[cell_id as usize] = 0;
            pending.push_back(cell_id);
        }
        let mut endpoints = vec![None; components.len()];
        while let Some(current) = pending.pop_front() {
            let current_distance = distance[current as usize];
            let label = labels[current as usize];
            if label != usize::MAX && label != main && endpoints[label].is_none() {
                endpoints[label] = Some(current);
            }
            for direction in HexDirection::ALL {
                let Some(neighbor) = world.neighbor_id(current, direction) else {
                    continue;
                };
                if distance[neighbor as usize] != u32::MAX {
                    continue;
                }
                distance[neighbor as usize] = current_distance.saturating_add(1);
                previous[neighbor as usize] = Some(current);
                pending.push_back(neighbor);
            }
        }

        let maximum_bridge = u32::from(world.manifest.macro_cell_size).max(4);
        let mut bridge_cells = BTreeSet::new();
        let mut discarded = BTreeSet::new();
        let mut connected_components = 0;
        for (label, component) in components.iter().enumerate() {
            if label == main {
                continue;
            }
            let Some(mut current) = endpoints[label] else {
                discarded.extend(component.iter().copied());
                continue;
            };
            if distance[current as usize] > maximum_bridge {
                discarded.extend(component.iter().copied());
                continue;
            }
            connected_components += 1;
            while labels[current as usize] != main {
                if world.cells()[current as usize].surface != Surface::Land {
                    bridge_cells.insert(current);
                }
                let Some(next) = previous[current as usize] else {
                    break;
                };
                current = next;
            }
        }
        let mut surfaces = bridge_cells
            .iter()
            .copied()
            .map(|id| (id, Surface::Land))
            .collect::<Vec<_>>();
        surfaces.extend(discarded.iter().copied().map(|id| (id, Surface::Ocean)));
        let elevation = bridge_cells
            .iter()
            .copied()
            .map(|id| (id, ElevationWrite::Set(1)))
            .collect::<Vec<_>>();
        Ok(WorldPatch {
            report: report(
                surfaces.len(),
                [format!(
                    "connected {connected_components} coastal components with {} bridge cells; discarded {} remote island cells",
                    bridge_cells.len(),
                    discarded.len()
                )],
            ),
            elevation,
            surfaces,
            ..WorldPatch::default()
        })
    }
}

#[derive(Clone, Copy)]
struct Ridge {
    first: (f64, f64),
    second: (f64, f64),
    radius: f64,
    height: i16,
}

impl WorldPass for MountainPass {
    fn name(&self) -> &'static str {
        "mountains"
    }

    fn reads(&self) -> &'static [WorldLayer] {
        &[WorldLayer::Elevation, WorldLayer::Surface]
    }

    fn writes(&self) -> &'static [WorldLayer] {
        &[WorldLayer::Elevation]
    }

    fn run(&self, world: &LayeredWorld, seed: u64) -> Result<WorldPatch, String> {
        let area = u64::from(world.width()) * u64::from(world.height());
        let density = u64::from(world.manifest.parameters.mountain_density_bps);
        let range_count =
            usize::try_from((area * density / 10_000 / (256 * 256)).clamp(2, 12)).unwrap_or(2);
        let short = f64::from(world.width().min(world.height()));
        let mut ridges = Vec::with_capacity(range_count);
        for index in 0..range_count {
            let hash = mix(seed, index as i32, 0);
            let center_x =
                f64::from(world.width()) * (0.20 + (hash & 0xffff) as f64 / 65_535.0 * 0.60);
            let center_y = f64::from(world.height())
                * (0.20 + ((hash >> 16) & 0xffff) as f64 / 65_535.0 * 0.60);
            let angle = ((hash >> 32) % 6) as f64 * std::f64::consts::PI / 3.0;
            let half_length = short * (0.08 + ((hash >> 40) & 0xff) as f64 / 255.0 * 0.11);
            let dx = angle.cos() * half_length;
            let dy = angle.sin() * half_length;
            ridges.push(Ridge {
                first: (center_x - dx, center_y - dy),
                second: (center_x + dx, center_y + dy),
                radius: (short / 34.0).max(3.0),
                height: 310 + i16::try_from((hash >> 48) % 180).unwrap_or_default(),
            });
        }

        let mut elevation = Vec::new();
        for id in 0..world.cells().len() as u32 {
            let cell = &world.cells()[id as usize];
            if cell.surface != Surface::Land {
                continue;
            }
            let column = f64::from(id % world.width());
            let row = f64::from(id / world.width());
            let uplift = ridges.iter().fold(0_i16, |largest, ridge| {
                let distance = point_segment_distance((column, row), ridge.first, ridge.second);
                if distance >= ridge.radius {
                    return largest;
                }
                let profile = 1.0 - distance / ridge.radius;
                largest.max((f64::from(ridge.height) * profile * profile).round() as i16)
            });
            if uplift > 0 {
                elevation.push((id, ElevationWrite::Add(uplift)));
            }
        }
        Ok(WorldPatch {
            report: report(
                elevation.len(),
                [format!(
                    "composed {range_count} deterministic mountain ranges"
                )],
            ),
            elevation,
            ..WorldPatch::default()
        })
    }
}

fn point_segment_distance(point: (f64, f64), first: (f64, f64), second: (f64, f64)) -> f64 {
    let segment = (second.0 - first.0, second.1 - first.1);
    let length_squared = segment.0 * segment.0 + segment.1 * segment.1;
    let amount = if length_squared == 0.0 {
        0.0
    } else {
        (((point.0 - first.0) * segment.0 + (point.1 - first.1) * segment.1) / length_squared)
            .clamp(0.0, 1.0)
    };
    let nearest = (first.0 + segment.0 * amount, first.1 + segment.1 * amount);
    (point.0 - nearest.0).hypot(point.1 - nearest.1)
}

impl WorldPass for LakeBasinPass {
    fn name(&self) -> &'static str {
        "lake-basins"
    }

    fn reads(&self) -> &'static [WorldLayer] {
        &[WorldLayer::Elevation, WorldLayer::Surface]
    }

    fn writes(&self) -> &'static [WorldLayer] {
        &[WorldLayer::Elevation]
    }

    fn run(&self, world: &LayeredWorld, seed: u64) -> Result<WorldPatch, String> {
        let area = u64::from(world.width()) * u64::from(world.height());
        let basin_count = usize::try_from((area / (384 * 384)).clamp(1, 6)).unwrap_or(1);
        let short = f64::from(world.width().min(world.height()));
        let radius = (short / 38.0).max(4.0);
        let land = (0..world.cells().len() as u32)
            .filter(|id| world.cells()[*id as usize].surface == Surface::Land)
            .collect::<Vec<_>>();
        if land.is_empty() {
            return Err("lake-basin pass has no land".to_owned());
        }
        let mut centers = Vec::with_capacity(basin_count);
        for index in 0..basin_count {
            let hash = mix(seed, index as i32, 71);
            let desired_column = world.width() / 4
                + u32::try_from(hash % u64::from((world.width() / 2).max(1))).unwrap_or_default();
            let desired_row = world.height() / 4
                + u32::try_from((hash >> 32) % u64::from((world.height() / 2).max(1)))
                    .unwrap_or_default();
            let desired = Axial::new(
                world.manifest.q_min + desired_column as i32,
                world.manifest.r_min + desired_row as i32,
            );
            let center = land
                .iter()
                .copied()
                .min_by_key(|cell_id| {
                    let coordinate = world.coordinate(*cell_id).expect("land cell in bounds");
                    (coordinate.distance(desired), coordinate)
                })
                .expect("land is nonempty");
            centers.push(center);
        }

        let mut elevation = Vec::new();
        for cell_id in land {
            let coordinate = world.coordinate(cell_id).expect("land cell in bounds");
            let deepest = centers.iter().fold(0_i16, |current, center| {
                let center = world.coordinate(*center).expect("basin center in bounds");
                let distance = coordinate.distance(center) as f64;
                if distance >= radius {
                    current
                } else {
                    let profile = 1.0 - distance / radius;
                    current.max((150.0 * profile * profile).round() as i16)
                }
            });
            if deepest > 0 {
                elevation.push((cell_id, ElevationWrite::Add(-deepest)));
            }
        }
        Ok(WorldPatch {
            report: report(
                elevation.len(),
                [format!(
                    "carved {basin_count} deterministic hydrology basins"
                )],
            ),
            elevation,
            ..WorldPatch::default()
        })
    }
}

impl WorldPass for HydrologyPass {
    fn name(&self) -> &'static str {
        "hydrology"
    }

    fn reads(&self) -> &'static [WorldLayer] {
        &[WorldLayer::Elevation, WorldLayer::Surface]
    }

    fn writes(&self) -> &'static [WorldLayer] {
        &[
            WorldLayer::Surface,
            WorldLayer::Moisture,
            WorldLayer::Hydrology,
            WorldLayer::Tags,
        ]
    }

    #[allow(clippy::too_many_lines)]
    fn run(&self, world: &LayeredWorld, seed: u64) -> Result<WorldPatch, String> {
        let count = world.cells().len();
        let mut filled = world
            .cells()
            .iter()
            .map(|cell| cell.elevation)
            .collect::<Vec<_>>();
        let mut visited = vec![false; count];
        let mut frontier = BinaryHeap::new();
        for (id, cell) in world.cells().iter().enumerate() {
            if cell.surface == Surface::Ocean {
                visited[id] = true;
                frontier.push(Reverse((cell.elevation, id as u32)));
            }
        }
        while let Some(Reverse((height, cell_id))) = frontier.pop() {
            for direction in HexDirection::ALL {
                let Some(neighbor) = world.neighbor_id(cell_id, direction) else {
                    continue;
                };
                if visited[neighbor as usize] {
                    continue;
                }
                visited[neighbor as usize] = true;
                filled[neighbor as usize] = filled[neighbor as usize].max(height);
                frontier.push(Reverse((filled[neighbor as usize], neighbor)));
            }
        }

        let lake_depth = world.manifest.parameters.lake_depth_threshold.max(1);
        let mut lake_mask = vec![false; count];
        for id in 0..count {
            let cell = &world.cells()[id];
            lake_mask[id] = cell.surface == Surface::Land
                && filled[id].saturating_sub(cell.elevation) >= lake_depth;
        }
        let (water_bodies, lake_count) = label_lakes(world, &lake_mask);

        let mut downstream = vec![None; count];
        for cell_id in 0..count as u32 {
            if world.cells()[cell_id as usize].surface != Surface::Land
                || lake_mask[cell_id as usize]
            {
                continue;
            }
            let current_key = (filled[cell_id as usize], cell_id);
            downstream[cell_id as usize] = HexDirection::ALL
                .into_iter()
                .filter_map(|direction| {
                    let neighbor = world.neighbor_id(cell_id, direction)?;
                    let neighbor_cell = &world.cells()[neighbor as usize];
                    let terminal =
                        neighbor_cell.surface == Surface::Ocean || lake_mask[neighbor as usize];
                    let key = (filled[neighbor as usize], neighbor);
                    (terminal || key < current_key).then_some((terminal, key, direction, neighbor))
                })
                .min_by_key(|(terminal, key, _, _)| (!*terminal, *key))
                .map(|(_, _, direction, neighbor)| (direction, neighbor));
        }

        let mut discharge = (0..count)
            .map(|id| 1 + u32::try_from(mix(seed, id as i32, 17) % 4).unwrap_or_default())
            .collect::<Vec<_>>();
        let mut drainage_order = (0..count as u32).collect::<Vec<_>>();
        drainage_order.sort_unstable_by_key(|id| Reverse((filled[*id as usize], *id)));
        for &cell_id in &drainage_order {
            if let Some((_, next)) = downstream[cell_id as usize] {
                discharge[next as usize] =
                    discharge[next as usize].saturating_add(discharge[cell_id as usize]);
            }
        }

        // Shallow unresolved sinks are allowed to remain ordinary damp land,
        // but they must not create rivers that disappear before reaching a
        // lake or ocean. The flow graph is acyclic by `(filled_height, id)`.
        let mut drains_to_water = vec![false; count];
        for &cell_id in drainage_order.iter().rev() {
            let Some((_, next)) = downstream[cell_id as usize] else {
                continue;
            };
            drains_to_water[cell_id as usize] = world.cells()[next as usize].surface
                == Surface::Ocean
                || lake_mask[next as usize]
                || drains_to_water[next as usize];
        }

        // Keep river density approximately proportional as worlds grow. A
        // threshold based only on a tiny fixed accumulation turns half of a
        // million-cell continent into channels; one source-area per ~512 map
        // cells retains a sparse, readable global network.
        let automatic_threshold = (count as u32 / 512)
            .max(world.width().min(world.height()).saturating_mul(2))
            .max(64);
        let threshold = if world.manifest.parameters.river_threshold == 0 {
            automatic_threshold
        } else {
            world.manifest.parameters.river_threshold
        };
        let mut rivers = vec![None; count];
        for cell_id in 0..count as u32 {
            let Some((outflow, _)) = downstream[cell_id as usize] else {
                continue;
            };
            if discharge[cell_id as usize] < threshold || !drains_to_water[cell_id as usize] {
                continue;
            }
            let relative = discharge[cell_id as usize] / threshold.max(1);
            rivers[cell_id as usize] = Some(RiverCell {
                outflow,
                inflow_mask: 0,
                order: u8::try_from(relative.ilog2() + 1).unwrap_or(u8::MAX),
                discharge: discharge[cell_id as usize],
            });
        }
        for cell_id in 0..count as u32 {
            let Some(river) = rivers[cell_id as usize] else {
                continue;
            };
            let Some((_, next)) = downstream[cell_id as usize] else {
                continue;
            };
            if let Some(next_river) = &mut rivers[next as usize] {
                next_river.inflow_mask |= 1 << river.outflow.opposite().index();
            }
        }

        let mut surfaces = Vec::new();
        let mut water_body_writes = Vec::new();
        let mut river_writes = Vec::new();
        let mut moisture = Vec::with_capacity(count);
        let mut tags = Vec::new();
        for cell_id in 0..count as u32 {
            if lake_mask[cell_id as usize] {
                surfaces.push((cell_id, Surface::Lake));
                water_body_writes.push((cell_id, water_bodies[cell_id as usize]));
            }
            if let Some(river) = rivers[cell_id as usize] {
                river_writes.push((cell_id, Some(river)));
                let mut flags = TerrainTags::RIVERBANK;
                if river.inflow_mask == 0 {
                    flags |= TerrainTags::SOURCE;
                }
                if let Some((_, next)) = downstream[cell_id as usize]
                    && (world.cells()[next as usize].surface == Surface::Ocean
                        || lake_mask[next as usize])
                {
                    flags |= TerrainTags::OUTLET;
                }
                tags.push((cell_id, TerrainTags(flags)));
            }
            let column = cell_id % world.width();
            let row = cell_id / world.width();
            let base = 92 + fractal_noise(seed ^ 0x4D4F_4953_5455_5245, column, row, 96) / 16;
            let adjacent_water = HexDirection::ALL.into_iter().any(|direction| {
                world
                    .neighbor_id(cell_id, direction)
                    .is_some_and(|neighbor| {
                        lake_mask[neighbor as usize]
                            || world.cells()[neighbor as usize].surface == Surface::Ocean
                            || rivers[neighbor as usize].is_some()
                    })
            });
            let value = if lake_mask[cell_id as usize] || rivers[cell_id as usize].is_some() {
                255
            } else if adjacent_water {
                base.max(190)
            } else {
                base
            };
            moisture.push((
                cell_id,
                u8::try_from(value.clamp(0, 255)).unwrap_or_default(),
            ));
        }
        let river_count = river_writes.len();
        Ok(WorldPatch {
            surfaces,
            water_bodies: water_body_writes,
            rivers: river_writes,
            moisture,
            tags,
            report: report(
                lake_mask.iter().filter(|value| **value).count() + river_count,
                [
                    format!("identified {lake_count} depression-fed lakes"),
                    format!(
                        "traced {river_count} river cells at accumulation threshold {threshold}"
                    ),
                ],
            ),
            ..WorldPatch::default()
        })
    }
}

fn label_lakes(world: &LayeredWorld, lake_mask: &[bool]) -> (Vec<Option<u32>>, u32) {
    let mut ids = vec![None; lake_mask.len()];
    let mut next_id = 1_u32;
    for seed in 0..lake_mask.len() as u32 {
        if !lake_mask[seed as usize] || ids[seed as usize].is_some() {
            continue;
        }
        ids[seed as usize] = Some(next_id);
        let mut pending = VecDeque::from([seed]);
        while let Some(current) = pending.pop_front() {
            for direction in HexDirection::ALL {
                let Some(neighbor) = world.neighbor_id(current, direction) else {
                    continue;
                };
                if lake_mask[neighbor as usize] && ids[neighbor as usize].is_none() {
                    ids[neighbor as usize] = Some(next_id);
                    pending.push_back(neighbor);
                }
            }
        }
        next_id += 1;
    }
    (ids, next_id - 1)
}

impl WorldPass for LandformPass {
    fn name(&self) -> &'static str {
        "landforms"
    }

    fn reads(&self) -> &'static [WorldLayer] {
        &[
            WorldLayer::Elevation,
            WorldLayer::Surface,
            WorldLayer::Hydrology,
        ]
    }

    fn writes(&self) -> &'static [WorldLayer] {
        &[WorldLayer::Landform]
    }

    fn run(&self, world: &LayeredWorld, _seed: u64) -> Result<WorldPatch, String> {
        let mut landforms = Vec::with_capacity(world.cells().len());
        for cell_id in 0..world.cells().len() as u32 {
            let cell = &world.cells()[cell_id as usize];
            if cell.surface != Surface::Land {
                continue;
            }
            let max_relief = HexDirection::ALL
                .into_iter()
                .filter_map(|direction| world.neighbor_id(cell_id, direction))
                .map(|neighbor| {
                    cell.elevation
                        .abs_diff(world.cells()[neighbor as usize].elevation)
                })
                .max()
                .unwrap_or_default();
            let landform = if cell.river.is_some() && max_relief > 28 {
                Landform::Valley
            } else if cell.elevation >= 430 || max_relief >= 170 {
                Landform::Mountain
            } else if cell.elevation >= 300 && max_relief < 48 {
                Landform::Plateau
            } else if cell.elevation >= 155 || max_relief >= 58 {
                Landform::Hill
            } else {
                Landform::Plain
            };
            landforms.push((cell_id, landform));
        }
        Ok(WorldPatch {
            report: report(
                landforms.len(),
                ["derived landforms from final relief and hydrology".to_owned()],
            ),
            landforms,
            ..WorldPatch::default()
        })
    }
}

impl WorldPass for BiomePass {
    fn name(&self) -> &'static str {
        "biomes"
    }

    fn reads(&self) -> &'static [WorldLayer] {
        &[
            WorldLayer::Elevation,
            WorldLayer::Surface,
            WorldLayer::Landform,
            WorldLayer::Moisture,
        ]
    }

    fn writes(&self) -> &'static [WorldLayer] {
        &[WorldLayer::Biome, WorldLayer::Fertility]
    }

    fn run(&self, world: &LayeredWorld, seed: u64) -> Result<WorldPatch, String> {
        let mut biomes = Vec::new();
        let mut fertility = Vec::new();
        for cell_id in 0..world.cells().len() as u32 {
            let cell = &world.cells()[cell_id as usize];
            if cell.surface != Surface::Land {
                continue;
            }
            let row = cell_id / world.width();
            let latitude = (i64::from(row) * 2 - i64::from(world.height())).unsigned_abs() * 255
                / u64::from(world.height());
            let biome = if cell.landform == Landform::Mountain || cell.elevation >= 480 {
                Biome::Alpine
            } else if latitude > 215 {
                Biome::Tundra
            } else if cell.moisture >= 205 {
                Biome::Wetland
            } else if cell.moisture >= 135 {
                Biome::Forest
            } else if cell.moisture < 68 {
                Biome::Dryland
            } else {
                Biome::TemperateGrassland
            };
            let variation =
                i16::try_from(mix(seed, cell_id as i32, 9) % 31).unwrap_or_default() - 15;
            let base_fertility = match biome {
                Biome::Wetland => 205,
                Biome::Forest => 165,
                Biome::TemperateGrassland => 180,
                Biome::Dryland => 70,
                Biome::Alpine | Biome::Tundra => 45,
            };
            biomes.push((cell_id, biome));
            fertility.push((
                cell_id,
                u8::try_from((base_fertility + variation).clamp(0, 255)).unwrap_or_default(),
            ));
        }
        Ok(WorldPatch {
            report: report(
                biomes.len(),
                ["classified biome and fertility without replacing landforms".to_owned()],
            ),
            biomes,
            fertility,
            ..WorldPatch::default()
        })
    }
}

impl WorldPass for GameplayPass {
    fn name(&self) -> &'static str {
        "gameplay"
    }

    fn reads(&self) -> &'static [WorldLayer] {
        &[
            WorldLayer::Surface,
            WorldLayer::Landform,
            WorldLayer::Biome,
            WorldLayer::Hydrology,
            WorldLayer::Fertility,
        ]
    }

    fn writes(&self) -> &'static [WorldLayer] {
        &[WorldLayer::Gameplay]
    }

    fn run(&self, world: &LayeredWorld, _seed: u64) -> Result<WorldPatch, String> {
        let mut gameplay = world
            .cells()
            .iter()
            .enumerate()
            .map(|(id, cell)| {
                let land = cell.surface == Surface::Land;
                let movement_cost = match cell.landform {
                    Landform::Plain | Landform::Valley => 10,
                    Landform::Hill | Landform::Plateau => 14,
                    Landform::Mountain => 22,
                } + u16::from(cell.river.is_some()) * 2;
                let military_capacity = match cell.landform {
                    Landform::Plain | Landform::Valley => 100,
                    Landform::Hill | Landform::Plateau => 82,
                    Landform::Mountain => 58,
                };
                let habitable = land && !matches!(cell.biome, Biome::Alpine | Biome::Tundra);
                (
                    id as u32,
                    GameplayCell {
                        passable: land,
                        capturable: land,
                        habitable,
                        movement_cost,
                        military_capacity: if land { military_capacity } else { 0 },
                        civilian_capacity: if habitable {
                            u16::from(cell.fertility).saturating_mul(2)
                        } else {
                            0
                        },
                    },
                )
            })
            .collect::<Vec<_>>();
        let mut labels = vec![usize::MAX; world.cells().len()];
        let mut component_sizes = Vec::new();
        for seed in 0..world.cells().len() as u32 {
            if labels[seed as usize] != usize::MAX
                || world.cells()[seed as usize].surface != Surface::Land
            {
                continue;
            }
            let label = component_sizes.len();
            let mut size = 0_usize;
            labels[seed as usize] = label;
            let mut pending = VecDeque::from([seed]);
            while let Some(current) = pending.pop_front() {
                size += 1;
                for direction in HexDirection::ALL {
                    let Some(neighbor) = world.neighbor_id(current, direction) else {
                        continue;
                    };
                    if labels[neighbor as usize] == usize::MAX
                        && world.cells()[neighbor as usize].surface == Surface::Land
                    {
                        labels[neighbor as usize] = label;
                        pending.push_back(neighbor);
                    }
                }
            }
            component_sizes.push(size);
        }
        let main_component = component_sizes
            .iter()
            .enumerate()
            .max_by_key(|(_, size)| **size)
            .map(|(label, _)| label)
            .ok_or_else(|| "gameplay pass has no land component".to_owned())?;
        let mut excluded = 0_usize;
        for (cell_id, cell) in &mut gameplay {
            if world.cells()[*cell_id as usize].surface == Surface::Land
                && labels[*cell_id as usize] != main_component
            {
                excluded += 1;
                cell.passable = false;
                cell.capturable = false;
                cell.habitable = false;
                cell.military_capacity = 0;
                cell.civilian_capacity = 0;
            }
        }
        Ok(WorldPatch {
            report: report(
                gameplay.len(),
                [format!(
                    "derived gameplay properties and excluded {excluded} disconnected decorative land cells"
                )],
            ),
            gameplay,
            ..WorldPatch::default()
        })
    }
}

impl WorldPass for SpawnPass {
    fn name(&self) -> &'static str {
        "spawns"
    }

    fn reads(&self) -> &'static [WorldLayer] {
        &[WorldLayer::Gameplay]
    }

    fn writes(&self) -> &'static [WorldLayer] {
        &[WorldLayer::Spawns]
    }

    fn run(&self, world: &LayeredWorld, _seed: u64) -> Result<WorldPatch, String> {
        let needed = usize::from(world.manifest.player_count);
        let all_candidates = (0..world.cells().len() as u32)
            .filter(|cell_id| {
                let cell = &world.cells()[*cell_id as usize];
                cell.gameplay.capturable
                    && cell.gameplay.habitable
                    && HexDirection::ALL.into_iter().any(|direction| {
                        world
                            .neighbor_id(*cell_id, direction)
                            .is_some_and(|neighbor| {
                                world.cells()[neighbor as usize].gameplay.capturable
                            })
                    })
            })
            .collect::<Vec<_>>();
        if all_candidates.len() < needed {
            return Err(format!(
                "spawn pass needs {needed} habitable candidates; found {}",
                all_candidates.len()
            ));
        }
        let target_candidates = needed.saturating_mul(64).clamp(4_096, 65_536);
        let stride = all_candidates.len().div_ceil(target_candidates).max(1);
        let mut candidates = all_candidates
            .into_iter()
            .step_by(stride)
            .collect::<Vec<_>>();
        if candidates.len() < needed {
            candidates = (0..world.cells().len() as u32)
                .filter(|id| world.cells()[*id as usize].gameplay.habitable)
                .collect();
        }
        let desired = Axial::new(
            world.manifest.q_min + world.width() as i32 / 4,
            world.manifest.r_min + world.height() as i32 / 2,
        );
        let first = candidates
            .iter()
            .enumerate()
            .min_by_key(|(_, id)| {
                let coordinate = world.coordinate(**id).expect("candidate in bounds");
                (coordinate.distance(desired), coordinate)
            })
            .map(|(index, _)| index)
            .ok_or_else(|| "spawn pass has no candidates".to_owned())?;
        let mut chosen = vec![false; candidates.len()];
        let mut nearest = vec![u64::MAX; candidates.len()];
        let mut spawns = Vec::with_capacity(needed);
        let choose =
            |index: usize, spawns: &mut Vec<Axial>, chosen: &mut [bool], nearest: &mut [u64]| {
                let coordinate = world
                    .coordinate(candidates[index])
                    .expect("candidate in bounds");
                chosen[index] = true;
                nearest[index] = 0;
                spawns.push(coordinate);
                for (candidate_index, cell_id) in candidates.iter().enumerate() {
                    if chosen[candidate_index] {
                        continue;
                    }
                    let candidate = world.coordinate(*cell_id).expect("candidate in bounds");
                    nearest[candidate_index] =
                        nearest[candidate_index].min(candidate.distance(coordinate));
                }
            };
        choose(first, &mut spawns, &mut chosen, &mut nearest);
        while spawns.len() < needed {
            let next = candidates
                .iter()
                .enumerate()
                .filter(|(index, _)| !chosen[*index])
                .max_by_key(|(index, id)| {
                    (
                        nearest[*index],
                        world.coordinate(**id).expect("candidate in bounds"),
                    )
                })
                .map(|(index, _)| index)
                .ok_or_else(|| "spawn candidate sampling exhausted".to_owned())?;
            choose(next, &mut spawns, &mut chosen, &mut nearest);
        }
        Ok(WorldPatch {
            spawns: Some(spawns),
            report: report(
                needed,
                [format!(
                    "sampled {} spawn candidates from a bounded pool",
                    candidates.len()
                )],
            ),
            ..WorldPatch::default()
        })
    }
}
