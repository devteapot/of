use spacetimedb::{Identity, ScheduleAt, SpacetimeType};

use crate::simulation_tick;

pub const SINGLETON_ID: u8 = 0;
pub const NEUTRAL_PLAYER: u16 = 0;
pub const DEFAULT_PLAYER_COUNT: u16 = 2;
pub const MIN_PLAYER_COUNT: u16 = 2;
pub const MAX_PLAYER_COUNT: u16 = 500;
/// Compact HUD and full tactical subscriptions stay in the low-scale band.
pub const HIGH_SCALE_PLAYER_THRESHOLD: u16 = 8;
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
    Reshape,
    /// Explicit one-shot share of movable troops from one strategic front arc
    /// onto another front of the same owned component.
    FrontRebalance,
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
    /// The order hit an attributable invariant violation during a tick. Its
    /// packets were retired with strength conserved in place and the order is
    /// permanently parked; the rest of the match keeps running.
    Quarantined,
}

#[derive(SpacetimeType, Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiptStatus {
    Accepted,
    Rejected,
}

#[derive(Clone)]
#[spacetimedb::table(accessor = player_slot, public)]
pub struct PlayerSlot {
    #[primary_key]
    pub player_id: u16,
    pub identity: Option<Identity>,
    pub display_name: String,
    pub connected: bool,
    pub has_reconnected: bool,
    pub reconnect_count: u32,
    pub ready: bool,
    pub joined_at_us: u64,
    pub last_seen_at_us: u64,
}

/// O(1) identity → player lookup used by join, reconnect, disconnect, and
/// command authorization. Slots alone would force a linear scan of every
/// configured seat at 500-player scale.
#[derive(Clone)]
#[spacetimedb::table(accessor = player_identity)]
pub struct PlayerIdentity {
    #[primary_key]
    pub identity: Identity,
    #[unique]
    pub player_id: u16,
}

/// The identity that configured the lobby. Recorded on the first successful
/// `configure_match`/`configure_map`; only that identity may reconfigure, and
/// only until the first player joins. Private: purely an authorization record.
#[derive(Clone)]
#[spacetimedb::table(accessor = lobby_configurator)]
pub struct LobbyConfigurator {
    #[primary_key]
    pub singleton_id: u8,
    pub identity: Identity,
}

/// Per-player monotonic idempotency watermark for `client_command_id`.
///
/// Commands at or below the watermark are duplicates by contract, so
/// `CommandReceipt` rows older than the bounded feedback window can be pruned
/// without breaking dedup. Private: clients read receipts, never this row.
#[derive(Clone)]
#[spacetimedb::table(accessor = command_watermark)]
pub struct CommandWatermark {
    #[primary_key]
    pub player_id: u16,
    pub highest_client_command_id: u64,
}

#[derive(Clone)]
#[spacetimedb::table(accessor = match_config, public)]
pub struct MatchConfig {
    #[primary_key]
    pub singleton_id: u8,
    pub map_preset: MapPreset,
    pub player_count: u16,
    pub lobby_configuration_locked: bool,
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
    pub winner_player_id: u16,
    /// Number of configured seats that currently have a claimed identity.
    /// Join uses this instead of scanning every slot to decide when the match
    /// can leave the lobby.
    pub claimed_players: u16,
    /// Monotonic durable ownership/topology revision. Every capture or
    /// relinquishment advances it transactionally.
    pub ownership_revision: u64,
    pub started_at_us: u64,
    pub completed_at_us: u64,
}

/// Public per-player match projection. IDs are contiguous from one through
/// `MatchConfig::player_count`; zero is always neutral.
#[derive(Clone)]
#[spacetimedb::table(accessor = player_state, public)]
pub struct PlayerState {
    #[primary_key]
    pub player_id: u16,
    pub spawn_cell_id: u32,
    pub controlled_cells: u64,
}

#[derive(Clone)]
#[spacetimedb::table(accessor = mobilization_policy, public)]
pub struct MobilizationPolicy {
    #[primary_key]
    pub player_id: u16,
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
    index(accessor = state_by_owner, btree(columns = [owner_player_id])),
    index(accessor = state_by_population_shard, btree(columns = [population_shard])),
    index(accessor = state_by_chunk, btree(columns = [chunk_q, chunk_r]))
)]
pub struct CellState {
    #[primary_key]
    #[index(direct)]
    pub cell_id: u32,
    pub owner_player_id: u16,
    pub civilians: u64,
    pub civilian_capacity: u64,
    pub infantry: u64,
    pub military_capacity: u64,
    /// Deterministic shard used by high-scale population updates. Equals
    /// `cell_id % population_step_interval` at map generation so each cell keeps
    /// the same update frequency as the low-scale full-scan path.
    ///
    /// Stored as `u16` so `population_step_interval` values above 255 are not
    /// silently truncated when sharded.
    pub population_shard: u16,
    /// Denormalized terrain chunk coordinates (same formula as `CellTerrain`)
    /// so high-scale clients can subscribe to a bounded interest square without
    /// joining through the immutable terrain table.
    pub chunk_q: i16,
    pub chunk_r: i16,
    pub last_changed_step: u64,
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

#[derive(Clone)]
#[spacetimedb::table(
    accessor = command_receipt,
    public,
    index(accessor = receipt_by_player, btree(columns = [player_id]))
)]
pub struct CommandReceipt {
    #[primary_key]
    pub receipt_key: u128,
    pub player_id: u16,
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
    pub player_id: u16,
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
    index(accessor = source_by_order, btree(columns = [order_id])),
    index(accessor = source_by_player, btree(columns = [player_id]))
)]
pub struct TransferSource {
    #[primary_key]
    pub source_key: u128,
    pub order_id: u64,
    /// Denormalized owner for selective high-scale client subscriptions.
    pub player_id: u16,
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
    index(accessor = destination_by_order, btree(columns = [order_id])),
    index(accessor = destination_by_player, btree(columns = [player_id]))
)]
pub struct TransferDestination {
    #[primary_key]
    pub destination_key: u128,
    pub order_id: u64,
    /// Denormalized owner for selective high-scale client subscriptions.
    pub player_id: u16,
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
    index(accessor = route_by_order, btree(columns = [order_id])),
    index(accessor = route_by_player, btree(columns = [player_id]))
)]
pub struct TransitRoute {
    #[primary_key]
    #[auto_inc]
    pub route_id: u64,
    pub order_id: u64,
    /// Denormalized owner for selective high-scale client subscriptions.
    pub player_id: u16,
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
    index(accessor = packet_by_cell, btree(columns = [current_cell])),
    index(accessor = packet_by_owner, btree(columns = [owner_player_id]))
)]
pub struct TransitPacket {
    #[primary_key]
    #[auto_inc]
    pub packet_key: u64,
    pub order_id: u64,
    pub owner_player_id: u16,
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

/// Compact private topology for one branching neutral or cluster-attack wave.
///
/// `selected_cells` is the sorted set of owned perimeter cells that committed
/// troops when the order was accepted. Expansion starts directly at those
/// cells; it never creates an internal support corridor through owned ground.
/// `outside_depths[cell_id]` is the static multi-source BFS distance from the
/// first destination ring, starting at one, or `u16::MAX` when unreachable.
#[derive(Clone)]
#[spacetimedb::table(accessor = expansion_wave)]
pub struct ExpansionWave {
    #[primary_key]
    pub order_id: u64,
    pub selected_cells: Vec<u32>,
    pub outside_depths: Vec<u16>,
    /// Optional neutral click objective. Branch weights mildly favor children
    /// that move closer to this cell.
    pub focus_cell_id: Option<u32>,
    /// Immutable sorted enemy footprint for `AttackClusters`. Empty for both
    /// neutral expansion variants. Attack branches may never leave this mask.
    pub target_cells: Vec<u32>,
}

/// Sparse per-cell rotating remainder cursor for unbiased asynchronous wave
/// splits. Kept out of [`ExpansionWave`] so per-branch cursor updates never
/// rewrite the wave's large immutable depth field; rows exist only for cells
/// whose cursor has rotated away from zero.
#[derive(Clone)]
#[spacetimedb::table(
    accessor = expansion_split_cursor,
    index(accessor = cursor_by_order, btree(columns = [order_id]))
)]
pub struct ExpansionSplitCursor {
    /// `order_cell_key(order_id, cell_id)`.
    #[primary_key]
    pub cursor_key: u128,
    pub order_id: u64,
    pub cell_id: u32,
    pub cursor: u8,
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
    pub owner_player_id: u16,
    pub remaining_infantry: u64,
}

#[derive(Clone)]
#[spacetimedb::table(
    accessor = combat_front,
    public,
    index(accessor = front_by_target, btree(columns = [to_cell])),
    index(accessor = front_by_attacker, btree(columns = [attacker_player_id])),
    index(accessor = front_by_defender, btree(columns = [defender_player_id]))
)]
pub struct CombatFront {
    #[primary_key]
    pub front_key: String,
    pub attacker_player_id: u16,
    pub defender_player_id: u16,
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

/// Private singleton that gates debug-only reducers used by the live quarantine
/// integration harness. Production publish / lobby orchestration never inserts
/// this row, so `debug_break_order_conservation` is unreachable in production.
#[derive(Clone)]
#[spacetimedb::table(accessor = debug_harness)]
pub struct DebugHarness {
    #[primary_key]
    pub singleton_id: u8,
    pub enabled: bool,
}
