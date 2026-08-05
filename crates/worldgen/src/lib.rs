//! Deterministic stepped-island generation shared by tools, server, and client fixtures.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;

use hex_core::{
    Axial, Cell, ForceComposition, HexMap, MovementConfig, TerrainKind, connected_components,
    shortest_path,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

pub const NEUTRAL_PLAYER: u32 = 0;
pub const PLAYER_ONE: u32 = 1;
pub const PLAYER_TWO: u32 = 2;
pub const DEFAULT_PLAYER_COUNT: u16 = 2;
pub const MIN_PLAYER_COUNT: u16 = 2;
pub const MAX_PLAYER_COUNT: u16 = 500;
/// Multi-cell ring footprints remain available through this count; higher
/// counts use one-cell farthest-point sampling.
pub const LEGACY_RING_PLAYER_LIMIT: u16 = 8;
pub const GENERATOR_VERSION: u16 = 1;

const fn default_player_count() -> u16 {
    DEFAULT_PLAYER_COUNT
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum MapPreset {
    Dev,
    Playtest,
    Validation,
}

impl MapPreset {
    #[must_use]
    pub const fn dimensions(self) -> (u16, u16) {
        match self {
            Self::Dev => (64, 64),
            Self::Playtest => (128, 128),
            Self::Validation => (192, 192),
        }
    }

    #[must_use]
    pub const fn seed(self) -> u64 {
        match self {
            Self::Dev => 0x0000_0FD3_6401,
            Self::Playtest => 0x0000_FA11_2802,
            Self::Validation => 0x0000_FA11_9203,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dev => "dev-stepped-island",
            Self::Playtest => "playtest-stepped-island",
            Self::Validation => "validation-stepped-island",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MapManifest {
    pub name: String,
    pub generator_version: u16,
    pub width: u16,
    pub height: u16,
    pub q_min: i32,
    pub r_min: i32,
    pub seed: u64,
    pub content_hash: u64,
    pub capturable_land: u32,
    #[serde(default = "default_player_count")]
    pub player_count: u16,
    pub spawn_cells: Vec<Axial>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GeneratedMap {
    pub manifest: MapManifest,
    pub cells: HexMap,
}

/// JSON and other human-inspectable formats encode cells as a stable array.
/// `HexMap` itself uses structured axial coordinates as ordered-map keys, which
/// formats such as JSON cannot represent as object keys.
impl Serialize for GeneratedMap {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct WireMap<'a> {
            manifest: &'a MapManifest,
            cells: Vec<&'a Cell>,
        }

        WireMap {
            manifest: &self.manifest,
            cells: self.cells.cells().collect(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for GeneratedMap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireMap {
            manifest: MapManifest,
            cells: Vec<Cell>,
        }

        let wire = WireMap::deserialize(deserializer)?;
        let mut cells = HexMap::new();
        for cell in wire.cells {
            let coordinate = cell.coordinate;
            if cells.insert(cell).is_some() {
                return Err(D::Error::custom(format!(
                    "duplicate map cell at ({}, {})",
                    coordinate.q, coordinate.r
                )));
            }
        }
        Ok(Self {
            manifest: wire.manifest,
            cells,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationReport {
    pub total_cells: usize,
    pub ground_cells: usize,
    pub capturable_cells: usize,
    pub cliffs: usize,
    pub slopes: usize,
}

#[must_use]
pub fn generate_preset(preset: MapPreset) -> GeneratedMap {
    generate_preset_for_players(preset, DEFAULT_PLAYER_COUNT)
}

/// Generates a curated preset for a supported number of players.
///
/// # Panics
///
/// Panics unless `player_count` is in `2..=500`.
#[must_use]
pub fn generate_preset_for_players(preset: MapPreset, player_count: u16) -> GeneratedMap {
    let (width, height) = preset.dimensions();
    generate_for_players(preset.name(), width, height, preset.seed(), player_count)
}

/// Generates a deterministic rectangular map containing one stepped island.
///
/// # Panics
///
/// Panics when either dimension is smaller than 24 cells. Curated presets are
/// all larger than this minimum.
#[must_use]
pub fn generate(name: impl Into<String>, width: u16, height: u16, seed: u64) -> GeneratedMap {
    generate_for_players(name, width, height, seed, DEFAULT_PLAYER_COUNT)
}

/// Generates a deterministic rectangular map for contiguous player IDs
/// `1..=player_count`; owner zero remains neutral.
///
/// Counts `2..=8` keep the legacy multi-cell ring footprints. Counts above
/// eight use deterministic farthest-point sampling with equal one-cell starts.
///
/// # Panics
///
/// Panics when `player_count` is outside `2..=500`, when either dimension is
/// smaller than 24 cells, or when either dimension is smaller than 32 cells
/// for more than two players. The 24-cell minimum is retained for legacy
/// two-player generation.
#[must_use]
pub fn generate_for_players(
    name: impl Into<String>,
    width: u16,
    height: u16,
    seed: u64,
    player_count: u16,
) -> GeneratedMap {
    assert!(
        width >= 24 && height >= 24,
        "maps must be at least 24 by 24"
    );
    assert!(
        (MIN_PLAYER_COUNT..=MAX_PLAYER_COUNT).contains(&player_count),
        "player count must be between 2 and 500"
    );
    assert!(
        player_count == DEFAULT_PLAYER_COUNT || (width >= 32 && height >= 32),
        "maps with more than two players must be at least 32 by 32"
    );

    let q_min = -(i32::from(width) / 2);
    let r_min = -(i32::from(height) / 2);
    let mut cells = HexMap::new();
    let mut candidate_land = Vec::new();

    for row in 0..height {
        for column in 0..width {
            let coordinate = Axial::new(q_min + i32::from(column), r_min + i32::from(row));
            if island_land(column, row, width, height, seed) {
                cells.insert(Cell::ground(coordinate, 1, None, 100));
                candidate_land.push(coordinate);
            } else {
                cells.insert(Cell::water(coordinate, 0));
            }
        }
    }

    retain_largest_island(&mut cells, &candidate_land);
    shape_elevation(&mut cells, width, height, q_min, r_min, seed);

    let short_dimension = width.min(height);
    let spawn_cells = spawn_cells(
        &cells,
        width,
        height,
        q_min,
        r_min,
        player_count,
        short_dimension,
    );
    seed_players(&mut cells, &spawn_cells, short_dimension);

    let capturable_land = cells.cells().filter(|cell| cell.capturable).count();
    // A u16-by-u16 map has fewer than u32::MAX cells by construction.
    let capturable_land = u32::try_from(capturable_land).unwrap_or(u32::MAX);
    let content_hash = content_hash(&cells, width, height, seed);

    GeneratedMap {
        manifest: MapManifest {
            name: name.into(),
            generator_version: GENERATOR_VERSION,
            width,
            height,
            q_min,
            r_min,
            seed,
            content_hash,
            capturable_land,
            player_count,
            spawn_cells,
        },
        cells,
    }
}

/// Checks connectivity, spawn validity, conquest accounting, slopes, and cliffs.
///
/// # Errors
///
/// Returns a human-readable validation error when the generated map cannot
/// support the V1 Conquest rules.
pub fn validate(generated: &GeneratedMap) -> Result<ValidationReport, String> {
    let map = &generated.cells;
    let manifest = &generated.manifest;
    let expected_total = validate_manifest_and_bounds(map, manifest)?;
    validate_cell_state(map, manifest.player_count)?;

    let capturable: Vec<_> = map
        .cells()
        .filter(|cell| cell.capturable)
        .map(|cell| cell.coordinate)
        .collect();
    let manifest_capturable = usize::try_from(manifest.capturable_land)
        .map_err(|_| "capturable denominator does not fit this platform")?;
    if capturable.len() != manifest_capturable {
        return Err("capturable denominator does not match map".to_owned());
    }
    if capturable.is_empty() {
        return Err("map has no capturable land".to_owned());
    }

    let movement = MovementConfig::default();
    let components = connected_components(map, capturable.iter().copied(), &movement);
    if components.len() != 1 {
        return Err(format!(
            "capturable land has {} disconnected components",
            components.len()
        ));
    }
    validate_spawns(map, manifest, &movement)?;

    let (slopes, cliffs) = elevation_edge_counts(map);
    if cliffs == 0 || slopes == 0 {
        return Err("map must exercise both slopes and cliffs".to_owned());
    }

    Ok(ValidationReport {
        total_cells: expected_total,
        ground_cells: map
            .cells()
            .filter(|cell| cell.terrain.ground_passable())
            .count(),
        capturable_cells: capturable.len(),
        cliffs,
        slopes,
    })
}

fn validate_manifest_and_bounds(map: &HexMap, manifest: &MapManifest) -> Result<usize, String> {
    if manifest.generator_version != GENERATOR_VERSION {
        return Err(format!(
            "unsupported generator version {}; expected {GENERATOR_VERSION}",
            manifest.generator_version
        ));
    }
    if manifest.width < 24 || manifest.height < 24 {
        return Err("manifest dimensions are below the 24 by 24 minimum".to_owned());
    }
    let expected_origin = Axial::new(
        -(i32::from(manifest.width) / 2),
        -(i32::from(manifest.height) / 2),
    );
    if manifest.q_min != expected_origin.q || manifest.r_min != expected_origin.r {
        return Err("manifest bounds do not match dimensions".to_owned());
    }

    let expected_total = usize::from(manifest.width) * usize::from(manifest.height);
    if map.cells().count() != expected_total {
        return Err("manifest dimensions do not match cell count".to_owned());
    }
    for row in 0..manifest.height {
        for column in 0..manifest.width {
            let coordinate = Axial::new(
                manifest.q_min + i32::from(column),
                manifest.r_min + i32::from(row),
            );
            if !map.contains(coordinate) {
                return Err(format!(
                    "map is missing bounded cell ({}, {})",
                    coordinate.q, coordinate.r
                ));
            }
        }
    }

    let expected_hash = content_hash(map, manifest.width, manifest.height, manifest.seed);
    if manifest.content_hash != expected_hash {
        return Err(format!(
            "content hash mismatch: manifest {:016x}, computed {expected_hash:016x}",
            manifest.content_hash
        ));
    }
    Ok(expected_total)
}

fn validate_cell_state(map: &HexMap, player_count: u16) -> Result<(), String> {
    for cell in map.cells() {
        if cell.capturable && !cell.terrain.ground_passable() {
            return Err(format!(
                "capturable cell ({}, {}) is not ground-passable",
                cell.coordinate.q, cell.coordinate.r
            ));
        }
        if cell.force() > cell.military_capacity {
            return Err(format!(
                "cell ({}, {}) exceeds military capacity",
                cell.coordinate.q, cell.coordinate.r
            ));
        }
        if cell.civilian_population > cell.civilian_capacity {
            return Err(format!(
                "cell ({}, {}) exceeds civilian capacity",
                cell.coordinate.q, cell.coordinate.r
            ));
        }
        if cell
            .owner
            .is_some_and(|owner| owner == NEUTRAL_PLAYER || owner > u32::from(player_count))
        {
            return Err(format!(
                "cell ({}, {}) has an unsupported owner",
                cell.coordinate.q, cell.coordinate.r
            ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn validate_spawns(
    map: &HexMap,
    manifest: &MapManifest,
    movement: &MovementConfig,
) -> Result<(), String> {
    if !(MIN_PLAYER_COUNT..=MAX_PLAYER_COUNT).contains(&manifest.player_count) {
        return Err("player count must be between 2 and 500".to_owned());
    }
    if manifest.spawn_cells.len() != usize::from(manifest.player_count) {
        return Err("spawn count does not match player count".to_owned());
    }
    if manifest
        .spawn_cells
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .len()
        != manifest.spawn_cells.len()
    {
        return Err("player spawns must be distinct".to_owned());
    }
    let high_scale = manifest.player_count > LEGACY_RING_PLAYER_LIMIT;
    let seed_radius = if high_scale {
        0
    } else {
        seed_radius(manifest.width.min(manifest.height))
    };
    let expected_cells = 1 + 3 * seed_radius * (seed_radius + 1);
    let minimum_spacing = if high_scale {
        1
    } else {
        minimum_spawn_spacing(manifest.width.min(manifest.height), manifest.player_count)
    };
    let mut expected_totals = None;
    for (index, spawn) in manifest.spawn_cells.iter().enumerate() {
        let owner = u32::try_from(index + 1).map_err(|_| "player ID overflow")?;
        let Some(cell) = map.get(*spawn) else {
            return Err(format!("spawn {} is missing", index + 1));
        };
        if !cell.capturable || !cell.terrain.ground_passable() || cell.owner != Some(owner) {
            return Err(format!("spawn {} is not valid owned land", index + 1));
        }
        if manifest.spawn_cells[..index]
            .iter()
            .any(|other| other.distance(*spawn) < minimum_spacing)
        {
            return Err("player spawns are not sufficiently spaced".to_owned());
        }

        let owned_coordinates = map
            .cells()
            .filter(|cell| cell.owner == Some(owner))
            .map(|cell| cell.coordinate)
            .collect::<Vec<_>>();
        if u64::try_from(owned_coordinates.len()).unwrap_or(u64::MAX) != expected_cells {
            return Err(format!(
                "player {} initial footprint has {} cells; expected {expected_cells}",
                index + 1,
                owned_coordinates.len()
            ));
        }
        if high_scale {
            if owned_coordinates != vec![*spawn] {
                return Err(format!(
                    "player {} must own exactly its one-cell spawn",
                    index + 1
                ));
            }
        } else if !full_seed_footprint(map, *spawn, seed_radius)
            || owned_coordinates
                .iter()
                .any(|cell| cell.distance(*spawn) > seed_radius)
        {
            return Err(format!(
                "player {} does not own its full seed footprint",
                index + 1
            ));
        }
        if connected_components(map, owned_coordinates.iter().copied(), movement).len() != 1 {
            return Err(format!(
                "player {} initial footprint is not connected",
                index + 1
            ));
        }
        let totals = map.cells().filter(|cell| cell.owner == Some(owner)).fold(
            (0_u64, 0_u64, 0_u64),
            |(cells, infantry, civilians), cell| {
                (
                    cells + 1,
                    infantry + cell.force(),
                    civilians + cell.civilian_population,
                )
            },
        );
        let capacity_total = map
            .cells()
            .filter(|cell| cell.owner == Some(owner))
            .map(|cell| cell.military_capacity)
            .sum::<u64>();
        // Low-scale multi-cell footprints keep historical terrain capacities.
        // High-scale one-cell starts require equal military capacity as well.
        let comparable = if high_scale {
            (totals.0, totals.1, totals.2, capacity_total)
        } else {
            (totals.0, totals.1, totals.2, 0)
        };
        if expected_totals.is_some_and(|expected| expected != comparable) {
            return Err(if high_scale {
                "players do not have equal initial controlled, infantry, civilian, and military-capacity totals"
                    .to_owned()
            } else {
                "players do not have equal initial controlled, infantry, and civilian totals"
                    .to_owned()
            });
        }
        if high_scale && capacity_total < HIGH_SCALE_SEED_INFANTRY {
            return Err(format!(
                "player {} spawn military capacity {capacity_total} cannot hold seeded force {HIGH_SCALE_SEED_INFANTRY}",
                index + 1
            ));
        }
        expected_totals = Some(comparable);
    }

    for spawn in manifest.spawn_cells.iter().skip(1) {
        if shortest_path(map, manifest.spawn_cells[0], *spawn, movement, |cell| {
            cell.capturable
        })
        .is_none()
        {
            return Err("player spawns have no traversable ground path".to_owned());
        }
    }
    Ok(())
}

fn elevation_edge_counts(map: &HexMap) -> (usize, usize) {
    let mut seen_edges = BTreeSet::new();
    let mut cliffs = 0;
    let mut slopes = 0;
    for cell in map.cells().filter(|cell| cell.terrain.ground_passable()) {
        for neighbor in cell.coordinate.neighbors() {
            let edge = if cell.coordinate < neighbor {
                (cell.coordinate, neighbor)
            } else {
                (neighbor, cell.coordinate)
            };
            if !seen_edges.insert(edge) {
                continue;
            }
            let Some(other) = map.get(neighbor) else {
                continue;
            };
            if !other.terrain.ground_passable() {
                continue;
            }
            let delta = (i32::from(cell.elevation) - i32::from(other.elevation)).unsigned_abs();
            if delta > 1 {
                cliffs += 1;
            } else if delta == 1 {
                slopes += 1;
            }
        }
    }
    (slopes, cliffs)
}

fn island_land(column: u16, row: u16, width: u16, height: u16, seed: u64) -> bool {
    let x = (i64::from(column) * 2 + 1) * 1_024 / i64::from(width) - 1_024;
    let y = (i64::from(row) * 2 + 1) * 1_024 / i64::from(height) - 1_024;
    let radial = x * x + y * y + x * y / 3;
    let coarse_x = i32::from(column / 4);
    let coarse_y = i32::from(row / 4);
    let noise = i64::try_from(mix(seed, coarse_x, coarse_y) % 161).unwrap_or_default() - 80;
    let threshold = 790_000 + noise * 1_100;
    radial <= threshold
}

fn retain_largest_island(map: &mut HexMap, candidates: &[Axial]) {
    let mut components =
        connected_components(map, candidates.iter().copied(), &MovementConfig::default());
    components.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    let retained: BTreeSet<_> = components.first().into_iter().flatten().copied().collect();
    for coordinate in candidates {
        if !retained.contains(coordinate) {
            map.insert(Cell::water(*coordinate, 0));
        }
    }
}

fn shape_elevation(map: &mut HexMap, width: u16, height: u16, q_min: i32, r_min: i32, seed: u64) {
    let short = i32::from(width.min(height));
    let hill_radius = (short / 5).max(5);
    let mesa_radius = (short / 16).max(2);
    let hills = [
        Axial::new(
            q_min + i32::from(width) * 2 / 5,
            r_min + i32::from(height) * 2 / 5,
        ),
        Axial::new(
            q_min + i32::from(width) * 2 / 3,
            r_min + i32::from(height) * 3 / 5,
        ),
    ];
    let mesa = Axial::new(
        q_min + i32::from(width) * 9 / 16,
        r_min + i32::from(height) * 5 / 14,
    );

    let coordinates: Vec<_> = map.coordinates().collect();
    for coordinate in coordinates {
        let Some(cell) = map.get_mut(coordinate) else {
            continue;
        };
        if !cell.terrain.ground_passable() {
            continue;
        }

        let mut elevation = 1_i16;
        for hill in hills {
            let distance = i32::try_from(coordinate.distance(hill)).unwrap_or(i32::MAX);
            if distance < hill_radius {
                let rise = (hill_radius - distance) * 3 / hill_radius;
                elevation = elevation.max(i16::try_from(1 + rise).unwrap_or(i16::MAX));
            }
        }

        let mesa_distance = i32::try_from(coordinate.distance(mesa)).unwrap_or(i32::MAX);
        if mesa_distance <= mesa_radius {
            elevation = 5;
            cell.capturable = false;
            cell.habitable = false;
        } else if mesa_distance == mesa_radius + 1 {
            elevation = elevation.min(2);
        }

        let variation = mix(seed ^ 0xE1E7_A710, coordinate.q, coordinate.r) % 11;
        if variation == 0 && elevation == 1 {
            elevation = 2;
        }

        cell.elevation = elevation;
        cell.terrain = match elevation {
            0 | 1 => TerrainKind::Plains,
            2 | 3 => TerrainKind::Hills,
            _ => TerrainKind::Mountain,
        };
        cell.military_capacity = match cell.terrain {
            TerrainKind::Plains => 100,
            TerrainKind::Hills => 80,
            TerrainKind::Mountain => 60,
            TerrainKind::Water => 0,
        };
        cell.civilian_capacity = if cell.habitable { 100 } else { 0 };
    }
}

fn nearest_capturable(map: &HexMap, desired: Axial) -> Axial {
    map.cells()
        .filter(|cell| cell.capturable)
        .map(|cell| cell.coordinate)
        .min_by_key(|coordinate| (coordinate.distance(desired), *coordinate))
        .unwrap_or(desired)
}

const fn seed_radius(short_dimension: u16) -> u64 {
    let scaled = short_dimension as u64 / 24;
    if scaled < 2 { 2 } else { scaled }
}

fn full_seed_footprint(map: &HexMap, center: Axial, radius: u64) -> bool {
    let Ok(radius_i32) = i32::try_from(radius) else {
        return false;
    };
    (-radius_i32..=radius_i32).all(|dq| {
        (-radius_i32..=radius_i32).all(|dr| {
            let coordinate = Axial::new(center.q + dq, center.r + dr);
            coordinate.distance(center) > radius
                || map.get(coordinate).is_some_and(|cell| {
                    let seeded_infantry = if coordinate == center { 70 } else { 35 };
                    cell.capturable
                        && cell.terrain.ground_passable()
                        && cell.military_capacity >= seeded_infantry
                })
        })
    })
}

fn seed_footprint_is_connected(map: &HexMap, center: Axial, radius: u64) -> bool {
    let coordinates = map
        .cells()
        .filter(|cell| cell.coordinate.distance(center) <= radius)
        .map(|cell| cell.coordinate)
        .collect::<Vec<_>>();
    connected_components(map, coordinates, &MovementConfig::default()).len() == 1
}

fn minimum_spawn_spacing(short_dimension: u16, player_count: u16) -> u64 {
    let radius = seed_radius(short_dimension);
    let ring_radius = u64::from(short_dimension) / 4;
    (3 * ring_radius / u64::from(player_count)).max(2 * radius + 1)
}

fn axial_ring(radius: i32) -> Vec<Axial> {
    debug_assert!(radius > 0);
    let mut ring = Vec::with_capacity(usize::try_from(6 * radius).unwrap_or_default());
    let mut coordinate = Axial::new(-radius, 0);
    let directions = [
        Axial::new(1, -1),
        Axial::new(1, 0),
        Axial::new(0, 1),
        Axial::new(-1, 1),
        Axial::new(-1, 0),
        Axial::new(0, -1),
    ];
    for direction in directions {
        for _ in 0..radius {
            ring.push(coordinate);
            coordinate = coordinate + direction;
        }
    }
    ring
}

fn nearest_valid_spawn(
    map: &HexMap,
    desired: Axial,
    selected: &[Axial],
    seed_radius: u64,
    minimum_spacing: u64,
    search_bound: i32,
) -> Option<Axial> {
    for distance in 0..=search_bound {
        for q in desired.q - distance..=desired.q + distance {
            for r in desired.r - distance..=desired.r + distance {
                let candidate = Axial::new(q, r);
                if candidate.distance(desired)
                    != u64::try_from(distance).expect("search distance is nonnegative")
                    || selected
                        .iter()
                        .any(|spawn| spawn.distance(candidate) < minimum_spacing)
                    || !full_seed_footprint(map, candidate, seed_radius)
                    || !seed_footprint_is_connected(map, candidate, seed_radius)
                {
                    continue;
                }
                return Some(candidate);
            }
        }
    }
    None
}

fn spawn_cells(
    map: &HexMap,
    width: u16,
    height: u16,
    q_min: i32,
    r_min: i32,
    player_count: u16,
    short_dimension: u16,
) -> Vec<Axial> {
    if player_count == DEFAULT_PLAYER_COUNT {
        // Preserve the exact version-1 two-player targets and nearest-land
        // behavior, including custom non-square map output.
        return vec![
            nearest_capturable(
                map,
                Axial::new(q_min + i32::from(width / 4), r_min + i32::from(height / 2)),
            ),
            nearest_capturable(
                map,
                Axial::new(
                    q_min + i32::from(width.saturating_mul(3) / 4),
                    r_min + i32::from(height / 2),
                ),
            ),
        ];
    }

    if player_count > LEGACY_RING_PLAYER_LIMIT {
        return farthest_point_spawns(map, player_count);
    }

    // Sample exact cells from an axial hex ring. This avoids treating a
    // Cartesian circle's x/y values as axial q/r coordinates.
    let ring_radius = i32::from(short_dimension / 4);
    let ring = axial_ring(ring_radius);
    let center = Axial::new(q_min + i32::from(width / 2), r_min + i32::from(height / 2));
    let radius = seed_radius(short_dimension);
    let spacing = minimum_spawn_spacing(short_dimension, player_count);
    let search_bound = i32::from(width) + i32::from(height);
    let mut spawns = Vec::with_capacity(usize::from(player_count));
    for player_index in 0..player_count {
        let target = ring[usize::from(player_index) * ring.len() / usize::from(player_count)];
        let desired = Axial::new(center.q + target.q, center.r + target.r);
        let spawn = nearest_valid_spawn(map, desired, &spawns, radius, spacing, search_bound)
            .expect("supported map has enough valid, well-spaced spawn footprints");
        spawns.push(spawn);
    }
    spawns
}

/// High-scale starts normalize their one spawn cell to the same capacity and
/// force. This permits well-spaced starts even on the dense 64x64/500-player
/// stress case while keeping every starting resource exactly equal.
const HIGH_SCALE_SEED_INFANTRY: u64 = 70;
const HIGH_SCALE_MILITARY_CAPACITY: u64 = 100;

/// Deterministic incremental farthest-point sampling over every sorted,
/// capturable passable cell that has at least one capturable neighbor. Used
/// only above the legacy ring footprint limit. Selected spawn cells are later
/// normalized to one uniform capacity by [`seed_players`].
///
/// Complexity is O(candidates * players): each candidate stores its current
/// minimum distance to the chosen set and is updated once per newly chosen
/// spawn, never by rescanning the full spawn list every iteration.
fn farthest_point_spawns(map: &HexMap, player_count: u16) -> Vec<Axial> {
    let needed = usize::from(player_count);
    let candidates = high_scale_spawn_candidates(map);
    assert!(
        candidates.len() >= needed,
        "high-scale spawn selection needs {needed} capturable cells with expansion frontage; found {}",
        candidates.len()
    );

    // First seed: the sorted candidate nearest the classic player-one target
    // keeps low-count continuity when crossing the high-scale boundary.
    let bounds = candidates
        .iter()
        .fold(None, |acc: Option<(i32, i32, i32, i32)>, cell| {
            Some(match acc {
                None => (cell.q, cell.q, cell.r, cell.r),
                Some((qmin, qmax, rmin, rmax)) => (
                    qmin.min(cell.q),
                    qmax.max(cell.q),
                    rmin.min(cell.r),
                    rmax.max(cell.r),
                ),
            })
        })
        .expect("candidates is non-empty");
    let desired = Axial::new(
        bounds.0 + (bounds.1 - bounds.0) / 4,
        i32::midpoint(bounds.2, bounds.3),
    );
    let first_index = candidates
        .iter()
        .enumerate()
        .min_by_key(|(_, cell)| (cell.distance(desired), **cell))
        .map(|(index, _)| index)
        .expect("candidates is non-empty");

    // min_distance[i] = distance from candidates[i] to the nearest chosen spawn.
    // Unchosen start at u64::MAX; chosen slots are tombstoned.
    let mut min_distance = vec![u64::MAX; candidates.len()];
    let mut chosen = vec![false; candidates.len()];
    let mut spawns = Vec::with_capacity(usize::from(player_count));

    let choose =
        |index: usize, spawns: &mut Vec<Axial>, min_distance: &mut [u64], chosen: &mut [bool]| {
            let spawn = candidates[index];
            spawns.push(spawn);
            chosen[index] = true;
            min_distance[index] = 0;
            for (candidate_index, candidate) in candidates.iter().enumerate() {
                if chosen[candidate_index] {
                    continue;
                }
                let distance = candidate.distance(spawn);
                if distance < min_distance[candidate_index] {
                    min_distance[candidate_index] = distance;
                }
            }
        };

    choose(first_index, &mut spawns, &mut min_distance, &mut chosen);

    while spawns.len() < usize::from(player_count) {
        let next_index = candidates
            .iter()
            .enumerate()
            .filter(|(index, _)| !chosen[*index])
            .max_by_key(|(index, candidate)| (min_distance[*index], **candidate))
            .map(|(index, _)| index)
            .expect("enough capturable candidates remain");
        assert!(
            min_distance[next_index] >= 2,
            "map cannot give every high-scale spawn an adjacent neutral expansion cell"
        );
        choose(next_index, &mut spawns, &mut min_distance, &mut chosen);
    }
    spawns
}

fn high_scale_spawn_candidates(map: &HexMap) -> Vec<Axial> {
    let mut candidates = map
        .cells()
        .filter(|cell| {
            cell.capturable
                && cell.terrain.ground_passable()
                && cell.coordinate.neighbors().into_iter().any(|neighbor| {
                    map.get(neighbor).is_some_and(|neighbor| {
                        neighbor.capturable && neighbor.terrain.ground_passable()
                    })
                })
        })
        .map(|cell| cell.coordinate)
        .collect::<Vec<_>>();
    candidates.sort_unstable();
    candidates
}

fn seed_players(map: &mut HexMap, spawns: &[Axial], short_dimension: u16) {
    if spawns.len() > usize::from(LEGACY_RING_PLAYER_LIMIT) {
        for (spawn_index, spawn) in spawns.iter().enumerate() {
            let Some(cell) = map.get_mut(*spawn) else {
                continue;
            };
            if !cell.capturable {
                continue;
            }
            let owner = u32::try_from(spawn_index + 1).expect("player IDs fit u32");
            cell.owner = Some(owner);
            cell.military_capacity = HIGH_SCALE_MILITARY_CAPACITY;
            cell.civilian_capacity = 240;
            cell.civilian_population = 180;
            cell.forces = ForceComposition::infantry(HIGH_SCALE_SEED_INFANTRY);
        }
        return;
    }

    let radius = seed_radius(short_dimension);
    let coordinates: Vec<_> = map.coordinates().collect();
    for coordinate in coordinates {
        let Some((spawn_index, distance)) = spawns
            .iter()
            .enumerate()
            .map(|(index, spawn)| (index, coordinate.distance(*spawn)))
            .filter(|(_, distance)| *distance <= radius)
            .min_by_key(|(index, distance)| (*distance, *index))
        else {
            continue;
        };
        let Some(cell) = map.get_mut(coordinate) else {
            continue;
        };
        if !cell.capturable {
            continue;
        }
        let owner = u32::try_from(spawn_index + 1).expect("player IDs fit u32");
        cell.owner = Some(owner);
        cell.civilian_capacity = if distance == 0 { 240 } else { 140 };
        cell.civilian_population = if distance == 0 { 180 } else { 80 };
        cell.forces = ForceComposition::infantry(if distance == 0 { 70 } else { 35 });
    }
}

fn content_hash(map: &HexMap, width: u16, height: u16, seed: u64) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    hash_value(&mut hash, u64::from(width));
    hash_value(&mut hash, u64::from(height));
    hash_value(&mut hash, seed);
    for cell in map.cells() {
        hash_value(
            &mut hash,
            u64::from(u32::from_ne_bytes(cell.coordinate.q.to_ne_bytes())),
        );
        hash_value(
            &mut hash,
            u64::from(u32::from_ne_bytes(cell.coordinate.r.to_ne_bytes())),
        );
        hash_value(
            &mut hash,
            u64::from(u16::from_ne_bytes(cell.elevation.to_ne_bytes())),
        );
        hash_value(&mut hash, u64::from(cell.terrain as u8));
        hash_value(&mut hash, u64::from(cell.capturable));
        hash_value(&mut hash, u64::from(cell.habitable));
        hash_value(&mut hash, u64::from(cell.owner.unwrap_or_default()));
        hash_value(&mut hash, cell.civilian_population);
        hash_value(&mut hash, cell.civilian_capacity);
        hash_value(&mut hash, cell.force());
        hash_value(&mut hash, cell.military_capacity);
    }
    hash
}

fn hash_value(hash: &mut u64, value: u64) {
    for byte in value.to_le_bytes() {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
}

fn mix(seed: u64, q: i32, r: i32) -> u64 {
    let mut value = seed
        ^ u64::from(u32::from_ne_bytes(q.to_ne_bytes())).wrapping_mul(0x9E37_79B1_85EB_CA87)
        ^ u64::from(u32::from_ne_bytes(r.to_ne_bytes())).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    value ^= value >> 30;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_curated_preset_is_pinned_deterministic_and_valid() {
        let cases = [
            (
                MapPreset::Dev,
                0x3b9b_9767_ada3_6223,
                ValidationReport {
                    total_cells: 64 * 64,
                    ground_cells: 2_456,
                    capturable_cells: 2_395,
                    cliffs: 54,
                    slopes: 1_157,
                },
            ),
            (
                MapPreset::Playtest,
                0x894c_50e9_e590_ddb9,
                ValidationReport {
                    total_cells: 128 * 128,
                    ground_cells: 9_874,
                    capturable_cells: 9_657,
                    cliffs: 102,
                    slopes: 4_296,
                },
            ),
            (
                MapPreset::Validation,
                0x40a1_9e2a_d460_8010,
                ValidationReport {
                    total_cells: 192 * 192,
                    ground_cells: 21_953,
                    capturable_cells: 21_484,
                    cliffs: 150,
                    slopes: 9_370,
                },
            ),
        ];

        for (preset, expected_hash, expected_report) in cases {
            let first = generate_preset(preset);
            let second = generate_preset(preset);
            assert_eq!(first, second);
            assert_eq!(first.manifest.content_hash, expected_hash);
            assert_eq!(
                validate(&first).expect("curated seed should validate"),
                expected_report
            );
        }
    }

    #[test]
    fn different_seeds_change_the_content_hash() {
        let first = generate("a", 32, 32, 1);
        let second = generate("b", 32, 32, 2);
        assert_ne!(first.manifest.content_hash, second.manifest.content_hash);
    }

    #[test]
    fn supported_player_counts_are_deterministic_valid_and_fair_on_every_preset() {
        for preset in [MapPreset::Dev, MapPreset::Playtest, MapPreset::Validation] {
            for player_count in MIN_PLAYER_COUNT..=LEGACY_RING_PLAYER_LIMIT {
                let first = generate_preset_for_players(preset, player_count);
                let second = generate_preset_for_players(preset, player_count);
                assert_eq!(first, second, "{preset:?} {player_count}p changed");
                assert_eq!(first.manifest.player_count, player_count);
                assert_eq!(first.manifest.spawn_cells.len(), usize::from(player_count));
                validate(&first).expect("supported player map should validate");

                let totals = (1..=u32::from(player_count))
                    .map(|owner| {
                        first
                            .cells
                            .cells()
                            .filter(|cell| cell.owner == Some(owner))
                            .fold((0_u64, 0_u64, 0_u64), |totals, cell| {
                                (
                                    totals.0 + 1,
                                    totals.1 + cell.force(),
                                    totals.2 + cell.civilian_population,
                                )
                            })
                    })
                    .collect::<BTreeSet<_>>();
                assert_eq!(totals.len(), 1, "{preset:?} {player_count}p is unfair");
            }
        }
    }

    #[test]
    fn high_scale_boundary_counts_are_deterministic_valid_and_fair_on_every_preset() {
        const BOUNDARIES: [u16; 6] = [8, 9, 32, 128, 256, 500];
        for preset in [MapPreset::Dev, MapPreset::Playtest, MapPreset::Validation] {
            for player_count in BOUNDARIES {
                if player_count == 500 && matches!(preset, MapPreset::Dev) {
                    // Dev64 has ~2395 capturable cells; 500 one-cell spawns fit.
                }
                let first = generate_preset_for_players(preset, player_count);
                let second = generate_preset_for_players(preset, player_count);
                assert_eq!(first, second, "{preset:?} {player_count}p changed");
                assert_eq!(first.manifest.player_count, player_count);
                assert_eq!(first.manifest.spawn_cells.len(), usize::from(player_count));
                validate(&first).expect("high-scale map should validate");
                let footprint = if player_count > LEGACY_RING_PLAYER_LIMIT {
                    1
                } else {
                    let radius = seed_radius(first.manifest.width.min(first.manifest.height));
                    1 + 3 * radius * (radius + 1)
                };
                let totals = (1..=u32::from(player_count))
                    .map(|owner| {
                        first
                            .cells
                            .cells()
                            .filter(|cell| cell.owner == Some(owner))
                            .fold((0_u64, 0_u64, 0_u64), |totals, cell| {
                                (
                                    totals.0 + 1,
                                    totals.1 + cell.force(),
                                    totals.2 + cell.civilian_population,
                                )
                            })
                    })
                    .collect::<BTreeSet<_>>();
                assert_eq!(totals.len(), 1, "{preset:?} {player_count}p is unfair");
                assert_eq!(
                    totals.iter().next().map(|row| row.0),
                    Some(footprint),
                    "{preset:?} {player_count}p footprint"
                );
            }
        }
    }

    #[test]
    fn axial_spawn_target_ring_contains_each_hex_distance_exactly_once() {
        for radius in 1..=16 {
            let ring = axial_ring(radius);
            assert_eq!(ring.len(), usize::try_from(6 * radius).unwrap());
            assert_eq!(
                ring.iter().copied().collect::<BTreeSet<_>>().len(),
                ring.len()
            );
            assert!(ring.iter().all(
                |coordinate| coordinate.distance(Axial::ZERO) == u64::try_from(radius).unwrap()
            ));
        }
    }

    #[test]
    fn two_player_wrappers_retain_the_existing_output() {
        assert_eq!(
            generate_preset(MapPreset::Dev),
            generate_preset_for_players(MapPreset::Dev, 2)
        );
        assert_eq!(
            generate("custom", 32, 32, 9),
            generate_for_players("custom", 32, 32, 9, 2)
        );
    }

    #[test]
    fn presets_expose_required_sizes() {
        assert_eq!(MapPreset::Dev.dimensions(), (64, 64));
        assert_eq!(MapPreset::Playtest.dimensions(), (128, 128));
        assert_eq!(MapPreset::Validation.dimensions(), (192, 192));
    }

    #[test]
    fn presets_match_the_curated_catalog_contract() {
        let expected = [
            (MapPreset::Dev, "dev-stepped-island", 265_511_937),
            (
                MapPreset::Playtest,
                "playtest-stepped-island",
                4_195_428_354,
            ),
            (
                MapPreset::Validation,
                "validation-stepped-island",
                4_195_455_491,
            ),
        ];
        for (preset, name, seed) in expected {
            assert_eq!(preset.name(), name);
            assert_eq!(preset.seed(), seed);
            assert_eq!(generate_preset(preset).manifest.generator_version, 1);
        }

        assert_eq!(
            serde_json::to_string(&MapPreset::Dev).expect("preset should serialize"),
            r#""Dev""#
        );
    }

    #[test]
    fn generated_map_json_is_stable_array_based_and_round_trips() {
        let generated = generate_preset(MapPreset::Dev);
        let first = serde_json::to_vec(&generated).expect("generated map should serialize");
        let second = serde_json::to_vec(&generated).expect("serialization should repeat");
        assert_eq!(first, second);

        let value: serde_json::Value =
            serde_json::from_slice(&first).expect("generated JSON should parse");
        assert!(value["cells"].is_array());
        assert_eq!(value["cells"].as_array().map(Vec::len), Some(64 * 64));

        let decoded: GeneratedMap =
            serde_json::from_slice(&first).expect("generated map should deserialize");
        assert_eq!(decoded, generated);
        assert!(validate(&decoded).is_ok());
    }

    #[test]
    fn legacy_version_one_manifest_json_defaults_to_two_players() {
        let fixture = r#"{
            "name":"legacy", "generator_version":1, "width":64, "height":64,
            "q_min":-32, "r_min":-32, "seed":1, "content_hash":2,
            "capturable_land":3,
            "spawn_cells":[{"q":-16,"r":0},{"q":16,"r":0}]
        }"#;
        let manifest: MapManifest =
            serde_json::from_str(fixture).expect("legacy version-1 JSON should deserialize");
        assert_eq!(manifest.player_count, DEFAULT_PLAYER_COUNT);
        assert_eq!(manifest.spawn_cells.len(), 2);

        let generated = generate_preset(MapPreset::Dev);
        let mut legacy_wire = serde_json::to_value(&generated).expect("map should serialize");
        legacy_wire["manifest"]
            .as_object_mut()
            .expect("manifest should be an object")
            .remove("player_count");
        let decoded: GeneratedMap =
            serde_json::from_value(legacy_wire).expect("legacy generated-map JSON should decode");
        assert_eq!(decoded, generated);
    }

    #[test]
    fn generated_map_json_rejects_duplicate_coordinates() {
        let generated = generate_preset(MapPreset::Dev);
        let mut value = serde_json::to_value(generated).expect("generated map should serialize");
        let cells = value["cells"]
            .as_array_mut()
            .expect("wire cells should be an array");
        cells.push(cells[0].clone());
        let error = serde_json::from_value::<GeneratedMap>(value)
            .expect_err("duplicate coordinates must be rejected");
        assert!(error.to_string().contains("duplicate map cell"));
    }

    #[test]
    fn validation_rejects_stale_hash_and_manifest_metadata() {
        let mut changed = generate_preset(MapPreset::Dev);
        changed
            .cells
            .get_mut(changed.manifest.spawn_cells[0])
            .expect("spawn exists")
            .civilian_population -= 1;
        assert!(
            validate(&changed)
                .expect_err("changed content must invalidate the hash")
                .contains("content hash mismatch")
        );

        changed.manifest.content_hash = content_hash(
            &changed.cells,
            changed.manifest.width,
            changed.manifest.height,
            changed.manifest.seed,
        );
        assert!(
            validate(&changed)
                .expect_err("hash-consistent unfair totals must be rejected")
                .contains("equal initial controlled, infantry, and civilian totals")
        );

        let mut wrong_version = generate_preset(MapPreset::Dev);
        wrong_version.manifest.generator_version += 1;
        assert!(
            validate(&wrong_version)
                .expect_err("unknown generator version must fail")
                .contains("unsupported generator version")
        );
    }

    #[test]
    fn supported_custom_dimensions_and_seed_sweep_validate_without_panics() {
        for seed in 0..32 {
            let generated = generate("seed-sweep", 24, 31, seed);
            validate(&generated).expect("supported generated map should validate");
        }
    }

    #[test]
    #[should_panic(expected = "maps must be at least 24 by 24")]
    fn too_narrow_dimensions_are_the_documented_panic() {
        let _ = generate("invalid", 23, 24, 0);
    }

    #[test]
    #[should_panic(expected = "maps must be at least 24 by 24")]
    fn too_short_dimensions_are_the_documented_panic() {
        let _ = generate("invalid", 24, 23, 0);
    }

    #[test]
    fn legacy_two_player_minimum_boundary_remains_supported() {
        let generated = generate("minimum-two-player", 24, 24, 0);
        validate(&generated).expect("24 by 24 should remain valid for two players");
    }

    #[test]
    fn multiplayer_minimum_boundary_supports_eight_players() {
        let generated = generate_for_players("minimum-multiplayer", 32, 32, 0, 8);
        validate(&generated).expect("32 by 32 should support every configured player count");
    }

    #[test]
    #[should_panic(expected = "maps with more than two players must be at least 32 by 32")]
    fn multiplayer_dimension_below_minimum_is_rejected_up_front() {
        let _ = generate_for_players("invalid", 31, 32, 0, 8);
    }

    #[test]
    #[should_panic(expected = "player count must be between 2 and 500")]
    fn unsupported_player_count_is_rejected() {
        let _ = generate_for_players("invalid", 24, 24, 0, 501);
    }

    #[test]
    fn nine_players_use_one_cell_high_scale_spawns() {
        let generated = generate_preset_for_players(MapPreset::Dev, 9);
        validate(&generated).expect("9p should validate");
        for owner in 1..=9 {
            let owned = generated
                .cells
                .cells()
                .filter(|cell| cell.owner == Some(owner))
                .count();
            assert_eq!(owned, 1, "player {owner} should own one cell");
        }
    }

    #[test]
    fn high_scale_spawns_use_one_uniform_capacity_tier_never_undersized() {
        for preset in [MapPreset::Dev, MapPreset::Playtest, MapPreset::Validation] {
            for player_count in [9_u16, 32, 128, 500] {
                let generated = generate_preset_for_players(preset, player_count);
                validate(&generated).expect("high-scale map should validate");
                let mut capacities = BTreeSet::new();
                for spawn in &generated.manifest.spawn_cells {
                    let cell = generated.cells.get(*spawn).expect("spawn cell");
                    assert!(
                        cell.military_capacity >= HIGH_SCALE_SEED_INFANTRY,
                        "{preset:?} {player_count}p spawn capacity {} < seed {HIGH_SCALE_SEED_INFANTRY}",
                        cell.military_capacity
                    );
                    assert_ne!(
                        cell.military_capacity, 60,
                        "{preset:?} {player_count}p must never seed into mountain capacity 60"
                    );
                    assert_eq!(cell.force(), HIGH_SCALE_SEED_INFANTRY);
                    capacities.insert(cell.military_capacity);
                }
                assert_eq!(
                    capacities.len(),
                    1,
                    "{preset:?} {player_count}p mixed capacity tiers: {capacities:?}"
                );
                assert_eq!(
                    capacities,
                    BTreeSet::from([HIGH_SCALE_MILITARY_CAPACITY]),
                    "{preset:?} {player_count}p spawn capacity was not normalized"
                );
                for spawn in &generated.manifest.spawn_cells {
                    assert!(
                        spawn.neighbors().into_iter().any(|neighbor| {
                            generated.cells.get(neighbor).is_some_and(|cell| {
                                cell.capturable
                                    && cell.terrain.ground_passable()
                                    && cell.owner.is_none()
                            })
                        }),
                        "{preset:?} {player_count}p spawn {spawn:?} has no neutral expansion frontage"
                    );
                }
            }
        }
    }

    #[test]
    fn high_scale_candidates_have_expansion_frontage() {
        let generated = generate_preset(MapPreset::Dev);
        let candidates = high_scale_spawn_candidates(&generated.cells);
        assert!(candidates.len() >= 500);
        for coordinate in candidates {
            assert!(coordinate.neighbors().into_iter().any(|neighbor| {
                generated
                    .cells
                    .get(neighbor)
                    .is_some_and(|cell| cell.capturable && cell.terrain.ground_passable())
            }));
        }
    }
}
