use spacetimedb::{AnonymousViewContext, Identity, Query, ScheduleAt, SpacetimeType};

use crate::simulation_tick;

pub const SINGLETON_ID: u8 = 0;
pub const PLAYER_ONE: u8 = 1;
pub const PLAYER_TWO: u8 = 2;
pub const NEUTRAL_PLAYER: u8 = 0;
/// Sentinel used after an Expand All contribution has left its real source.
/// Map cell identifiers are always below this value.
pub const EXPANSION_AGGREGATE_ORIGIN: u32 = u32::MAX;

#[derive(SpacetimeType, Clone, Copy, Debug, Eq, PartialEq)]
pub enum MapPreset {
    Dev64,
    Playtest128,
    Validation192,
}

impl MapPreset {
    pub const fn side(self) -> u16 {
        match self {
            Self::Dev64 => 64,
            Self::Playtest128 => 128,
            Self::Validation192 => 192,
        }
    }

    pub const fn seed(self) -> u64 {
        match self {
            Self::Dev64 => 0x0000_0fd3_6401,
            Self::Playtest128 => 0x0000_fa11_2802,
            Self::Validation192 => 0x0000_fa11_9203,
        }
    }
}

#[derive(SpacetimeType, Clone, Copy, Debug, Eq, PartialEq)]
pub enum MatchPhase {
    Lobby,
    Running,
    Completed,
}

#[derive(SpacetimeType, Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerrainClass {
    Water,
    Plains,
    Hills,
    Mountain,
}

#[derive(SpacetimeType, Clone, Copy, Debug, Eq, PartialEq)]
pub enum OrderKind {
    Balance,
    FrontLoad,
    CoreLoad,
    PerimeterLoad,
    Reshape,
    PushFront,
    ExpandAll,
    ExpandClusters,
    AttackClusters,
}

#[derive(SpacetimeType, Clone, Copy, Debug, Eq, PartialEq)]
pub enum OrderStatus {
    Active,
    Completed,
    Cancelled,
}

#[derive(SpacetimeType, Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiptStatus {
    Accepted,
    Rejected,
}

/// Persistent distribution behavior for one owned traversable cluster.
///
/// Assignments are stored per cell so splits inherit their previous behavior
/// without manufacturing a new cluster identity. When components merge, the
/// assignment with the newest explicit revision wins for the whole component.
#[derive(SpacetimeType, Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClusterPolicyKind {
    Balanced,
    Center,
    Perimeter,
    Directional,
}

#[derive(Clone)]
#[spacetimedb::table(accessor = player_slot, public)]
pub struct PlayerSlot {
    #[primary_key]
    pub player_id: u8,
    pub identity: Option<Identity>,
    pub display_name: String,
    pub connected: bool,
    pub has_reconnected: bool,
    pub reconnect_count: u32,
    pub ready: bool,
    pub joined_at_us: u64,
    pub last_seen_at_us: u64,
}

#[derive(Clone)]
#[spacetimedb::table(accessor = match_config, public)]
pub struct MatchConfig {
    #[primary_key]
    pub singleton_id: u8,
    pub map_preset: MapPreset,
    pub map_seed: u64,
    pub map_width: u16,
    pub map_height: u16,
    pub map_q_min: i32,
    pub map_r_min: i32,
    pub chunk_size: u16,
    pub logical_step_ms: u32,
    pub population_step_interval: u32,
    pub base_military_capacity: u64,
    pub base_edge_throughput_per_second: u64,
    pub base_combat_frontage: u64,
    pub max_elevation_step: u8,
    pub uphill_attack_bps: u32,
    pub combat_lethality_bps: u32,
    pub civilian_growth_bps: u32,
    pub mobilization_per_population_step: u64,
    pub conquest_threshold_bps: u32,
    pub map_hash: u64,
    pub spawn_one_cell: u32,
    pub spawn_two_cell: u32,
}

#[derive(Clone)]
#[spacetimedb::table(accessor = match_state, public)]
pub struct MatchState {
    #[primary_key]
    pub singleton_id: u8,
    pub phase: MatchPhase,
    pub logical_step: u64,
    pub capturable_cells: u64,
    pub required_control: u64,
    pub player_one_controlled: u64,
    pub player_two_controlled: u64,
    pub winner_player_id: u8,
    /// Monotonic authority-owned revision for explicit cluster-policy changes.
    pub latest_cluster_policy_revision: u64,
    /// Monotonic durable ownership/topology revision. Every capture or
    /// relinquishment advances it transactionally.
    pub ownership_revision: u64,
    /// Ownership revision represented by the durable component topology rows.
    /// A mismatch forces one cold-start-safe global relabeling pass.
    pub policy_topology_revision: u64,
    /// Stable key of the last component considered by periodic policy
    /// maintenance. The next pass resumes after it and wraps deterministically.
    pub policy_replan_cursor: u64,
    pub started_at_us: u64,
    pub completed_at_us: u64,
}

#[derive(Clone)]
#[spacetimedb::table(accessor = mobilization_policy, public)]
pub struct MobilizationPolicy {
    #[primary_key]
    pub player_id: u8,
    pub target_bps: u32,
}

#[derive(Clone)]
#[spacetimedb::table(
    accessor = cell_terrain,
    public,
    index(accessor = terrain_by_chunk, btree(columns = [chunk_q, chunk_r]))
)]
pub struct CellTerrain {
    #[primary_key]
    #[index(direct)]
    pub cell_id: u32,
    pub q: i32,
    pub r: i32,
    pub chunk_q: i16,
    pub chunk_r: i16,
    pub terrain: TerrainClass,
    pub elevation: i16,
    pub passable: bool,
    pub capturable: bool,
    pub habitable: bool,
}

#[derive(Clone)]
#[spacetimedb::table(
    accessor = cell_state,
    public,
    index(accessor = state_by_owner, btree(columns = [owner_player_id]))
)]
pub struct CellState {
    #[primary_key]
    #[index(direct)]
    pub cell_id: u32,
    pub owner_player_id: u8,
    pub civilians: u64,
    pub civilian_capacity: u64,
    pub infantry: u64,
    pub military_capacity: u64,
    pub last_changed_step: u64,
    /// Last step at which ownership, infantry, or capacity changed. Civilian-
    /// only growth does not dirty military distribution plans.
    pub last_policy_changed_step: u64,
}

/// Small durable invalidation record for one derived owned component.
#[derive(Clone)]
#[spacetimedb::table(accessor = policy_replan_state)]
pub struct PolicyReplanState {
    /// `(owner_player_id << 32) | minimum_cell_id`.
    #[primary_key]
    pub component_key: u64,
    pub shape_hash: u64,
    pub policy_revision: u64,
    pub last_plan_step: u64,
}

/// Durable boundary-weight cache. Large vectors are rewritten only when the
/// component topology changes, never on ordinary recruitment or movement.
#[derive(Clone)]
#[spacetimedb::table(accessor = policy_topology_cache)]
pub struct PolicyTopologyCache {
    #[primary_key]
    pub component_key: u64,
    pub owner_player_id: u8,
    pub ownership_revision: u64,
    pub shape_hash: u64,
    pub cell_ids: Vec<u32>,
    /// Static cell data aligned with `cell_ids`. Keeping it beside the durable
    /// topology turns periodic planning into one owner-indexed state scan
    /// instead of two direct table probes per component cell.
    pub q: Vec<i32>,
    pub r: Vec<i32>,
    pub terrain: Vec<TerrainClass>,
    pub elevation: Vec<i16>,
    pub capturable: Vec<bool>,
    pub habitable: Vec<bool>,
    pub civilian_capacity: Vec<u64>,
    pub military_capacity: Vec<u64>,
    /// CoreLoad weights aligned with `cell_ids`; PerimeterLoad uses the exact
    /// complementary weight around the shared 20,000 midpoint sum.
    pub core_weights: Vec<u32>,
}

/// Static, direction-aware movement limits for one undirected neighboring
/// cell pair. Terrain, elevation, capacity, and match configuration cannot
/// change after a match starts, so hot simulation phases should not rebuild
/// these values through repeated terrain/state probes.
#[derive(Clone)]
#[spacetimedb::table(accessor = static_edge_limit)]
pub struct StaticEdgeLimit {
    /// `(min(cell_a, cell_b) << 32) | max(cell_a, cell_b)`.
    #[primary_key]
    pub edge_key: u64,
    pub first_cell: u32,
    pub second_cell: u32,
    pub traversable: bool,
    pub first_to_second_throughput: u64,
    pub second_to_first_throughput: u64,
    pub first_to_second_frontage: u64,
    pub second_to_first_frontage: u64,
    pub first_to_second_uphill: bool,
    pub second_to_first_uphill: bool,
}

/// Policy lineage attached to one owned cell.
///
/// This table deliberately does not identify a cluster directly: connected
/// components are derived from current ownership and terrain. Per-cell lineage
/// makes a split free, while `revision` gives merges a deterministic winner.
#[derive(Clone)]
#[spacetimedb::table(
    accessor = cluster_policy_assignment,
    public,
    index(accessor = policy_by_owner, btree(columns = [owner_player_id]))
)]
pub struct ClusterPolicyAssignment {
    #[primary_key]
    pub cell_id: u32,
    pub owner_player_id: u8,
    pub kind: ClusterPolicyKind,
    /// Exact fixed-point axial facing used only by `Directional`.
    pub orientation_q: i32,
    pub orientation_r: i32,
    pub revision: u64,
}

#[derive(Clone)]
#[spacetimedb::table(
    accessor = command_receipt,
    public,
    index(accessor = receipt_by_player, btree(columns = [player_id]))
)]
pub struct CommandReceipt {
    #[primary_key]
    pub receipt_key: u128,
    pub player_id: u8,
    pub client_command_id: u64,
    pub command_name: String,
    pub status: ReceiptStatus,
    pub order_id: u64,
    pub message: String,
    pub logical_step: u64,
}

#[derive(Clone)]
#[spacetimedb::table(
    accessor = transfer_order,
    public,
    index(accessor = order_by_player, btree(columns = [player_id])),
    index(accessor = order_by_status, btree(columns = [status]))
)]
pub struct TransferOrder {
    #[primary_key]
    #[auto_inc]
    pub order_id: u64,
    pub player_id: u8,
    pub client_command_id: u64,
    pub kind: OrderKind,
    pub status: OrderStatus,
    pub requested_infantry: u64,
    pub committed_infantry: u64,
    pub in_transit_infantry: u64,
    /// Surviving strength released from this order, including occupation
    /// garrisons, endpoints, and release-in-place when cancelled.
    pub delivered_infantry: u64,
    pub casualty_infantry: u64,
    pub orientation_q: i32,
    pub orientation_r: i32,
    pub created_step: u64,
    pub updated_step: u64,
}

#[derive(Clone)]
#[spacetimedb::table(
    accessor = transfer_source,
    public,
    index(accessor = source_by_order, btree(columns = [order_id]))
)]
pub struct TransferSource {
    #[primary_key]
    pub source_key: u128,
    pub order_id: u64,
    pub cell_id: u32,
    pub committed_infantry: u64,
    pub queued_infantry: u64,
}

/// A selected trailing-edge cell which may be relinquished after a friendly
/// Push has completed its one-hex retreat. This is private command topology:
/// clients derive the same forecast from selection geometry, while authority
/// rechecks the physical cell before changing ownership.
#[derive(Clone)]
#[spacetimedb::table(
    accessor = retreat_abandonment,
    index(accessor = abandonment_by_order, btree(columns = [order_id]))
)]
pub struct RetreatAbandonment {
    #[primary_key]
    pub abandonment_key: u128,
    pub order_id: u64,
    pub cell_id: u32,
}

#[derive(Clone)]
#[spacetimedb::table(
    accessor = transfer_destination,
    public,
    index(accessor = destination_by_order, btree(columns = [order_id]))
)]
pub struct TransferDestination {
    #[primary_key]
    pub destination_key: u128,
    pub order_id: u64,
    /// Destination for redistribution orders; stable first-front lane anchor
    /// for a sustained Push Front operation.
    pub cell_id: u32,
    pub target_infantry: u64,
    pub received_infantry: u64,
}

/// One immutable planned leg shared by every packet fragment traveling it.
/// Sustained Push may extend the row, but ordinary packet movement updates
/// only the compact packet metadata and never reserializes this vector.
#[derive(Clone)]
#[spacetimedb::table(
    accessor = transit_route,
    public,
    index(accessor = route_by_order, btree(columns = [order_id]))
)]
pub struct TransitRoute {
    #[primary_key]
    #[auto_inc]
    pub route_id: u64,
    pub order_id: u64,
    pub cells: Vec<u32>,
}

#[derive(Clone)]
#[spacetimedb::table(
    accessor = transit_packet,
    public,
    index(accessor = packet_by_order, btree(columns = [order_id])),
    index(
        accessor = packet_by_order_destination,
        btree(columns = [order_id, destination_cell])
    ),
    index(accessor = packet_by_cell, btree(columns = [current_cell]))
)]
pub struct TransitPacket {
    #[primary_key]
    #[auto_inc]
    pub packet_key: u64,
    pub order_id: u64,
    pub owner_player_id: u8,
    pub origin_cell: u32,
    pub current_cell: u32,
    pub destination_cell: u32,
    pub infantry: u64,
    /// Portion of `infantry` not yet debited from the order's source queue.
    /// Expansion packets may aggregate several origins and consume the order
    /// pool deterministically instead of retaining one row per origin.
    pub pending_source_infantry: u64,
    /// Zero identifies an expansion edge/rest packet whose one-edge route is
    /// represented by `current_cell` and `destination_cell` directly.
    pub route_id: u64,
    pub route_index: u32,
    pub updated_step: u64,
}

/// Tactical packet stream for regular clients. Background policy packets are
/// authoritative simulation detail, but are not rendered and previously made
/// up most replicated packet traffic during large rebalances.
#[spacetimedb::view(accessor = visible_packets, public, primary_key = packet_key)]
pub fn visible_packets(ctx: &AnonymousViewContext) -> impl Query<TransitPacket> {
    ctx.from
        .transfer_order()
        .r#where(|order| order.client_command_id.ne(0_u64))
        .right_semijoin(ctx.from.transit_packet(), |order, packet| {
            order.order_id.eq(packet.packet_by_order)
        })
}

/// Route stream matching `visible_packets`; background policy routes stay
/// authoritative but are not replicated to regular rendering clients.
#[spacetimedb::view(accessor = visible_routes, public, primary_key = route_id)]
pub fn visible_routes(ctx: &AnonymousViewContext) -> impl Query<TransitRoute> {
    ctx.from
        .transfer_order()
        .r#where(|order| order.client_command_id.ne(0_u64))
        .right_semijoin(ctx.from.transit_route(), |order, route| {
            order.order_id.eq(route.route_by_order)
        })
}

/// Compact private topology for one branching neutral or cluster-attack wave.
///
/// `selected_cells` and `seed_depths` are parallel sorted vectors. A seed
/// depth of zero marks the selected perimeter; larger values flow toward zero.
/// `outside_depths[cell_id]` is the static multi-source BFS distance from the
/// first destination ring, starting at one, or `u16::MAX` when unreachable.
#[derive(Clone)]
#[spacetimedb::table(accessor = expansion_wave)]
pub struct ExpansionWave {
    #[primary_key]
    pub order_id: u64,
    pub selected_cells: Vec<u32>,
    pub seed_depths: Vec<u16>,
    pub outside_depths: Vec<u16>,
    /// Per-cell rotating remainder cursor for unbiased asynchronous splits.
    pub split_cursors: Vec<u8>,
    /// Optional neutral click objective. Branch weights are 3/2/1 according
    /// to whether a child moves closer/equally/farther from this cell.
    pub focus_cell_id: Option<u32>,
    /// Immutable sorted enemy footprint for `AttackClusters`. Empty for both
    /// neutral expansion variants. Attack branches may never leave this mask.
    pub target_cells: Vec<u32>,
}

/// Sparse unpaid occupation garrison created by an Expand All capture.
///
/// The debt belongs to the captured cell rather than to one order so a later
/// overlapping expansion can finish paying it after the capturing order ends.
#[derive(Clone)]
#[spacetimedb::table(accessor = expansion_garrison_debt)]
pub struct ExpansionGarrisonDebt {
    #[primary_key]
    pub cell_id: u32,
    pub owner_player_id: u8,
    pub remaining_infantry: u64,
}

#[derive(Clone)]
#[spacetimedb::table(
    accessor = combat_front,
    public,
    index(accessor = front_by_target, btree(columns = [to_cell]))
)]
pub struct CombatFront {
    #[primary_key]
    pub front_key: String,
    pub attacker_player_id: u8,
    pub defender_player_id: u8,
    pub from_cell: u32,
    pub to_cell: u32,
    pub queued_infantry: u64,
    pub attacker_engaged: u64,
    pub defender_engaged: u64,
    pub attacker_casualties: u64,
    pub defender_casualties: u64,
    pub frontage: u64,
    pub uphill: bool,
    pub logical_step: u64,
}

#[spacetimedb::table(accessor = simulation_schedule, scheduled(simulation_tick))]
pub struct SimulationSchedule {
    #[primary_key]
    #[auto_inc]
    pub scheduled_id: u64,
    pub scheduled_at: ScheduleAt,
}
