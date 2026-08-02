//! Deterministic stepped-island generation shared by tools, server, and client fixtures.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;

use hex_core::{
    Axial, Cell, ForceComposition, HexMap, MovementConfig, TerrainKind, connected_components,
    shortest_path,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

pub const PLAYER_ONE: u32 = 1;
pub const PLAYER_TWO: u32 = 2;
pub const GENERATOR_VERSION: u16 = 1;

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
    pub spawn_cells: [Axial; 2],
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
    let (width, height) = preset.dimensions();
    generate(preset.name(), width, height, preset.seed())
}

/// Generates a deterministic rectangular map containing one stepped island.
///
/// # Panics
///
/// Panics when either dimension is smaller than 24 cells. Curated presets are
/// all larger than this minimum.
#[must_use]
pub fn generate(name: impl Into<String>, width: u16, height: u16, seed: u64) -> GeneratedMap {
    assert!(
        width >= 24 && height >= 24,
        "maps must be at least 24 by 24"
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

    let spawn_one = nearest_capturable(
        &cells,
        Axial::new(q_min + i32::from(width / 4), r_min + i32::from(height / 2)),
    );
    let spawn_two = nearest_capturable(
        &cells,
        Axial::new(
            q_min + i32::from(width.saturating_mul(3) / 4),
            r_min + i32::from(height / 2),
        ),
    );
    seed_players(&mut cells, [spawn_one, spawn_two], width.min(height));

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
            spawn_cells: [spawn_one, spawn_two],
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
    validate_cell_state(map)?;

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

fn validate_cell_state(map: &HexMap) -> Result<(), String> {
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
            .is_some_and(|owner| ![PLAYER_ONE, PLAYER_TWO].contains(&owner))
        {
            return Err(format!(
                "cell ({}, {}) has an unsupported owner",
                cell.coordinate.q, cell.coordinate.r
            ));
        }
    }
    Ok(())
}

fn validate_spawns(
    map: &HexMap,
    manifest: &MapManifest,
    movement: &MovementConfig,
) -> Result<(), String> {
    if manifest.spawn_cells[0] == manifest.spawn_cells[1] {
        return Err("player spawns must be distinct".to_owned());
    }
    for (index, (spawn, owner)) in manifest
        .spawn_cells
        .iter()
        .zip([PLAYER_ONE, PLAYER_TWO])
        .enumerate()
    {
        let Some(cell) = map.get(*spawn) else {
            return Err(format!("spawn {} is missing", index + 1));
        };
        if !cell.capturable || cell.owner != Some(owner) {
            return Err(format!("spawn {} is not valid owned land", index + 1));
        }
    }

    if shortest_path(
        map,
        manifest.spawn_cells[0],
        manifest.spawn_cells[1],
        movement,
        |cell| cell.capturable,
    )
    .is_none()
    {
        return Err("player spawns have no traversable ground path".to_owned());
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

fn seed_players(map: &mut HexMap, spawns: [Axial; 2], short_dimension: u16) {
    let radius = (u64::from(short_dimension) / 24).max(2);
    let coordinates: Vec<_> = map.coordinates().collect();
    for coordinate in coordinates {
        let Some(cell) = map.get_mut(coordinate) else {
            continue;
        };
        if !cell.capturable {
            continue;
        }
        let owner = if coordinate.distance(spawns[0]) <= radius {
            Some(PLAYER_ONE)
        } else if coordinate.distance(spawns[1]) <= radius {
            Some(PLAYER_TWO)
        } else {
            None
        };
        if let Some(owner) = owner {
            cell.owner = Some(owner);
            let distance = coordinate.distance(spawns[usize::from(owner == PLAYER_TWO)]);
            cell.civilian_capacity = if distance == 0 { 240 } else { 140 };
            cell.civilian_population = if distance == 0 { 180 } else { 80 };
            cell.forces = ForceComposition::infantry(if distance == 0 { 70 } else { 35 });
        }
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
}
