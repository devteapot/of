use std::collections::BTreeMap;

use crate::coord::{Axial, HexEdge};

pub type Strength = u64;
pub type PlayerId = u32;

/// V1 carries infantry only, while retaining a composition-shaped state.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ForceComposition {
    pub infantry: Strength,
}

impl ForceComposition {
    pub const fn infantry(strength: Strength) -> Self {
        Self { infantry: strength }
    }

    /// Infantry has a capacity weight of one in V1.
    pub const fn weighted_strength(self) -> Strength {
        self.infantry
    }
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum TerrainKind {
    #[default]
    Plains,
    Hills,
    Mountain,
    Water,
}

impl TerrainKind {
    pub const fn ground_passable(self) -> bool {
        !matches!(self, Self::Water)
    }
}

/// Static and dynamic state at one authoritative gameplay hex.
///
/// Deserialization re-validates constructor invariants (capacity bounds and
/// water-cell emptiness) so a snapshot cannot smuggle in a cell that
/// API-built state could never contain.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(try_from = "RawCell"))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cell {
    pub coordinate: Axial,
    pub terrain: TerrainKind,
    pub elevation: i16,
    pub capturable: bool,
    pub habitable: bool,
    pub owner: Option<PlayerId>,
    pub civilian_population: u64,
    pub civilian_capacity: u64,
    pub forces: ForceComposition,
    pub military_capacity: Strength,
}

#[cfg(feature = "serde")]
#[derive(serde::Deserialize)]
struct RawCell {
    coordinate: Axial,
    terrain: TerrainKind,
    elevation: i16,
    capturable: bool,
    habitable: bool,
    owner: Option<PlayerId>,
    civilian_population: u64,
    civilian_capacity: u64,
    forces: ForceComposition,
    military_capacity: Strength,
}

#[cfg(feature = "serde")]
impl TryFrom<RawCell> for Cell {
    type Error = String;

    fn try_from(raw: RawCell) -> Result<Self, Self::Error> {
        let cell = Self {
            coordinate: raw.coordinate,
            terrain: raw.terrain,
            elevation: raw.elevation,
            capturable: raw.capturable,
            habitable: raw.habitable,
            owner: raw.owner,
            civilian_population: raw.civilian_population,
            civilian_capacity: raw.civilian_capacity,
            forces: raw.forces,
            military_capacity: raw.military_capacity,
        };
        cell.validate()?;
        Ok(cell)
    }
}

impl Cell {
    pub fn ground(
        coordinate: Axial,
        elevation: i16,
        owner: Option<PlayerId>,
        military_capacity: Strength,
    ) -> Self {
        Self {
            coordinate,
            terrain: TerrainKind::Plains,
            elevation,
            capturable: true,
            habitable: true,
            owner,
            civilian_population: 0,
            civilian_capacity: 0,
            forces: ForceComposition::default(),
            military_capacity,
        }
    }

    pub fn water(coordinate: Axial, elevation: i16) -> Self {
        Self {
            coordinate,
            terrain: TerrainKind::Water,
            elevation,
            capturable: false,
            habitable: false,
            owner: None,
            civilian_population: 0,
            civilian_capacity: 0,
            forces: ForceComposition::default(),
            military_capacity: 0,
        }
    }

    /// Rejects capacity overflows and water cells that carry land state.
    pub fn validate(&self) -> Result<(), String> {
        if self.force() > self.military_capacity {
            return Err(format!(
                "cell {:?} force {} exceeds military capacity {}",
                self.coordinate,
                self.force(),
                self.military_capacity
            ));
        }
        if self.civilian_population > self.civilian_capacity {
            return Err(format!(
                "cell {:?} civilian population {} exceeds capacity {}",
                self.coordinate, self.civilian_population, self.civilian_capacity
            ));
        }
        if self.terrain == TerrainKind::Water
            && (self.capturable
                || self.habitable
                || self.owner.is_some()
                || self.military_capacity != 0
                || self.force() != 0
                || self.civilian_population != 0
                || self.civilian_capacity != 0)
        {
            return Err(format!(
                "water cell {:?} must be empty, unowned, and non-capturable",
                self.coordinate
            ));
        }
        Ok(())
    }

    pub const fn force(&self) -> Strength {
        self.forces.weighted_strength()
    }

    pub const fn free_military_capacity(&self) -> Strength {
        self.military_capacity.saturating_sub(self.force())
    }
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MovementConfig {
    pub max_elevation_step: u16,
    pub level_cost: u32,
    pub uphill_cost: u32,
    pub downhill_cost: u32,
}

impl Default for MovementConfig {
    fn default() -> Self {
        Self {
            max_elevation_step: 1,
            level_cost: 10,
            uphill_cost: 15,
            downhill_cost: 10,
        }
    }
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Traversal {
    pub cost: u32,
    pub elevation_delta: i32,
}

impl Traversal {
    pub const fn is_uphill(self) -> bool {
        self.elevation_delta > 0
    }
}

/// Checks adjacency, terrain passability, and the V1 one-level slope rule.
pub fn ground_traversal(from: &Cell, to: &Cell, config: &MovementConfig) -> Option<Traversal> {
    if from.coordinate.distance(to.coordinate) != 1
        || !from.terrain.ground_passable()
        || !to.terrain.ground_passable()
    {
        return None;
    }

    let delta = i32::from(to.elevation) - i32::from(from.elevation);
    if delta.unsigned_abs() > u32::from(config.max_elevation_step) {
        return None;
    }

    let cost = match delta.cmp(&0) {
        std::cmp::Ordering::Greater => config.uphill_cost,
        std::cmp::Ordering::Less => config.downhill_cost,
        std::cmp::Ordering::Equal => config.level_cost,
    };
    Some(Traversal {
        cost,
        elevation_delta: delta,
    })
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EdgeLimits {
    /// Strength that may cross the undirected edge in one logical step.
    pub throughput: Strength,
    /// Strength that may actively fight across the edge in one combat step.
    pub frontage: Strength,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogisticsConfig {
    pub default_military_capacity: Strength,
    pub default_edge_throughput: Strength,
    pub default_combat_frontage: Strength,
}

impl Default for LogisticsConfig {
    fn default() -> Self {
        Self {
            default_military_capacity: 100,
            default_edge_throughput: 20,
            default_combat_frontage: 25,
        }
    }
}

impl LogisticsConfig {
    pub const fn default_edge_limits(self) -> EdgeLimits {
        EdgeLimits {
            throughput: self.default_edge_throughput,
            frontage: self.default_combat_frontage,
        }
    }
}

/// A deterministic map container. Ordered maps keep iteration stable everywhere.
///
/// Deserialization re-validates the constructor invariants: every cell must be
/// keyed by its own coordinate and every edge-limit override must connect two
/// existing cells, so a snapshot load can never diverge from API-built state.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(try_from = "RawHexMap"))]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HexMap {
    cells: BTreeMap<Axial, Cell>,
    edge_limits: BTreeMap<HexEdge, EdgeLimits>,
}

#[cfg(feature = "serde")]
#[derive(serde::Deserialize)]
struct RawHexMap {
    cells: BTreeMap<Axial, Cell>,
    edge_limits: BTreeMap<HexEdge, EdgeLimits>,
}

#[cfg(feature = "serde")]
impl TryFrom<RawHexMap> for HexMap {
    type Error = String;

    fn try_from(raw: RawHexMap) -> Result<Self, Self::Error> {
        for (&key, cell) in &raw.cells {
            if key != cell.coordinate {
                return Err(format!(
                    "map cell keyed at {key:?} disagrees with its coordinate {:?}",
                    cell.coordinate
                ));
            }
        }
        // `HexEdge` deserialization already enforced adjacency and canonical
        // order; only endpoint existence remains map-level.
        for edge in raw.edge_limits.keys() {
            if !raw.cells.contains_key(&edge.a) || !raw.cells.contains_key(&edge.b) {
                return Err(format!(
                    "edge limits reference missing cells: {:?} and {:?}",
                    edge.a, edge.b
                ));
            }
        }
        Ok(Self {
            cells: raw.cells,
            edge_limits: raw.edge_limits,
        })
    }
}

impl HexMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, cell: Cell) -> Option<Cell> {
        self.cells.insert(cell.coordinate, cell)
    }

    pub fn get(&self, coordinate: Axial) -> Option<&Cell> {
        self.cells.get(&coordinate)
    }

    pub fn get_mut(&mut self, coordinate: Axial) -> Option<&mut Cell> {
        self.cells.get_mut(&coordinate)
    }

    pub fn contains(&self, coordinate: Axial) -> bool {
        self.cells.contains_key(&coordinate)
    }

    pub fn cells(&self) -> impl ExactSizeIterator<Item = &Cell> {
        self.cells.values()
    }

    pub fn coordinates(&self) -> impl ExactSizeIterator<Item = Axial> + '_ {
        self.cells.keys().copied()
    }

    pub fn set_edge_limits(&mut self, first: Axial, second: Axial, limits: EdgeLimits) -> bool {
        let Some(edge) = HexEdge::new(first, second) else {
            return false;
        };
        if !self.contains(first) || !self.contains(second) {
            return false;
        }
        self.edge_limits.insert(edge, limits);
        true
    }

    pub fn edge_limits(
        &self,
        first: Axial,
        second: Axial,
        config: &LogisticsConfig,
    ) -> Option<EdgeLimits> {
        let edge = HexEdge::new(first, second)?;
        if !self.contains(first) || !self.contains(second) {
            return None;
        }
        Some(
            self.edge_limits
                .get(&edge)
                .copied()
                .unwrap_or_else(|| config.default_edge_limits()),
        )
    }

    pub fn total_force(&self) -> Strength {
        self.cells().map(Cell::force).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ground_traversal_obeys_slope_and_water_rules() {
        let movement = MovementConfig::default();
        let origin = Cell::ground(Axial::ZERO, 2, None, 100);
        let level = Cell::ground(Axial::new(1, 0), 2, None, 100);
        let uphill = Cell::ground(Axial::new(0, 1), 3, None, 100);
        let cliff = Cell::ground(Axial::new(-1, 1), 4, None, 100);
        let water = Cell::water(Axial::new(-1, 0), 2);

        assert_eq!(
            ground_traversal(&origin, &level, &movement).unwrap().cost,
            10
        );
        let slope = ground_traversal(&origin, &uphill, &movement).unwrap();
        assert_eq!(slope.cost, 15);
        assert!(slope.is_uphill());
        assert_eq!(ground_traversal(&origin, &cliff, &movement), None);
        assert_eq!(ground_traversal(&origin, &water, &movement), None);
    }

    #[test]
    fn edge_limits_are_undirected_and_overridable() {
        let a = Axial::ZERO;
        let b = Axial::new(1, 0);
        let mut map = HexMap::new();
        map.insert(Cell::ground(a, 0, Some(1), 100));
        map.insert(Cell::ground(b, 0, Some(1), 100));
        let config = LogisticsConfig::default();
        assert_eq!(
            map.edge_limits(a, b, &config),
            Some(config.default_edge_limits())
        );

        let narrow = EdgeLimits {
            throughput: 3,
            frontage: 4,
        };
        assert!(map.set_edge_limits(a, b, narrow));
        assert_eq!(map.edge_limits(a, b, &config), Some(narrow));
        assert_eq!(map.edge_limits(b, a, &config), Some(narrow));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn deserialized_maps_reject_key_and_edge_invariant_violations() {
        let mut map = HexMap::new();
        map.insert(Cell::ground(Axial::ZERO, 0, Some(1), 100));
        map.insert(Cell::ground(Axial::new(1, 0), 0, Some(1), 100));
        assert!(map.set_edge_limits(
            Axial::ZERO,
            Axial::new(1, 0),
            EdgeLimits {
                throughput: 5,
                frontage: 6,
            },
        ));

        // Round trip through the raw wire shape preserves valid state.
        let raw = RawHexMap {
            cells: map.cells.clone(),
            edge_limits: map.edge_limits.clone(),
        };
        assert_eq!(HexMap::try_from(raw).unwrap(), map);

        // A cell keyed away from its own coordinate is rejected.
        let mut misfiled = map.cells.clone();
        let stray = Cell::ground(Axial::new(5, 5), 0, Some(1), 100);
        misfiled.insert(Axial::new(9, 9), stray);
        let error = HexMap::try_from(RawHexMap {
            cells: misfiled,
            edge_limits: BTreeMap::new(),
        })
        .unwrap_err();
        assert!(error.contains("disagrees"), "{error}");

        // Edge limits over missing cells are rejected.
        let dangling = HexEdge::new(Axial::new(7, 0), Axial::new(8, 0)).unwrap();
        let error = HexMap::try_from(RawHexMap {
            cells: map.cells.clone(),
            edge_limits: BTreeMap::from([(
                dangling,
                EdgeLimits {
                    throughput: 1,
                    frontage: 1,
                },
            )]),
        })
        .unwrap_err();
        assert!(error.contains("missing cells"), "{error}");
    }

    #[cfg(feature = "serde")]
    #[test]
    fn deserialized_cells_reject_capacity_and_water_invariant_violations() {
        let ground = Cell::ground(Axial::ZERO, 0, Some(1), 100);
        let water = Cell::water(Axial::new(1, 0), 0);
        assert_eq!(
            Cell::try_from(RawCell {
                coordinate: ground.coordinate,
                terrain: ground.terrain,
                elevation: ground.elevation,
                capturable: ground.capturable,
                habitable: ground.habitable,
                owner: ground.owner,
                civilian_population: ground.civilian_population,
                civilian_capacity: ground.civilian_capacity,
                forces: ground.forces,
                military_capacity: ground.military_capacity,
            })
            .unwrap(),
            ground
        );
        assert_eq!(
            Cell::try_from(RawCell {
                coordinate: water.coordinate,
                terrain: water.terrain,
                elevation: water.elevation,
                capturable: water.capturable,
                habitable: water.habitable,
                owner: water.owner,
                civilian_population: water.civilian_population,
                civilian_capacity: water.civilian_capacity,
                forces: water.forces,
                military_capacity: water.military_capacity,
            })
            .unwrap(),
            water
        );

        let over_force = Cell::try_from(RawCell {
            coordinate: Axial::ZERO,
            terrain: TerrainKind::Plains,
            elevation: 0,
            capturable: true,
            habitable: true,
            owner: Some(1),
            civilian_population: 0,
            civilian_capacity: 0,
            forces: ForceComposition::infantry(101),
            military_capacity: 100,
        })
        .unwrap_err();
        assert!(over_force.contains("military capacity"), "{over_force}");

        let over_civilians = Cell::try_from(RawCell {
            coordinate: Axial::ZERO,
            terrain: TerrainKind::Plains,
            elevation: 0,
            capturable: true,
            habitable: true,
            owner: Some(1),
            civilian_population: 11,
            civilian_capacity: 10,
            forces: ForceComposition::default(),
            military_capacity: 100,
        })
        .unwrap_err();
        assert!(
            over_civilians.contains("civilian population"),
            "{over_civilians}"
        );

        let wet_garrison = Cell::try_from(RawCell {
            coordinate: Axial::ZERO,
            terrain: TerrainKind::Water,
            elevation: 0,
            capturable: false,
            habitable: false,
            owner: None,
            civilian_population: 0,
            civilian_capacity: 0,
            forces: ForceComposition::infantry(1),
            military_capacity: 1,
        })
        .unwrap_err();
        assert!(wet_garrison.contains("water cell"), "{wet_garrison}");
    }

    #[test]
    fn force_composition_and_capacity_stay_separate() {
        let mut cell = Cell::ground(Axial::ZERO, 0, Some(1), 100);
        cell.forces = ForceComposition::infantry(35);
        assert_eq!(cell.force(), 35);
        assert_eq!(cell.free_military_capacity(), 65);
    }
}
