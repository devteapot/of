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

/// Persistent troop-distribution behavior attached to a complete owned
/// traversable cluster. The policy describes only troops that are currently
/// free; live action packets remain outside its target calculation while still
/// occupying physical capacity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClusterPolicy {
    Balanced,
    Center,
    Perimeter,
    Directional,
}

impl ClusterPolicy {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Balanced => "BALANCED",
            Self::Center => "CENTER",
            Self::Perimeter => "PERIMETER",
            Self::Directional => "DIRECTIONAL",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClusterPolicyView {
    pub kind: ClusterPolicy,
    /// Exact fixed-point axial facing for `Directional`; zero otherwise.
    pub orientation: Axial,
    /// Authority-owned revision used to resolve cluster merges.
    pub revision: u64,
}

impl ClusterPolicyView {
    pub const BALANCED_DEFAULT: Self = Self {
        kind: ClusterPolicy::Balanced,
        orientation: Axial::ZERO,
        revision: 0,
    };
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

/// Compact presentation state for a cell whose control is actively disputed.
///
/// Combat remains edge-based and authoritative ownership stays on [`CellView`].
/// This projection only gives the chunk renderer enough information to show
/// the relative forces without introducing per-cell render entities.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContestedCellView {
    pub controller_player: u32,
    pub attacker_player: u32,
    /// Surviving hostile strength represented by the edge-combat snapshot.
    pub attacker_strength: u64,
    /// Attacker share of the currently represented force, in the inclusive
    /// range `0.0..=1.0`. Rendering clamps network interpolation defensively.
    pub attacker_share: f32,
}

/// Client-side projection used to preview redistribution commands that retask
/// troops already committed to an active order. Enemy contested cells act as
/// handles; selecting one expands to every current packet cell of each order
/// represented at that combat front.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RetaskProjection {
    pub handle_orders: BTreeMap<Axial, BTreeSet<u64>>,
    pub active_order_ids: BTreeSet<u64>,
    /// Active internal redistribution orders issued by persistent cluster
    /// policy maintenance. These orders yield automatically to an explicit
    /// cluster action, but are not exposed as user-selected retask handles.
    pub background_policy_order_ids: BTreeSet<u64>,
    /// Persistent launch cells from authoritative `TransferSource` rows.
    /// Unlike packet locations these remain stable after an action advances,
    /// allowing a selected source cluster to stop work it originally issued.
    pub order_source_cells: BTreeMap<u64, BTreeSet<Axial>>,
    pub order_strength_by_cell: BTreeMap<u64, BTreeMap<Axial, u64>>,
    pub active_strength_by_cell: BTreeMap<Axial, u64>,
    /// Outstanding destination strength for active local Formation/Reshape
    /// orders, retained by order so an explicitly superseded order can be
    /// excluded from conservative overlap checks.
    pub destination_reservations_by_order: BTreeMap<u64, BTreeMap<Axial, u64>>,
    /// Every cell named as a destination by an active local order, retained by
    /// order so a replacement command can discard the claims it supersedes.
    /// Unlike capacity reservations, this includes Push/Expand destinations:
    /// a retreat must not preview relinquishing ground another live order is
    /// still using as an endpoint.
    pub destination_claims_by_order: BTreeMap<u64, BTreeSet<Axial>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProjectedOrderSelection {
    pub cells: BTreeSet<Axial>,
    /// Strength this command may affect before its participation percentage is
    /// applied. Unrelated active packet allocations are excluded.
    pub affected_strength_by_cell: BTreeMap<Axial, u64>,
    /// Strength that remains outside this command at each projected cell.
    pub unaffected_strength_by_cell: BTreeMap<Axial, u64>,
    /// Outstanding inbound destination reservations belonging to active local
    /// internal orders which this command does not supersede.
    pub unrelated_destination_reservations_by_cell: BTreeMap<Axial, u64>,
    /// Destination cells claimed by any active local order which this command
    /// does not supersede. This is ownership-safety metadata, not capacity
    /// reservation metadata.
    pub unrelated_destination_claims: BTreeSet<Axial>,
    pub superseded_order_count: usize,
    pub superseded_strength: u64,
    /// Background policy work the authority will preempt automatically. This
    /// is reported separately from explicit retask handles because these IDs
    /// must never be sent in a user supersede payload.
    pub released_policy_order_count: usize,
    pub released_policy_strength: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OrderSelectionProjectionError {
    InvalidSource(Axial),
    StaleOrder(u64),
    UnknownPacketCell(Axial),
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
    /// Advances only when ownership, infantry, capacity, terrain, or topology
    /// changes can affect command planning. Civilian-only and combat-overlay
    /// presentation changes intentionally leave this stable.
    pub planning_revision: u64,
    /// Advances when the transient combat overlay changes. This is separate
    /// from cell values so HUD totals and command previews stay cached.
    pub contest_revision: u64,
    pub local_player: u32,
    pub authority: AuthorityState,
    pub connection: [ConnectionState; 2],
    pub phase: MatchPhase,
    pub conquest_threshold_bps: u32,
    pub max_elevation_step: u16,
    /// Coordinates which authoritative terrain marks as non-capturable.
    ///
    /// Most local fixtures and focused tests model ordinary capturable ground,
    /// so the sparse negative set keeps those callers lightweight while online
    /// snapshots retain the full terrain distinction needed by command previews.
    pub non_capturable_cells: BTreeSet<Axial>,
    pub authoritative_control: Option<[u64; 2]>,
    pub capturable_cells: u64,
    pub required_control: u64,
    pub logical_step: u64,
    pub mobilization_target: f32,
    pub active_orders: usize,
    pub queued_infantry: u64,
    pub active_flows: Vec<ActiveFlow>,
    pub active_fronts: Vec<ActiveFront>,
    /// Only cells with an active hostile front are present. Callers replacing
    /// this projection must dirty the chunks containing removed and inserted
    /// coordinates so their vertex colors are refreshed.
    pub contested_cells: BTreeMap<Axial, ContestedCellView>,
    pub retask_projection: RetaskProjection,
    /// Per-cell projection of the persistent cluster policy. Storing the
    /// authority rows directly preserves split lineage and lets the client
    /// report a mixed selection without inventing stable cluster IDs.
    pub cluster_policies: BTreeMap<Axial, ClusterPolicyView>,
    /// Advances independently of cell state so order/packet-only changes
    /// invalidate redistribution previews without forcing terrain recoloring.
    pub retask_revision: u64,
    pub latest_result: String,
    pub order_log: VecDeque<String>,
    pub toast: Option<Toast>,
    pub dirty_chunks: BTreeSet<ChunkCoord>,
}

impl MatchView {
    pub fn cluster_policy_at(&self, coordinate: Axial) -> Option<ClusterPolicyView> {
        self.is_local_owned_passable(coordinate).then(|| {
            self.cluster_policies
                .get(&coordinate)
                .copied()
                .unwrap_or(ClusterPolicyView::BALANCED_DEFAULT)
        })
    }

    pub fn connecting(preferred_player: u8) -> Self {
        Self {
            cells: BTreeMap::new(),
            cells_by_chunk: BTreeMap::new(),
            chunk_index_revision: 0,
            cell_state_revision: 0,
            planning_revision: 0,
            contest_revision: 0,
            local_player: u32::from(preferred_player),
            authority: AuthorityState::Connecting,
            connection: [ConnectionState::Syncing, ConnectionState::Syncing],
            phase: MatchPhase::Lobby,
            conquest_threshold_bps: 8_000,
            max_elevation_step: 1,
            non_capturable_cells: BTreeSet::new(),
            authoritative_control: None,
            capturable_cells: 0,
            required_control: 0,
            logical_step: 0,
            mobilization_target: 0.25,
            active_orders: 0,
            queued_infantry: 0,
            active_flows: Vec::new(),
            active_fronts: Vec::new(),
            contested_cells: BTreeMap::new(),
            retask_projection: RetaskProjection::default(),
            cluster_policies: BTreeMap::new(),
            retask_revision: 0,
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
            planning_revision: 1,
            contest_revision: 0,
            local_player: PLAYER_ONE,
            authority: AuthorityState::Offline,
            connection: [ConnectionState::Offline, ConnectionState::Offline],
            phase: MatchPhase::Running,
            conquest_threshold_bps: 8_000,
            max_elevation_step: 1,
            non_capturable_cells: BTreeSet::new(),
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
            contested_cells: BTreeMap::new(),
            retask_projection: RetaskProjection::default(),
            cluster_policies: BTreeMap::new(),
            retask_revision: 0,
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
        self.mark_planning_changed();
        self.cells.get_mut(&coordinate)
    }

    pub fn mark_cell_state_changed(&mut self) {
        self.cell_state_revision = self.cell_state_revision.wrapping_add(1);
    }

    pub fn mark_planning_changed(&mut self) {
        self.planning_revision = self.planning_revision.wrapping_add(1);
    }

    /// Replaces the transient combat projection and schedules both newly
    /// contested and newly cleared cells for chunk recoloring.
    pub fn set_contested_cells(&mut self, contested_cells: BTreeMap<Axial, ContestedCellView>) {
        if self.contested_cells == contested_cells {
            return;
        }
        self.dirty_chunks.extend(
            self.contested_cells
                .keys()
                .chain(contested_cells.keys())
                .copied()
                .map(chunk_of),
        );
        self.contested_cells = contested_cells;
        self.contest_revision = self.contest_revision.wrapping_add(1);
    }

    /// Rebuilds the spatial index after a wholesale authoritative map update.
    /// Incremental cell-state changes do not need to touch this index because
    /// coordinates never move between render chunks.
    pub fn rebuild_chunk_index(&mut self) {
        self.cells_by_chunk = index_cells_by_chunk(&self.cells);
        self.chunk_index_revision = self.chunk_index_revision.wrapping_add(1);
        self.mark_cell_state_changed();
        self.mark_planning_changed();
    }

    pub fn cells_in_chunk(&self, chunk: ChunkCoord) -> &[Axial] {
        self.cells_by_chunk.get(&chunk).map_or(&[], Vec::as_slice)
    }

    pub fn is_local_owned(&self, coordinate: Axial) -> bool {
        self.cell(coordinate)
            .is_some_and(|cell| cell.owner == Some(self.local_player))
    }

    pub fn is_local_owned_passable(&self, coordinate: Axial) -> bool {
        self.cell(coordinate).is_some_and(|cell| {
            cell.owner == Some(self.local_player) && cell.is_land() && !cell.blocked
        })
    }

    /// Whether local troops can cross this exact owned edge under the current
    /// authoritative movement configuration.
    pub fn is_local_traversable_edge(&self, from: Axial, to: Axial) -> bool {
        if from.distance(to) != 1 {
            return false;
        }
        self.cell(from)
            .zip(self.cell(to))
            .is_some_and(|(from, to)| {
                from.owner == Some(self.local_player)
                    && to.owner == Some(self.local_player)
                    && from.is_land()
                    && to.is_land()
                    && !from.blocked
                    && !to.blocked
                    && (i32::from(from.elevation) - i32::from(to.elevation)).unsigned_abs()
                        <= u32::from(self.max_elevation_step)
            })
    }

    pub fn is_capturable(&self, coordinate: Axial) -> bool {
        self.cell(coordinate)
            .is_some_and(|cell| cell.is_land() && !cell.blocked)
            && !self.non_capturable_cells.contains(&coordinate)
    }

    pub fn is_local_retask_handle(&self, coordinate: Axial) -> bool {
        let Some(cell) = self.cell(coordinate) else {
            return false;
        };
        cell.is_land()
            && !cell.blocked
            && cell.owner.is_some_and(|owner| owner != self.local_player)
            && self
                .contested_cells
                .get(&coordinate)
                .is_some_and(|contest| {
                    contest.attacker_player == self.local_player && contest.attacker_strength > 0
                })
    }

    pub fn set_retask_projection(&mut self, projection: RetaskProjection) {
        if self.retask_projection == projection {
            return;
        }
        self.retask_projection = projection;
        self.retask_revision = self.retask_revision.wrapping_add(1);
    }

    pub fn project_order_selection(
        &self,
        sources: &BTreeSet<Axial>,
        supersede_order_ids: &BTreeSet<u64>,
    ) -> Result<ProjectedOrderSelection, OrderSelectionProjectionError> {
        self.project_order_selection_with_policy_release(
            sources,
            supersede_order_ids,
            &BTreeSet::new(),
        )
    }

    /// Projects an explicit cluster action using the same priority rule as
    /// authority: intersecting background policy orders yield automatically,
    /// while every other active allocation remains fixed.
    ///
    /// Cancelling a policy order releases all of its packets, but an explicit
    /// action may only draw from released strength whose current cell is in
    /// the selected cluster union. Remote survivors therefore do not expand
    /// the action's physical source set.
    pub fn project_cluster_action_selection(
        &self,
        sources: &BTreeSet<Axial>,
        supersede_order_ids: &BTreeSet<u64>,
    ) -> Result<ProjectedOrderSelection, OrderSelectionProjectionError> {
        // Legacy explicit retasks may contribute current packet cells beyond
        // the painted source set. Authority resolves that physical source
        // union before checking which background policy orders intersect it.
        let physical_sources = self
            .project_order_selection(sources, supersede_order_ids)?
            .cells;
        let released_policy_order_ids = self
            .intersecting_background_policy_orders(&physical_sources)
            .difference(supersede_order_ids)
            .copied()
            .collect();
        self.project_order_selection_with_policy_release(
            &physical_sources,
            supersede_order_ids,
            &released_policy_order_ids,
        )
    }

    fn intersecting_background_policy_orders(&self, sources: &BTreeSet<Axial>) -> BTreeSet<u64> {
        self.retask_projection
            .background_policy_order_ids
            .iter()
            .filter(|&&order_id| {
                self.retask_projection
                    .order_source_cells
                    .get(&order_id)
                    .is_some_and(|cells| !cells.is_disjoint(sources))
                    || self
                        .retask_projection
                        .order_strength_by_cell
                        .get(&order_id)
                        .is_some_and(|strength| {
                            strength
                                .keys()
                                .any(|coordinate| sources.contains(coordinate))
                        })
                    || self
                        .retask_projection
                        .destination_claims_by_order
                        .get(&order_id)
                        .is_some_and(|cells| !cells.is_disjoint(sources))
            })
            .copied()
            .collect()
    }

    fn project_order_selection_with_policy_release(
        &self,
        sources: &BTreeSet<Axial>,
        supersede_order_ids: &BTreeSet<u64>,
        released_policy_order_ids: &BTreeSet<u64>,
    ) -> Result<ProjectedOrderSelection, OrderSelectionProjectionError> {
        if let Some(&invalid) = sources
            .iter()
            .find(|coordinate| !self.is_local_owned_passable(**coordinate))
        {
            return Err(OrderSelectionProjectionError::InvalidSource(invalid));
        }

        let mut superseded_by_cell = BTreeMap::<Axial, u64>::new();
        for &order_id in supersede_order_ids {
            if !self.retask_projection.active_order_ids.contains(&order_id) {
                return Err(OrderSelectionProjectionError::StaleOrder(order_id));
            }
            let Some(strength_by_cell) =
                self.retask_projection.order_strength_by_cell.get(&order_id)
            else {
                return Err(OrderSelectionProjectionError::StaleOrder(order_id));
            };
            for (&coordinate, &strength) in strength_by_cell {
                let pooled = superseded_by_cell.entry(coordinate).or_default();
                *pooled = pooled.saturating_add(strength);
            }
        }

        let mut released_policy_by_cell = BTreeMap::<Axial, u64>::new();
        for &order_id in released_policy_order_ids {
            let Some(strength_by_cell) =
                self.retask_projection.order_strength_by_cell.get(&order_id)
            else {
                continue;
            };
            for (&coordinate, &strength) in strength_by_cell {
                if sources.contains(&coordinate) {
                    let pooled = released_policy_by_cell.entry(coordinate).or_default();
                    *pooled = pooled.saturating_add(strength);
                }
            }
        }

        let mut projected = ProjectedOrderSelection {
            cells: sources
                .iter()
                .copied()
                .chain(superseded_by_cell.keys().copied())
                .collect(),
            superseded_order_count: supersede_order_ids.len(),
            superseded_strength: superseded_by_cell.values().copied().sum(),
            released_policy_order_count: released_policy_order_ids.len(),
            released_policy_strength: released_policy_by_cell.values().copied().sum(),
            ..Default::default()
        };
        for (&order_id, reservations) in &self.retask_projection.destination_reservations_by_order {
            if supersede_order_ids.contains(&order_id)
                || released_policy_order_ids.contains(&order_id)
            {
                continue;
            }
            for (&coordinate, &strength) in reservations {
                if strength > 0 {
                    let reserved = projected
                        .unrelated_destination_reservations_by_cell
                        .entry(coordinate)
                        .or_default();
                    *reserved = reserved.saturating_add(strength);
                }
            }
        }
        for (&order_id, claims) in &self.retask_projection.destination_claims_by_order {
            if !supersede_order_ids.contains(&order_id)
                && !released_policy_order_ids.contains(&order_id)
            {
                projected
                    .unrelated_destination_claims
                    .extend(claims.iter().copied());
            }
        }
        for &coordinate in &projected.cells {
            let Some(cell) = self.cell(coordinate) else {
                return Err(OrderSelectionProjectionError::UnknownPacketCell(coordinate));
            };
            let active = self
                .retask_projection
                .active_strength_by_cell
                .get(&coordinate)
                .copied()
                .unwrap_or(0)
                .min(cell.infantry);
            let superseded = superseded_by_cell
                .get(&coordinate)
                .copied()
                .unwrap_or(0)
                .min(active);
            let released_policy = released_policy_by_cell
                .get(&coordinate)
                .copied()
                .unwrap_or(0)
                .min(active.saturating_sub(superseded));
            // Once an order is explicitly superseded, its current packet cells
            // become physical sources. All otherwise-unallocated infantry at
            // those cells is available too; only unrelated active allocations
            // remain outside the replacement command.
            let unallocated = cell.infantry.saturating_sub(active);
            let affected = unallocated
                .saturating_add(superseded)
                .saturating_add(released_policy)
                .min(cell.infantry);
            projected
                .affected_strength_by_cell
                .insert(coordinate, affected);
            projected
                .unaffected_strength_by_cell
                .insert(coordinate, cell.infantry.saturating_sub(affected));
        }
        Ok(projected)
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
        && (i32::from(from.elevation) - i32::from(to.elevation)).unsigned_abs()
            <= u32::from(view.max_elevation_step)
}

#[derive(Clone, Debug, Default)]
pub struct SourceReachability {
    previous: BTreeMap<Axial, Axial>,
    distance: BTreeMap<Axial, u32>,
}

impl SourceReachability {
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

pub fn reachability_from_sources(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
) -> SourceReachability {
    let (previous, distance) = traverse(view, sources, false);
    SourceReachability { previous, distance }
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
    fn owned_clusters_default_to_balanced_until_an_authoritative_policy_arrives() {
        let coordinate = Axial::ZERO;
        let mut view = MatchView::connecting(1);
        view.cells.insert(coordinate, flat_cell(coordinate, 10));
        assert_eq!(
            view.cluster_policy_at(coordinate),
            Some(ClusterPolicyView::BALANCED_DEFAULT)
        );

        view.cluster_policies.insert(
            coordinate,
            ClusterPolicyView {
                kind: ClusterPolicy::Directional,
                orientation: Axial::new(2, -1),
                revision: 4,
            },
        );
        assert_eq!(
            view.cluster_policy_at(coordinate),
            Some(ClusterPolicyView {
                kind: ClusterPolicy::Directional,
                orientation: Axial::new(2, -1),
                revision: 4,
            })
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
    fn local_traversable_edges_require_adjacent_owned_passable_cells_and_allowed_slope() {
        let origin = Axial::ZERO;
        let neighbor = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(origin, flat_cell(origin, 10));
        view.cells.insert(neighbor, flat_cell(neighbor, 0));
        view.max_elevation_step = 1;

        assert!(view.is_local_traversable_edge(origin, neighbor));
        assert!(!view.is_local_traversable_edge(origin, Axial::new(2, 0)));

        view.cell_mut(neighbor).expect("neighbor").elevation = 2;
        assert!(!view.is_local_traversable_edge(origin, neighbor));
        view.cell_mut(neighbor).expect("neighbor").elevation = 1;
        assert!(view.is_local_traversable_edge(origin, neighbor));

        view.cell_mut(neighbor).expect("neighbor").blocked = true;
        assert!(!view.is_local_traversable_edge(origin, neighbor));
        view.cell_mut(neighbor).expect("neighbor").blocked = false;
        view.cell_mut(neighbor).expect("neighbor").terrain = TerrainKind::Water;
        assert!(!view.is_local_traversable_edge(origin, neighbor));
        view.cell_mut(neighbor).expect("neighbor").terrain = TerrainKind::Plains;
        view.cell_mut(neighbor).expect("neighbor").owner = Some(PLAYER_TWO);
        assert!(!view.is_local_traversable_edge(origin, neighbor));
    }

    #[test]
    fn route_search_seeds_every_source_and_selects_the_reachable_component() {
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

    #[test]
    fn replacing_contests_dirties_both_cleared_and_added_chunks() {
        let mut view = MatchView::connecting(1);
        let planning_revision = view.planning_revision;
        let cell_state_revision = view.cell_state_revision;
        let cleared = Axial::new(-40, 0);
        let added = Axial::new(40, 0);
        view.contested_cells.insert(
            cleared,
            ContestedCellView {
                controller_player: PLAYER_ONE,
                attacker_player: PLAYER_TWO,
                attacker_strength: 25,
                attacker_share: 0.25,
            },
        );

        view.set_contested_cells(BTreeMap::from([(
            added,
            ContestedCellView {
                controller_player: PLAYER_TWO,
                attacker_player: PLAYER_ONE,
                attacker_strength: 75,
                attacker_share: 0.75,
            },
        )]));

        assert!(view.dirty_chunks.contains(&chunk_of(cleared)));
        assert!(view.dirty_chunks.contains(&chunk_of(added)));
        assert!(!view.contested_cells.contains_key(&cleared));
        assert!(view.contested_cells.contains_key(&added));
        assert_eq!(view.planning_revision, planning_revision);
        assert_eq!(view.cell_state_revision, cell_state_revision);
    }

    #[test]
    fn retask_projection_releases_selected_orders_but_preserves_unrelated_allocations() {
        let first = Axial::ZERO;
        let second = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(first, flat_cell(first, 100));
        view.cells.insert(second, flat_cell(second, 50));
        view.set_retask_projection(RetaskProjection {
            active_order_ids: BTreeSet::from([7, 9]),
            order_strength_by_cell: BTreeMap::from([
                (7, BTreeMap::from([(first, 40), (second, 10)])),
                (9, BTreeMap::from([(first, 20), (second, 20)])),
            ]),
            active_strength_by_cell: BTreeMap::from([(first, 60), (second, 30)]),
            destination_reservations_by_order: BTreeMap::from([
                (7, BTreeMap::from([(first, 12)])),
                (9, BTreeMap::from([(second, 15)])),
            ]),
            destination_claims_by_order: BTreeMap::from([
                (7, BTreeSet::from([first])),
                (9, BTreeSet::from([second])),
            ]),
            ..Default::default()
        });

        let projected = view
            .project_order_selection(&BTreeSet::new(), &BTreeSet::from([7]))
            .expect("active order should project to every packet cell");

        assert_eq!(projected.cells, BTreeSet::from([first, second]));
        assert_eq!(projected.superseded_order_count, 1);
        assert_eq!(projected.superseded_strength, 50);
        assert_eq!(
            projected.affected_strength_by_cell,
            BTreeMap::from([(first, 80), (second, 30)])
        );
        assert_eq!(
            projected.unaffected_strength_by_cell,
            BTreeMap::from([(first, 20), (second, 20)])
        );
        assert_eq!(
            projected.unrelated_destination_reservations_by_cell,
            BTreeMap::from([(second, 15)])
        );
        assert_eq!(
            projected.unrelated_destination_claims,
            BTreeSet::from([second])
        );
    }

    #[test]
    fn cluster_action_releases_only_local_strength_from_intersecting_background_policy() {
        let selected = Axial::ZERO;
        let remote = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(selected, flat_cell(selected, 100));
        view.cells.insert(remote, flat_cell(remote, 50));
        view.set_retask_projection(RetaskProjection {
            active_order_ids: BTreeSet::from([7, 9]),
            background_policy_order_ids: BTreeSet::from([7]),
            order_source_cells: BTreeMap::from([
                (7, BTreeSet::from([selected])),
                (9, BTreeSet::from([selected])),
            ]),
            order_strength_by_cell: BTreeMap::from([
                (7, BTreeMap::from([(selected, 40), (remote, 10)])),
                (9, BTreeMap::from([(selected, 20), (remote, 20)])),
            ]),
            active_strength_by_cell: BTreeMap::from([(selected, 60), (remote, 30)]),
            destination_reservations_by_order: BTreeMap::from([
                (7, BTreeMap::from([(remote, 10)])),
                (9, BTreeMap::from([(selected, 15)])),
            ]),
            destination_claims_by_order: BTreeMap::from([
                (7, BTreeSet::from([remote])),
                (9, BTreeSet::from([selected])),
            ]),
            ..Default::default()
        });

        let projected = view
            .project_cluster_action_selection(&BTreeSet::from([selected]), &BTreeSet::new())
            .expect("intersecting policy work should yield to the cluster action");

        assert_eq!(projected.cells, BTreeSet::from([selected]));
        assert_eq!(projected.released_policy_order_count, 1);
        assert_eq!(projected.released_policy_strength, 40);
        assert_eq!(projected.superseded_order_count, 0);
        assert_eq!(
            projected.affected_strength_by_cell,
            BTreeMap::from([(selected, 80)])
        );
        assert_eq!(
            projected.unaffected_strength_by_cell,
            BTreeMap::from([(selected, 20)])
        );
        assert_eq!(
            projected.unrelated_destination_reservations_by_cell,
            BTreeMap::from([(selected, 15)])
        );
        assert_eq!(
            projected.unrelated_destination_claims,
            BTreeSet::from([selected])
        );
    }

    #[test]
    fn cluster_action_keeps_nonintersecting_policy_and_explicit_actions_fixed() {
        let selected = Axial::ZERO;
        let remote = Axial::new(2, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(selected, flat_cell(selected, 100));
        view.cells.insert(remote, flat_cell(remote, 50));
        view.set_retask_projection(RetaskProjection {
            active_order_ids: BTreeSet::from([7, 9]),
            background_policy_order_ids: BTreeSet::from([7]),
            order_source_cells: BTreeMap::from([
                (7, BTreeSet::from([remote])),
                (9, BTreeSet::from([selected])),
            ]),
            order_strength_by_cell: BTreeMap::from([
                (7, BTreeMap::from([(remote, 10)])),
                (9, BTreeMap::from([(selected, 30)])),
            ]),
            active_strength_by_cell: BTreeMap::from([(selected, 30), (remote, 10)]),
            destination_claims_by_order: BTreeMap::from([
                (7, BTreeSet::from([remote])),
                (9, BTreeSet::from([selected])),
            ]),
            ..Default::default()
        });

        let projected = view
            .project_cluster_action_selection(&BTreeSet::from([selected]), &BTreeSet::new())
            .expect("unrelated allocations remain fixed");

        assert_eq!(projected.released_policy_order_count, 0);
        assert_eq!(
            projected.affected_strength_by_cell,
            BTreeMap::from([(selected, 70)])
        );
        assert_eq!(
            projected.unaffected_strength_by_cell,
            BTreeMap::from([(selected, 30)])
        );
    }

    #[test]
    fn policy_intersection_uses_the_physical_union_from_an_explicit_retask() {
        let selected = Axial::ZERO;
        let packet_cell = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(selected, flat_cell(selected, 100));
        view.cells.insert(packet_cell, flat_cell(packet_cell, 50));
        view.set_retask_projection(RetaskProjection {
            active_order_ids: BTreeSet::from([7, 9]),
            background_policy_order_ids: BTreeSet::from([7]),
            order_source_cells: BTreeMap::from([(7, BTreeSet::from([packet_cell]))]),
            order_strength_by_cell: BTreeMap::from([
                (7, BTreeMap::from([(packet_cell, 20)])),
                (9, BTreeMap::from([(packet_cell, 10)])),
            ]),
            active_strength_by_cell: BTreeMap::from([(packet_cell, 30)]),
            ..Default::default()
        });

        let projected = view
            .project_cluster_action_selection(&BTreeSet::from([selected]), &BTreeSet::from([9]))
            .expect("the explicit packet cell should participate in policy intersection");

        assert_eq!(projected.cells, BTreeSet::from([selected, packet_cell]));
        assert_eq!(projected.released_policy_order_count, 1);
        assert_eq!(projected.released_policy_strength, 20);
        assert_eq!(projected.superseded_strength, 10);
        assert_eq!(projected.affected_strength_by_cell[&packet_cell], 50);
    }

    #[test]
    fn enemy_local_pressure_is_a_handle_but_local_contest_is_an_owned_source() {
        let enemy = Axial::ZERO;
        let local = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(
            enemy,
            CellView {
                owner: Some(PLAYER_TWO),
                ..flat_cell(enemy, 20)
            },
        );
        view.cells.insert(local, flat_cell(local, 30));
        view.contested_cells = BTreeMap::from([
            (
                enemy,
                ContestedCellView {
                    controller_player: PLAYER_TWO,
                    attacker_player: PLAYER_ONE,
                    attacker_strength: 15,
                    attacker_share: 15.0 / 35.0,
                },
            ),
            (
                local,
                ContestedCellView {
                    controller_player: PLAYER_ONE,
                    attacker_player: PLAYER_TWO,
                    attacker_strength: 10,
                    attacker_share: 0.25,
                },
            ),
        ]);

        assert!(view.is_local_retask_handle(enemy));
        assert!(!view.is_local_owned_passable(enemy));
        assert!(view.is_local_owned_passable(local));
        assert!(!view.is_local_retask_handle(local));
    }
}
