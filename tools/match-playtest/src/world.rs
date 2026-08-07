//! Read-side world geometry over one client-cache snapshot.
//!
//! Every scenario decision (candidate selection, expected Share accounting,
//! strategic front derivation) works on an immutable [`WorldSnapshot`] taken
//! from the SDK cache, mirroring the authoritative rules in
//! `modules/match/src/rules.rs` for traversability and availability.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use anyhow::{Context, Result};
use hex_core::{
    Axial, StrategicExterior, UNIFORM_ALLOCATION_WEIGHT, redistribution_targets_dense_with_weights,
    strategic_front_index_for_seed, strategic_fronts,
};
use match_bindings::{
    CellStateTableAccess, CellTerrainTableAccess, DbConnection, MatchConfig,
    MatchConfigTableAccess, TerrainClass, TransitPacketTableAccess,
};
use spacetimedb_sdk::Table;

pub const SINGLETON_ID: u8 = 0;
pub const NEUTRAL_PLAYER: u16 = 0;

/// One cell merged from the public terrain and state tables.
#[derive(Clone, Debug)]
pub struct WorldCell {
    pub cell_id: u32,
    pub coordinate: Axial,
    pub terrain: TerrainClass,
    pub elevation: i16,
    pub passable: bool,
    pub capturable: bool,
    pub owner: u16,
    pub infantry: u64,
    pub military_capacity: u64,
}

pub struct WorldSnapshot {
    pub config: MatchConfig,
    pub cells: HashMap<u32, WorldCell>,
    pub by_coordinate: HashMap<Axial, u32>,
    /// Live transit-packet strength per (owner, cell), i.e. allocated infantry
    /// that is unavailable to a later Share commitment.
    pub allocated: HashMap<(u16, u32), u64>,
}

impl WorldSnapshot {
    pub fn capture(conn: &DbConnection) -> Result<Self> {
        let config = conn
            .db
            .match_config()
            .singleton_id()
            .find(&SINGLETON_ID)
            .context("match config is missing from the client cache")?;
        let mut cells = HashMap::new();
        let mut by_coordinate = HashMap::new();
        let states: HashMap<u32, _> = conn
            .db
            .cell_state()
            .iter()
            .map(|state| (state.cell_id, state))
            .collect();
        for terrain in conn.db.cell_terrain().iter() {
            let Some(state) = states.get(&terrain.cell_id) else {
                continue;
            };
            let coordinate = Axial::new(terrain.q, terrain.r);
            by_coordinate.insert(coordinate, terrain.cell_id);
            cells.insert(
                terrain.cell_id,
                WorldCell {
                    cell_id: terrain.cell_id,
                    coordinate,
                    terrain: terrain.terrain,
                    elevation: terrain.elevation,
                    passable: terrain.passable,
                    capturable: terrain.capturable,
                    owner: state.owner_player_id,
                    infantry: state.infantry,
                    military_capacity: state.military_capacity,
                },
            );
        }
        let mut allocated = HashMap::new();
        for packet in conn.db.transit_packet().iter() {
            *allocated
                .entry((packet.owner_player_id, packet.current_cell))
                .or_insert(0) += packet.infantry;
        }
        Ok(Self {
            config,
            cells,
            by_coordinate,
            allocated,
        })
    }

    pub fn cell(&self, cell_id: u32) -> Result<&WorldCell> {
        self.cells
            .get(&cell_id)
            .with_context(|| format!("cell {cell_id} is missing from the snapshot"))
    }

    pub fn neighbor_ids(&self, cell_id: u32) -> Vec<u32> {
        let Some(cell) = self.cells.get(&cell_id) else {
            return Vec::new();
        };
        let mut ids: Vec<u32> = cell
            .coordinate
            .neighbors()
            .into_iter()
            .filter_map(|coordinate| self.by_coordinate.get(&coordinate).copied())
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Mirrors the authoritative ground-traversal rule: both endpoints
    /// passable and the elevation step within the configured limit.
    pub fn edge_traversable(&self, from: u32, to: u32) -> bool {
        let (Some(a), Some(b)) = (self.cells.get(&from), self.cells.get(&to)) else {
            return false;
        };
        a.passable
            && b.passable
            && a.elevation.abs_diff(b.elevation) <= u16::from(self.config.max_elevation_step)
    }

    /// Free (unallocated) infantry available to a new Share commitment.
    pub fn available_infantry(&self, player: u16, cell_id: u32) -> u64 {
        let Some(cell) = self.cells.get(&cell_id) else {
            return 0;
        };
        cell.infantry
            .saturating_sub(self.allocated.get(&(player, cell_id)).copied().unwrap_or(0))
    }

    /// Complete owned traversable components, ordered by their lowest cell ID.
    pub fn owned_components(&self, player: u16) -> Vec<BTreeSet<u32>> {
        let mut unvisited: BTreeSet<u32> = self
            .cells
            .values()
            .filter(|cell| cell.owner == player && cell.passable)
            .map(|cell| cell.cell_id)
            .collect();
        let mut components = Vec::new();
        while let Some(seed) = unvisited.pop_first() {
            let mut component = BTreeSet::from([seed]);
            let mut pending = VecDeque::from([seed]);
            while let Some(current) = pending.pop_front() {
                for neighbor in self.neighbor_ids(current) {
                    if unvisited.contains(&neighbor) && self.edge_traversable(current, neighbor) {
                        unvisited.remove(&neighbor);
                        component.insert(neighbor);
                        pending.push_back(neighbor);
                    }
                }
            }
            components.push(component);
        }
        components.sort_by_key(|component| component.first().copied().unwrap_or(u32::MAX));
        components
    }

    /// Directed eligible neutral perimeter edges of a component: owned source
    /// cell to unclaimed passable capturable traversable neighbor.
    pub fn neutral_perimeter_edges(&self, component: &BTreeSet<u32>) -> Vec<(u32, u32)> {
        let mut edges = Vec::new();
        for &source in component {
            for target in self.neighbor_ids(source) {
                if component.contains(&target) {
                    continue;
                }
                let Some(cell) = self.cells.get(&target) else {
                    continue;
                };
                if cell.owner == NEUTRAL_PLAYER
                    && cell.passable
                    && cell.capturable
                    && self.edge_traversable(source, target)
                {
                    edges.push((source, target));
                }
            }
        }
        edges.sort_unstable();
        edges.dedup();
        edges
    }

    /// Whether the two players currently share a traversable hostile front.
    pub fn players_share_front(&self, player: u16, enemy: u16) -> bool {
        self.cells.values().any(|cell| {
            cell.owner == player
                && self.neighbor_ids(cell.cell_id).into_iter().any(|neighbor| {
                    self.cells
                        .get(&neighbor)
                        .is_some_and(|other| other.owner == enemy)
                        && self.edge_traversable(cell.cell_id, neighbor)
                })
        })
    }

    /// Nearest neutral cell (BFS through neutral traversable ground from the
    /// component perimeter) that touches enemy territory. Returns
    /// `(distance, focus_cell)`.
    pub fn nearest_neutral_focus_toward_enemy(
        &self,
        component: &BTreeSet<u32>,
        enemy: u16,
    ) -> Option<(u32, u32)> {
        let mut reached = BTreeSet::new();
        let mut pending = VecDeque::new();
        for (_, target) in self.neutral_perimeter_edges(component) {
            if reached.insert(target) {
                pending.push_back((target, 1_u32));
            }
        }
        while let Some((current, distance)) = pending.pop_front() {
            let neighbors = self.neighbor_ids(current);
            if neighbors.iter().any(|&neighbor| {
                self.cells
                    .get(&neighbor)
                    .is_some_and(|cell| cell.owner == enemy)
                    && self.edge_traversable(current, neighbor)
            }) {
                return Some((distance, current));
            }
            for neighbor in neighbors {
                let Some(cell) = self.cells.get(&neighbor) else {
                    continue;
                };
                if cell.owner == NEUTRAL_PLAYER
                    && cell.passable
                    && cell.capturable
                    && self.edge_traversable(current, neighbor)
                    && reached.insert(neighbor)
                {
                    pending.push_back((neighbor, distance.saturating_add(1)));
                }
            }
        }
        None
    }
}

pub fn basis_point_share(value: u64, basis_points: u32) -> u64 {
    u64::try_from(u128::from(value) * u128::from(basis_points) / 10_000)
        .expect("basis-point share cannot exceed the input value")
}

/// Expected Share-once commitments for a set of participating source cells.
pub fn expected_shares(
    snapshot: &WorldSnapshot,
    player: u16,
    sources: &BTreeSet<u32>,
    commitment_bps: u32,
) -> BTreeMap<u32, u64> {
    sources
        .iter()
        .map(|&cell_id| {
            (
                cell_id,
                basis_point_share(snapshot.available_infantry(player, cell_id), commitment_bps),
            )
        })
        .collect()
}

/// One planned front-rebalance payload plus the expectation baseline used by
/// the scenario asserts. The derivation mirrors `plan_front_rebalance` in
/// `modules/match/src/orders.rs` and the front-seed selection proven in
/// `tools/match-perf`.
#[derive(Clone, Debug)]
pub struct FrontRebalancePlan {
    pub source_front_seed: u32,
    pub target_front_seed: u32,
    pub source_front_cells: BTreeSet<u32>,
    pub target_front_cells: BTreeSet<u32>,
    pub front_count: usize,
}

pub fn plan_front_rebalance(
    snapshot: &WorldSnapshot,
    player: u16,
    component: &BTreeSet<u32>,
) -> Result<FrontRebalancePlan> {
    let component_coordinates: BTreeSet<Axial> = component
        .iter()
        .filter_map(|cell_id| snapshot.cells.get(cell_id).map(|cell| cell.coordinate))
        .collect();
    anyhow::ensure!(
        component_coordinates.len() == component.len(),
        "front rebalance component has cells without terrain"
    );
    let classify = |source: Axial, target: Axial| -> StrategicExterior {
        let Some(&source_id) = snapshot.by_coordinate.get(&source) else {
            return StrategicExterior::Ignored;
        };
        let Some(&target_id) = snapshot.by_coordinate.get(&target) else {
            return StrategicExterior::Ignored;
        };
        let Some(target_cell) = snapshot.cells.get(&target_id) else {
            return StrategicExterior::Ignored;
        };
        if !target_cell.passable
            || !target_cell.capturable
            || !snapshot.edge_traversable(source_id, target_id)
            || target_cell.owner == player
        {
            return StrategicExterior::Ignored;
        }
        if target_cell.owner == NEUTRAL_PLAYER {
            StrategicExterior::Neutral
        } else {
            StrategicExterior::Opponent(u32::from(target_cell.owner))
        }
    };
    let fronts = strategic_fronts(component_coordinates.iter().copied(), |source, target| {
        classify(source, target)
    })
    .map_err(|error| anyhow::anyhow!("component has no strategic boundary: {error:?}"))?;
    anyhow::ensure!(
        fronts.len() >= 2,
        "component exposes only {} strategic front(s)",
        fronts.len()
    );

    let mut resolvable = Vec::new();
    for index in 0..fronts.len() {
        let mut seed_candidates: Vec<(u32, Axial)> = fronts[index]
            .source_cells()
            .into_iter()
            .filter_map(|coordinate| {
                snapshot
                    .by_coordinate
                    .get(&coordinate)
                    .map(|&cell_id| (cell_id, coordinate))
            })
            .collect();
        seed_candidates.sort_unstable_by_key(|(cell_id, _)| *cell_id);
        for (cell_id, coordinate) in seed_candidates {
            if strategic_front_index_for_seed(&fronts, coordinate) == Some(index) {
                resolvable.push((index, cell_id));
                break;
            }
        }
    }
    anyhow::ensure!(
        resolvable.len() >= 2,
        "could not resolve two distinct strategic front seeds"
    );

    let front_cell_ids = |index: usize| -> BTreeSet<u32> {
        fronts[index]
            .source_cells()
            .into_iter()
            .filter_map(|coordinate| snapshot.by_coordinate.get(&coordinate).copied())
            .collect()
    };
    let mut candidates: Vec<(u32, FrontRebalancePlan)> = Vec::new();
    for &(source_index, source_seed) in &resolvable {
        let source_cells = front_cell_ids(source_index);
        for &(target_index, target_seed) in &resolvable {
            if source_index == target_index || source_seed == target_seed {
                continue;
            }
            let target_cells = front_cell_ids(target_index);
            let movable_sources: BTreeSet<u32> = source_cells
                .difference(&target_cells)
                .copied()
                .filter(|&cell_id| snapshot.available_infantry(player, cell_id) > 0)
                .collect();
            let target_headroom = target_cells.iter().any(|&cell_id| {
                snapshot
                    .cells
                    .get(&cell_id)
                    .is_some_and(|cell| cell.infantry < cell.military_capacity)
            });
            if !movable_sources.is_empty() && target_headroom {
                // Prefer the longest source→target hop so client polling can
                // observe route-index progression instead of an instant settle.
                let mut route_score = 0_u32;
                for &from in &movable_sources {
                    for &to in &target_cells {
                        if let Some(distance) =
                            component_bfs_distance(snapshot, component, from, to)
                        {
                            route_score = route_score.max(distance);
                        }
                    }
                }
                candidates.push((
                    route_score,
                    FrontRebalancePlan {
                        source_front_seed: source_seed,
                        target_front_seed: target_seed,
                        source_front_cells: source_cells
                            .difference(&target_cells)
                            .copied()
                            .collect(),
                        target_front_cells: target_cells,
                        front_count: fronts.len(),
                    },
                ));
            }
        }
    }
    candidates.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    candidates
        .into_iter()
        .next()
        .map(|(_, plan)| plan)
        .ok_or_else(|| {
            anyhow::anyhow!("no strategic front pair has movable troops and destination headroom")
        })
}

fn component_bfs_distance(
    snapshot: &WorldSnapshot,
    component: &BTreeSet<u32>,
    from: u32,
    to: u32,
) -> Option<u32> {
    let mut reached: BTreeMap<u32, u32> = BTreeMap::from([(from, 0)]);
    let mut pending = VecDeque::from([from]);
    while let Some(current) = pending.pop_front() {
        let distance = reached[&current];
        if current == to {
            return Some(distance);
        }
        for neighbor in snapshot.neighbor_ids(current) {
            if component.contains(&neighbor)
                && snapshot.edge_traversable(current, neighbor)
                && !reached.contains_key(&neighbor)
            {
                reached.insert(neighbor, distance.saturating_add(1));
                pending.push_back(neighbor);
            }
        }
    }
    None
}

/// Mirrors capped Share-once source commitments for [`issue_front_rebalance`].
/// Returns `(per-source commits, headroom_capped)`.
pub fn expected_front_rebalance_commits(
    snapshot: &WorldSnapshot,
    player: u16,
    plan: &FrontRebalancePlan,
    commitment_bps: u32,
) -> Result<(BTreeMap<u32, u64>, bool)> {
    let mut source_limits = BTreeMap::new();
    let mut total_supply = 0_u64;
    for &cell_id in &plan.source_front_cells {
        let share = basis_point_share(snapshot.available_infantry(player, cell_id), commitment_bps);
        if share > 0 {
            source_limits.insert(cell_id, share);
            total_supply = total_supply.saturating_add(share);
        }
    }
    let mut total_headroom = 0_u64;
    for &cell_id in &plan.target_front_cells {
        let cell = snapshot
            .cell(cell_id)
            .with_context(|| format!("target front cell {cell_id} is missing"))?;
        total_headroom =
            total_headroom.saturating_add(cell.military_capacity.saturating_sub(cell.infantry));
    }
    let deliverable = total_supply.min(total_headroom);
    let capped = deliverable < total_supply;
    if capped && deliverable > 0 {
        let entries: Vec<(u32, u64)> = source_limits.into_iter().collect();
        let coordinates: Vec<Axial> = entries
            .iter()
            .map(|(cell_id, _)| snapshot.cell(*cell_id).map(|cell| cell.coordinate))
            .collect::<Result<_>>()?;
        let capacities: Vec<u64> = entries.iter().map(|(_, limit)| *limit).collect();
        let distribution = redistribution_targets_dense_with_weights(
            &coordinates,
            &capacities,
            deliverable,
            vec![UNIFORM_ALLOCATION_WEIGHT; entries.len()],
        )
        .map_err(|error| anyhow::anyhow!("front rebalance source cap mirror failed: {error:?}"))?;
        source_limits = entries
            .into_iter()
            .zip(distribution.targets)
            .filter(|(_, amount)| *amount > 0)
            .map(|((cell_id, _), amount)| (cell_id, amount))
            .collect();
    }
    Ok((source_limits, capped))
}
