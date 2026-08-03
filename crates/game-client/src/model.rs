use std::collections::{BTreeMap, BTreeSet, VecDeque};

use bevy::prelude::*;
use hex_core::{Axial, ChunkCoord, TerrainKind};

use crate::geometry::chunk_of;

pub const PLAYER_ONE: u32 = 1;
pub const PLAYER_TWO: u32 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub enum ConnectionState {
    Connected,
    Syncing,
    ClaimedOffline,
    Open,
    Offline,
}

impl ConnectionState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Connected => "CONNECTED",
            Self::Syncing => "SYNCING",
            Self::ClaimedOffline => "CLAIMED OFFLINE",
            Self::Open => "OPEN SLOT",
            Self::Offline => "OFFLINE FIXTURE",
        }
    }
}

/// Whether this process may currently issue authoritative match commands.
///
/// This is intentionally separate from [`ConnectionState`]: the latter
/// describes the two persisted player slots, while a newly connected identity
/// may be subscribed as an unbound observer because both slots belong to other
/// identities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthorityState {
    Offline,
    Connecting,
    Ready,
    SlotUnavailable { reason: String },
    ConnectionUnavailable { reason: String },
}

impl AuthorityState {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Offline => "OFFLINE",
            Self::Connecting => "CONNECTING",
            Self::Ready => "READY",
            Self::SlotUnavailable { .. } => "SLOT UNAVAILABLE",
            Self::ConnectionUnavailable { .. } => "CONNECTION UNAVAILABLE",
        }
    }

    pub fn command_block_reason(&self) -> String {
        match self {
            Self::SlotUnavailable { reason } => format!("Player slot unavailable: {reason}"),
            Self::ConnectionUnavailable { reason } => {
                format!("Authoritative connection unavailable: {reason}")
            }
            Self::Offline => "Offline authority does not use the online transport".to_owned(),
            Self::Connecting | Self::Ready => "Authoritative match is still connecting".to_owned(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MatchPhase {
    Lobby,
    Running,
    Victory(u32),
}

impl MatchPhase {
    pub fn label(self, conquest_threshold_bps: u32) -> String {
        match self {
            Self::Lobby => "LOBBY · WAITING FOR PLAYER".to_owned(),
            Self::Running => format!(
                "CONQUEST · {:.0}% TO WIN",
                conquest_threshold_bps as f32 / 100.0
            ),
            Self::Victory(player) => format!("PLAYER {player} VICTORY"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CellView {
    pub coordinate: Axial,
    pub terrain: TerrainKind,
    pub elevation: i16,
    pub owner: Option<u32>,
    pub civilians: u64,
    pub infantry: u64,
    pub military_capacity: u64,
    pub blocked: bool,
}

impl CellView {
    pub const fn is_water(&self) -> bool {
        matches!(self.terrain, TerrainKind::Water)
    }

    pub const fn is_land(&self) -> bool {
        !self.is_water()
    }

    pub fn density(&self) -> f32 {
        if self.military_capacity == 0 {
            0.0
        } else {
            (self.infantry as f32 / self.military_capacity as f32).clamp(0.0, 1.0)
        }
    }

    pub const fn free_capacity(&self) -> u64 {
        self.military_capacity.saturating_sub(self.infantry)
    }
}

#[derive(Clone, Debug)]
pub struct ActiveFlow {
    pub route: Vec<Axial>,
    pub strength: u64,
    pub attacking: bool,
    pub age: f32,
    pub lifetime: f32,
}

#[derive(Clone, Debug)]
pub struct ActiveFront {
    pub friendly: Axial,
    pub hostile: Axial,
    pub intensity: f32,
    pub age: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToastKind {
    Info,
    Success,
    Rejection,
}

#[derive(Clone, Debug)]
pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub remaining: f32,
}

#[derive(Resource, Debug)]
pub struct MatchView {
    pub cells: BTreeMap<Axial, CellView>,
    /// Stable spatial index used by rendering and overlays.
    ///
    /// Keeping this alongside `cells` means chunk work scales with the cells
    /// in the chunk instead of repeatedly filtering the entire map.
    pub cells_by_chunk: BTreeMap<ChunkCoord, Vec<Axial>>,
    /// Monotonic topology generation consumed by the terrain renderer.
    /// Ordinary cell-state updates leave this unchanged; wholesale map/index
    /// replacements advance it so render-chunk reconciliation is event-driven.
    pub chunk_index_revision: u64,
    /// Advances whenever authoritative or offline cell values may have changed.
    /// Presentation caches use this instead of rescanning large selections or
    /// visible regions every frame.
    pub cell_state_revision: u64,
    pub local_player: u32,
    pub authority: AuthorityState,
    pub connection: [ConnectionState; 2],
    pub phase: MatchPhase,
    pub conquest_threshold_bps: u32,
    pub authoritative_control: Option<[u64; 2]>,
    pub capturable_cells: u64,
    pub required_control: u64,
    pub logical_step: u64,
    pub mobilization_target: f32,
    pub active_orders: usize,
    pub queued_infantry: u64,
    pub active_flows: Vec<ActiveFlow>,
    pub active_fronts: Vec<ActiveFront>,
    pub latest_result: String,
    pub order_log: VecDeque<String>,
    pub toast: Option<Toast>,
    pub dirty_chunks: BTreeSet<ChunkCoord>,
}

impl MatchView {
    pub fn connecting(preferred_player: u8) -> Self {
        Self {
            cells: BTreeMap::new(),
            cells_by_chunk: BTreeMap::new(),
            chunk_index_revision: 0,
            cell_state_revision: 0,
            local_player: u32::from(preferred_player),
            authority: AuthorityState::Connecting,
            connection: [ConnectionState::Syncing, ConnectionState::Syncing],
            phase: MatchPhase::Lobby,
            conquest_threshold_bps: 8_000,
            authoritative_control: None,
            capturable_cells: 0,
            required_control: 0,
            logical_step: 0,
            mobilization_target: 0.25,
            active_orders: 0,
            queued_infantry: 0,
            active_flows: Vec::new(),
            active_fronts: Vec::new(),
            latest_result: "Connecting to authoritative match…".to_owned(),
            order_log: VecDeque::new(),
            toast: None,
            dirty_chunks: BTreeSet::new(),
        }
    }

    pub fn offline_fixture() -> Self {
        let mut cells = BTreeMap::new();
        let radius = 19;

        for q in -radius..=radius {
            for r in -radius..=radius {
                let coordinate = Axial::new(q, r);
                let distance = coordinate.distance(Axial::ZERO) as i32;
                if distance > radius {
                    continue;
                }

                let coast_noise = i32::try_from((q * 37 + r * 61 + q * r * 3).unsigned_abs() % 7)
                    .expect("coast noise modulo seven fits in i32");
                let land = distance <= 16 || (distance == 17 && coast_noise <= 2);
                if !land {
                    cells.insert(
                        coordinate,
                        CellView {
                            coordinate,
                            terrain: TerrainKind::Water,
                            elevation: 0,
                            owner: None,
                            civilians: 0,
                            infantry: 0,
                            military_capacity: 0,
                            blocked: false,
                        },
                    );
                    continue;
                }

                let peak_a = 6_i32.saturating_sub(coordinate.distance(Axial::new(-2, -4)) as i32);
                let peak_b = 7_i32.saturating_sub(coordinate.distance(Axial::new(7, 1)) as i32);
                let ridge = if (-1..=1).contains(&(q + r)) && (-8..=8).contains(&q) {
                    2
                } else {
                    0
                };
                let elevation =
                    (1 + peak_a.max(0) / 2 + peak_b.max(0) / 2 + ridge).clamp(1, 6) as i16;
                let terrain = if elevation >= 5 {
                    TerrainKind::Mountain
                } else if elevation >= 3 {
                    TerrainKind::Hills
                } else {
                    TerrainKind::Plains
                };
                let owner = if q <= -5 {
                    Some(PLAYER_ONE)
                } else if q >= 6 {
                    Some(PLAYER_TWO)
                } else {
                    None
                };
                let capacity = match terrain {
                    TerrainKind::Plains => 100,
                    TerrainKind::Hills => 82,
                    TerrainKind::Mountain => 58,
                    TerrainKind::Water => 0,
                };
                let variation = u64::from((q * 19 + r * 31).unsigned_abs() % 37);
                let infantry = match owner {
                    Some(PLAYER_ONE) => 24 + variation,
                    Some(PLAYER_TWO) => 30 + variation,
                    _ => 0,
                }
                .min(capacity);
                let civilians = if owner.is_some() {
                    55 + u64::from((q * 11 + r * 17).unsigned_abs() % 80)
                } else {
                    0
                };
                let blocked = elevation >= 6 || (q == 1 && (-5..=-2).contains(&r));

                cells.insert(
                    coordinate,
                    CellView {
                        coordinate,
                        terrain,
                        elevation,
                        owner,
                        civilians,
                        infantry,
                        military_capacity: capacity,
                        blocked,
                    },
                );
            }
        }

        let cells_by_chunk = index_cells_by_chunk(&cells);
        let dirty_chunks = cells_by_chunk.keys().copied().collect();
        let mut order_log = VecDeque::new();
        order_log.push_front("Fixture ready · authority adapter is offline".to_owned());

        Self {
            cells,
            cells_by_chunk,
            chunk_index_revision: 1,
            cell_state_revision: 1,
            local_player: PLAYER_ONE,
            authority: AuthorityState::Offline,
            connection: [ConnectionState::Offline, ConnectionState::Offline],
            phase: MatchPhase::Running,
            conquest_threshold_bps: 8_000,
            authoritative_control: None,
            capturable_cells: 0,
            required_control: 0,
            logical_step: 0,
            mobilization_target: 0.55,
            active_orders: 0,
            queued_infantry: 0,
            active_flows: Vec::new(),
            active_fronts: vec![ActiveFront {
                friendly: Axial::new(-5, 4),
                hostile: Axial::new(-4, 4),
                intensity: 0.55,
                age: 0.0,
            }],
            latest_result: "Offline fixture loaded · commands resolve locally".to_owned(),
            order_log,
            toast: Some(Toast {
                text: "Offline fixture · Player 1".to_owned(),
                kind: ToastKind::Info,
                remaining: 4.0,
            }),
            dirty_chunks,
        }
    }

    pub fn cell(&self, coordinate: Axial) -> Option<&CellView> {
        self.cells.get(&coordinate)
    }

    pub fn cell_mut(&mut self, coordinate: Axial) -> Option<&mut CellView> {
        self.dirty_chunks.insert(chunk_of(coordinate));
        self.mark_cell_state_changed();
        self.cells.get_mut(&coordinate)
    }

    pub fn mark_cell_state_changed(&mut self) {
        self.cell_state_revision = self.cell_state_revision.wrapping_add(1);
    }

    /// Rebuilds the spatial index after a wholesale authoritative map update.
    /// Incremental cell-state changes do not need to touch this index because
    /// coordinates never move between render chunks.
    pub fn rebuild_chunk_index(&mut self) {
        self.cells_by_chunk = index_cells_by_chunk(&self.cells);
        self.chunk_index_revision = self.chunk_index_revision.wrapping_add(1);
        self.mark_cell_state_changed();
    }

    pub fn cells_in_chunk(&self, chunk: ChunkCoord) -> &[Axial] {
        self.cells_by_chunk.get(&chunk).map_or(&[], Vec::as_slice)
    }

    pub fn is_local_owned(&self, coordinate: Axial) -> bool {
        self.cell(coordinate)
            .is_some_and(|cell| cell.owner == Some(self.local_player))
    }

    pub fn conquest_percent(&self, player: u32) -> f32 {
        if let Some(controlled) = self.authoritative_control
            && self.capturable_cells > 0
            && matches!(player, 1 | 2)
        {
            return controlled[(player - 1) as usize] as f32 * 100.0 / self.capturable_cells as f32;
        }
        let capturable = self.cells.values().filter(|cell| cell.is_land()).count();
        if capturable == 0 {
            return 0.0;
        }
        let owned = self
            .cells
            .values()
            .filter(|cell| cell.is_land() && cell.owner == Some(player))
            .count();
        owned as f32 * 100.0 / capturable as f32
    }

    pub fn selected_totals(&self, cells: &BTreeSet<Axial>) -> (u64, u64, u64) {
        cells
            .iter()
            .filter_map(|coordinate| self.cell(*coordinate))
            .fold((0, 0, 0), |(infantry, capacity, civilians), cell| {
                (
                    infantry + cell.infantry,
                    capacity + cell.military_capacity,
                    civilians + cell.civilians,
                )
            })
    }

    pub fn push_log(&mut self, message: impl Into<String>) {
        self.latest_result = message.into();
        self.order_log.push_front(self.latest_result.clone());
        self.order_log.truncate(5);
    }

    pub fn show_toast(&mut self, text: impl Into<String>, kind: ToastKind) {
        self.toast = Some(Toast {
            text: text.into(),
            kind,
            remaining: 4.5,
        });
    }
}

fn index_cells_by_chunk(cells: &BTreeMap<Axial, CellView>) -> BTreeMap<ChunkCoord, Vec<Axial>> {
    let mut by_chunk = BTreeMap::<ChunkCoord, Vec<Axial>>::new();
    for coordinate in cells.keys().copied() {
        by_chunk
            .entry(chunk_of(coordinate))
            .or_default()
            .push(coordinate);
    }
    by_chunk
}

pub fn update_transient_state(time: Res<Time>, mut view: ResMut<MatchView>) {
    let delta = time.delta_secs();
    for flow in &mut view.active_flows {
        flow.age += delta;
    }
    view.active_flows.retain(|flow| flow.age < flow.lifetime);

    for front in &mut view.active_fronts {
        front.age += delta;
    }
    view.active_fronts.retain(|front| front.age < 18.0);

    if let Some(toast) = &mut view.toast {
        toast.remaining -= delta;
        if toast.remaining <= 0.0 {
            view.toast = None;
        }
    }
}

fn can_traverse(view: &MatchView, from: Axial, to: Axial) -> bool {
    let Some(from) = view.cell(from) else {
        return false;
    };
    let Some(to) = view.cell(to) else {
        return false;
    };
    from.is_land()
        && to.is_land()
        && !to.blocked
        && (i32::from(from.elevation) - i32::from(to.elevation)).unsigned_abs() <= 1
}

#[derive(Clone, Debug, Default)]
pub struct SourceReachability {
    previous: BTreeMap<Axial, Axial>,
    distance: BTreeMap<Axial, u32>,
}

impl SourceReachability {
    pub fn contains(&self, coordinate: Axial) -> bool {
        self.previous.contains_key(&coordinate)
    }

    pub fn route_to_any(&self, destinations: &BTreeSet<Axial>) -> Option<Vec<Axial>> {
        let destination = destinations
            .iter()
            .filter_map(|coordinate| {
                self.distance
                    .get(coordinate)
                    .map(|distance| (*distance, *coordinate))
            })
            .min()
            .map(|(_, coordinate)| coordinate)?;
        let mut current = destination;
        let mut route = vec![current];
        loop {
            let previous = self.previous[&current];
            if previous == current {
                break;
            }
            current = previous;
            route.push(current);
        }
        route.reverse();
        Some(route)
    }
}

#[derive(Clone, Debug, Default)]
pub struct DestinationReachability {
    next: BTreeMap<Axial, Axial>,
    distance: BTreeMap<Axial, u32>,
}

impl DestinationReachability {
    pub fn contains(&self, coordinate: Axial) -> bool {
        self.next.contains_key(&coordinate)
    }

    pub fn reachable_sources(&self, sources: &BTreeSet<Axial>) -> BTreeSet<Axial> {
        sources
            .iter()
            .filter(|coordinate| self.contains(**coordinate))
            .copied()
            .collect()
    }

    pub fn route_from_any(&self, sources: &BTreeSet<Axial>) -> Option<Vec<Axial>> {
        let source = sources
            .iter()
            .filter_map(|coordinate| {
                self.distance
                    .get(coordinate)
                    .map(|distance| (*distance, *coordinate))
            })
            .min()
            .map(|(_, coordinate)| coordinate)?;
        let mut current = source;
        let mut route = vec![current];
        loop {
            let next = self.next[&current];
            if next == current {
                break;
            }
            current = next;
            route.push(current);
        }
        Some(route)
    }
}

pub fn reachability_from_sources(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
) -> SourceReachability {
    let (previous, distance) = traverse(view, sources, false);
    SourceReachability { previous, distance }
}

pub fn reachability_to_destinations(
    view: &MatchView,
    destinations: &BTreeSet<Axial>,
) -> DestinationReachability {
    let (next, distance) = traverse(view, destinations, true);
    DestinationReachability { next, distance }
}

fn traverse(
    view: &MatchView,
    seeds: &BTreeSet<Axial>,
    reverse: bool,
) -> (BTreeMap<Axial, Axial>, BTreeMap<Axial, u32>) {
    let valid_seeds = seeds
        .iter()
        .copied()
        .filter(|coordinate| view.cell(*coordinate).is_some_and(CellView::is_land));
    let mut frontier = VecDeque::new();
    let mut links = BTreeMap::new();
    let mut distances = BTreeMap::<Axial, u32>::new();
    for seed in valid_seeds {
        if links.insert(seed, seed).is_none() {
            distances.insert(seed, 0);
            frontier.push_back(seed);
        }
    }

    while let Some(current) = frontier.pop_front() {
        let distance = distances[&current];
        for neighbor in current.neighbors() {
            let traversable = if reverse {
                can_traverse(view, neighbor, current)
            } else {
                can_traverse(view, current, neighbor)
            };
            if !traversable || links.contains_key(&neighbor) {
                continue;
            }
            links.insert(neighbor, current);
            distances.insert(neighbor, distance.saturating_add(1));
            frontier.push_back(neighbor);
        }
    }
    (links, distances)
}

#[allow(dead_code)]
pub fn find_route(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    destinations: &BTreeSet<Axial>,
) -> Option<Vec<Axial>> {
    if sources.is_empty() || destinations.is_empty() {
        return None;
    }
    reachability_from_sources(view, sources).route_to_any(destinations)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_cell(coordinate: Axial, infantry: u64) -> CellView {
        CellView {
            coordinate,
            terrain: TerrainKind::Plains,
            elevation: 0,
            owner: Some(PLAYER_ONE),
            civilians: 0,
            infantry,
            military_capacity: 100,
            blocked: false,
        }
    }

    fn disconnected_route_fixture() -> MatchView {
        let mut view = MatchView::connecting(1);
        for cell in [
            flat_cell(Axial::ZERO, 10),
            flat_cell(Axial::new(10, 0), 20),
            flat_cell(Axial::new(11, 0), 0),
        ] {
            view.cells.insert(cell.coordinate, cell);
        }
        view.rebuild_chunk_index();
        view
    }

    #[test]
    fn fixture_has_two_players_and_water() {
        let fixture = MatchView::offline_fixture();
        assert!(fixture.cells.values().any(CellView::is_water));
        assert!(
            fixture
                .cells
                .values()
                .any(|cell| cell.owner == Some(PLAYER_ONE))
        );
        assert!(
            fixture
                .cells
                .values()
                .any(|cell| cell.owner == Some(PLAYER_TWO))
        );
    }

    #[test]
    fn local_route_respects_cliffs_and_water() {
        let fixture = MatchView::offline_fixture();
        let sources = BTreeSet::from([Axial::new(-10, 1)]);
        let destinations = BTreeSet::from([Axial::new(-7, 1)]);
        let route = find_route(&fixture, &sources, &destinations).expect("fixture route");
        assert_eq!(route.first(), sources.first());
        assert_eq!(route.last(), destinations.first());
        assert!(
            route
                .iter()
                .all(|coordinate| fixture.cell(*coordinate).unwrap().is_land())
        );
    }

    #[test]
    fn route_search_seeds_every_source_and_reverse_search_filters_components() {
        let view = disconnected_route_fixture();
        let isolated = Axial::ZERO;
        let reachable = Axial::new(10, 0);
        let destination = Axial::new(11, 0);
        let sources = BTreeSet::from([isolated, reachable]);
        let destinations = BTreeSet::from([destination]);

        assert_eq!(
            find_route(&view, &sources, &destinations),
            Some(vec![reachable, destination])
        );
        let reverse = reachability_to_destinations(&view, &destinations);
        assert_eq!(
            reverse.reachable_sources(&sources),
            BTreeSet::from([reachable])
        );
        assert_eq!(
            reverse.route_from_any(&sources),
            Some(vec![reachable, destination])
        );
    }

    #[test]
    fn chunk_index_contains_every_cell_exactly_once() {
        let fixture = MatchView::offline_fixture();
        let indexed = fixture
            .cells_by_chunk
            .values()
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>();

        assert_eq!(indexed.len(), fixture.cells.len());
        assert_eq!(indexed, fixture.cells.keys().copied().collect());
        for (chunk, coordinates) in &fixture.cells_by_chunk {
            assert!(
                coordinates
                    .iter()
                    .all(|coordinate| chunk_of(*coordinate) == *chunk)
            );
        }
    }

    #[test]
    fn rebuilding_chunk_index_advances_topology_revision() {
        let mut fixture = MatchView::offline_fixture();
        let before = fixture.chunk_index_revision;
        let before_state = fixture.cell_state_revision;

        fixture.rebuild_chunk_index();

        assert_eq!(fixture.chunk_index_revision, before.wrapping_add(1));
        assert_eq!(fixture.cell_state_revision, before_state.wrapping_add(1));
    }

    #[test]
    fn mutable_cell_access_invalidates_cell_state_caches() {
        let mut fixture = MatchView::offline_fixture();
        let coordinate = fixture
            .cells
            .keys()
            .copied()
            .find(|coordinate| fixture.is_local_owned(*coordinate))
            .expect("owned fixture cell");
        let before = fixture.cell_state_revision;

        fixture
            .cell_mut(coordinate)
            .expect("fixture cell")
            .civilians += 1;

        assert_eq!(fixture.cell_state_revision, before.wrapping_add(1));
    }
}
