//! Continuous global-invariant instrumentation for one live session.
//!
//! A dedicated observer connection samples the public tables a few times per
//! logical step and checks:
//!
//! - total strength conservation between stable snapshots (exact while no
//!   combat or mobilization is expected, casualty-accounted during combat,
//!   growth-accounted while mobilization is enabled);
//! - per-order conservation (`committed == in_transit + delivered + casualties`);
//! - no cell above its military capacity;
//! - `PlayerState::controlled_cells` consistency with actual cell ownership;
//! - tick liveness (`logical_step` advancing);
//! - physical packet traversal (routed packets stay on their persisted route
//!   and only move forward along it — no teleporting).
//!
//! SpacetimeDB SDK row callbacks from one transaction can reach the client
//! cache across turns, so cross-table checks only fire after the same
//! violation persists over several samples spanning multiple logical steps,
//! and conservation deltas are only evaluated between "stable" snapshots
//! (two consecutive identical reads).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use match_bindings::{
    CellStateTableAccess, CombatFrontTableAccess, MatchStateTableAccess, OrderStatus,
    PlayerStateTableAccess, TransferOrderTableAccess, TransitPacketTableAccess,
    TransitRouteTableAccess,
};
use serde::Serialize;
use spacetimedb_sdk::Table;

use crate::client::Client;
use crate::world::SINGLETON_ID;

/// Expected accounting regime for the current scenario phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum Mode {
    /// No combat, no mobilization: total infantry must be exactly constant.
    Strict,
    /// Enemy combat expected: decreases must be covered by casualties.
    Combat,
    /// Mobilization enabled: population converts into infantry locally, so
    /// only monotonicity of (civilians + infantry) is enforced.
    Mobilization,
}

impl Mode {
    const fn as_u8(self) -> u8 {
        match self {
            Self::Strict => 0,
            Self::Combat => 1,
            Self::Mobilization => 2,
        }
    }

    const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Combat,
            2 => Self::Mobilization,
            _ => Self::Strict,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Violation {
    pub rule: String,
    pub detail: String,
    pub logical_step: u64,
    pub mode: Mode,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct WindowSummary {
    pub mode: String,
    pub first_step: u64,
    pub last_step: u64,
    pub start_infantry: u64,
    pub end_infantry: u64,
    pub attacker_casualty_delta: u64,
    /// Additional strength decrease attributed to defender-side losses.
    pub defender_loss_residual: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct MonitorReport {
    pub samples: u64,
    pub stable_checkpoints: u64,
    pub first_step: u64,
    pub last_step: u64,
    pub max_cell_fill_ratio_bps: u64,
    pub tracked_packet_transitions: u64,
    pub violations: Vec<Violation>,
    pub windows: Vec<WindowSummary>,
}

#[derive(Clone, PartialEq)]
struct Snapshot {
    logical_step: u64,
    total_infantry: u64,
    total_population: u64,
    attacker_casualties: u64,
}

struct PendingViolation {
    consecutive: u32,
    first_step: u64,
    last_step: u64,
    detail: String,
}

struct MonitorState {
    report: MonitorReport,
    previous_stable: Option<Snapshot>,
    window: Option<WindowSummary>,
    pending: HashMap<String, PendingViolation>,
    packet_tracks: HashMap<u64, (u64, u32, u32)>,
    last_step_change: Instant,
    last_seen_step: u64,
}

pub struct Monitor {
    stop: Arc<AtomicBool>,
    mode: Arc<AtomicU8>,
    window_epoch: Arc<AtomicU8>,
    state: Arc<Mutex<MonitorState>>,
    thread: Option<JoinHandle<Client>>,
}

/// How many consecutive samples (spanning at least two logical steps) a
/// cross-table inconsistency must persist before it is a real violation.
const PERSISTENCE_SAMPLES: u32 = 4;
const LIVENESS_BUDGET: Duration = Duration::from_secs(10);

impl Monitor {
    pub fn start(observer: Client, poll: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let mode = Arc::new(AtomicU8::new(Mode::Strict.as_u8()));
        let window_epoch = Arc::new(AtomicU8::new(0));
        let state = Arc::new(Mutex::new(MonitorState {
            report: MonitorReport::default(),
            previous_stable: None,
            window: None,
            pending: HashMap::new(),
            packet_tracks: HashMap::new(),
            last_step_change: Instant::now(),
            last_seen_step: 0,
        }));
        let thread = {
            let stop = Arc::clone(&stop);
            let mode = Arc::clone(&mode);
            let window_epoch = Arc::clone(&window_epoch);
            let state = Arc::clone(&state);
            thread::spawn(move || {
                let mut seen_epoch = u8::MAX;
                while !stop.load(Ordering::Relaxed) {
                    let current_mode = Mode::from_u8(mode.load(Ordering::Relaxed));
                    let epoch = window_epoch.load(Ordering::Relaxed);
                    if epoch != seen_epoch {
                        seen_epoch = epoch;
                        let mut guard = state.lock().expect("monitor state poisoned");
                        let finished = guard.window.take();
                        if let Some(window) = finished {
                            guard.report.windows.push(window);
                        }
                        guard.previous_stable = None;
                    }
                    sample(&observer, current_mode, &state);
                    thread::sleep(poll);
                }
                let mut guard = state.lock().expect("monitor state poisoned");
                let finished = guard.window.take();
                if let Some(window) = finished {
                    guard.report.windows.push(window);
                }
                drop(guard);
                observer
            })
        };
        Self {
            stop,
            mode,
            window_epoch,
            state,
            thread: Some(thread),
        }
    }

    /// Switches the conservation regime and closes the current window.
    pub fn set_mode(&self, mode: Mode) {
        self.mode.store(mode.as_u8(), Ordering::Relaxed);
        self.window_epoch.fetch_add(1, Ordering::Relaxed);
    }

    pub fn finish(mut self) -> (MonitorReport, Client) {
        self.stop.store(true, Ordering::Relaxed);
        let thread = self.thread.take().expect("monitor already finished");
        let observer = thread.join().expect("monitor thread panicked");
        let report = self
            .state
            .lock()
            .expect("monitor state poisoned")
            .report
            .clone();
        (report, observer)
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn read_snapshot(observer: &Client) -> Option<Snapshot> {
    let state = observer
        .conn
        .db
        .match_state()
        .singleton_id()
        .find(&SINGLETON_ID)?;
    let mut total_infantry = 0_u64;
    let mut total_population = 0_u64;
    for cell in observer.conn.db.cell_state().iter() {
        total_infantry = total_infantry.saturating_add(cell.infantry);
        total_population = total_population
            .saturating_add(cell.infantry)
            .saturating_add(cell.civilians);
    }
    let attacker_casualties = observer
        .conn
        .db
        .transfer_order()
        .iter()
        .map(|order| order.casualty_infantry)
        .fold(0_u64, u64::saturating_add);
    Some(Snapshot {
        logical_step: state.logical_step,
        total_infantry,
        total_population,
        attacker_casualties,
    })
}

#[allow(clippy::too_many_lines)]
fn sample(observer: &Client, mode: Mode, state: &Arc<Mutex<MonitorState>>) {
    let Some(first) = read_snapshot(observer) else {
        return;
    };
    let second = read_snapshot(observer);
    let stable = second.as_ref() == Some(&first);
    let logical_step = first.logical_step;

    let mut immediate_violations: Vec<(String, String)> = Vec::new();
    let mut persistent_candidates: Vec<(String, String)> = Vec::new();

    // Per-order conservation. Row reads are atomic, but an order and its
    // packets may land in different turns; the counter identity below only
    // involves one row, so a persistent breach is a real accounting bug.
    for order in observer.conn.db.transfer_order().iter() {
        let accounted = order
            .in_transit_infantry
            .checked_add(order.delivered_infantry)
            .and_then(|value| value.checked_add(order.casualty_infantry));
        if accounted != Some(order.committed_infantry) {
            persistent_candidates.push((
                format!("order-conservation:{}", order.order_id),
                format!(
                    "order {} kind {:?} status {:?}: committed={} in_transit={} delivered={} casualties={}",
                    order.order_id,
                    order.kind,
                    order.status,
                    order.committed_infantry,
                    order.in_transit_infantry,
                    order.delivered_infantry,
                    order.casualty_infantry
                ),
            ));
        }
        if order.status != OrderStatus::Active && order.in_transit_infantry != 0 {
            persistent_candidates.push((
                format!("settled-order-transit:{}", order.order_id),
                format!(
                    "non-active order {} still reports {} in transit",
                    order.order_id, order.in_transit_infantry
                ),
            ));
        }
    }

    // Capacity and ownership tallies.
    let mut owned_counts: HashMap<u16, u64> = HashMap::new();
    let mut max_fill_bps = 0_u64;
    for cell in observer.conn.db.cell_state().iter() {
        if cell.owner_player_id != 0 {
            *owned_counts.entry(cell.owner_player_id).or_insert(0) += 1;
        }
        if let Some(fill) = cell
            .infantry
            .saturating_mul(10_000)
            .checked_div(cell.military_capacity)
        {
            max_fill_bps = max_fill_bps.max(fill);
        }
        if cell.infantry > cell.military_capacity {
            persistent_candidates.push((
                format!("cell-over-capacity:{}", cell.cell_id),
                format!(
                    "cell {} owner {} infantry {} exceeds military capacity {}",
                    cell.cell_id, cell.owner_player_id, cell.infantry, cell.military_capacity
                ),
            ));
        }
    }
    for player in observer.conn.db.player_state().iter() {
        let actual = owned_counts.get(&player.player_id).copied().unwrap_or(0);
        if player.controlled_cells != actual {
            persistent_candidates.push((
                format!("controlled-cells:{}", player.player_id),
                format!(
                    "player {} reports {} controlled cells but owns {}",
                    player.player_id, player.controlled_cells, actual
                ),
            ));
        }
    }

    // Physical traversal: routed packets must sit on their persisted route and
    // only move forward along it. Packet keys are auto-increment and never
    // reused, so a shrinking route_index is a teleport/rewind.
    let routes: HashMap<u64, Vec<u32>> = observer
        .conn
        .db
        .transit_route()
        .iter()
        .map(|route| (route.route_id, route.cells))
        .collect();
    let mut transitions = 0_u64;
    {
        let mut guard = state.lock().expect("monitor state poisoned");
        for packet in observer.conn.db.transit_packet().iter() {
            if packet.route_id == 0 {
                continue;
            }
            let Some(cells) = routes.get(&packet.route_id) else {
                // Route row may arrive in a later turn than the packet row.
                continue;
            };
            let index = packet.route_index as usize;
            if cells.get(index) != Some(&packet.current_cell) {
                immediate_violations.push((
                    "packet-off-route".to_owned(),
                    format!(
                        "packet {} of order {} sits on cell {} but route {} index {} is {:?}",
                        packet.packet_key,
                        packet.order_id,
                        packet.current_cell,
                        packet.route_id,
                        packet.route_index,
                        cells.get(index)
                    ),
                ));
            }
            if let Some(&(previous_route, previous_index, previous_cell)) =
                guard.packet_tracks.get(&packet.packet_key)
            {
                if previous_route == packet.route_id && packet.route_index < previous_index {
                    immediate_violations.push((
                        "packet-rewind".to_owned(),
                        format!(
                            "packet {} rewound from route index {} (cell {}) to {} (cell {})",
                            packet.packet_key,
                            previous_index,
                            previous_cell,
                            packet.route_index,
                            packet.current_cell
                        ),
                    ));
                }
                if previous_route == packet.route_id && packet.route_index != previous_index {
                    transitions += 1;
                }
            }
            guard.packet_tracks.insert(
                packet.packet_key,
                (packet.route_id, packet.route_index, packet.current_cell),
            );
        }
        guard.report.tracked_packet_transitions += transitions;
        guard.report.max_cell_fill_ratio_bps =
            guard.report.max_cell_fill_ratio_bps.max(max_fill_bps);
    }

    // Combat-front casualty context for the strict/combat distinction.
    let front_casualties_this_step: u64 = observer
        .conn
        .db
        .combat_front()
        .iter()
        .filter(|front| front.logical_step == logical_step)
        .map(|front| front.attacker_casualties + front.defender_casualties)
        .sum();

    let mut guard = state.lock().expect("monitor state poisoned");
    guard.report.samples += 1;
    if guard.report.first_step == 0 {
        guard.report.first_step = logical_step;
    }
    guard.report.last_step = guard.report.last_step.max(logical_step);

    // Liveness.
    if logical_step != guard.last_seen_step {
        guard.last_seen_step = logical_step;
        guard.last_step_change = Instant::now();
    } else if guard.last_step_change.elapsed() > LIVENESS_BUDGET {
        guard.last_step_change = Instant::now();
        let violation = Violation {
            rule: "tick-liveness".to_owned(),
            detail: format!(
                "logical_step stalled at {logical_step} for more than {LIVENESS_BUDGET:?}"
            ),
            logical_step,
            mode,
        };
        guard.report.violations.push(violation);
    }

    for (rule, detail) in immediate_violations {
        guard.report.violations.push(Violation {
            rule,
            detail,
            logical_step,
            mode,
        });
    }

    // Persistence-filtered cross-table checks.
    let mut still_pending: HashMap<String, PendingViolation> = HashMap::new();
    for (key, detail) in persistent_candidates {
        let entry = guard.pending.remove(&key);
        let mut pending = entry.unwrap_or(PendingViolation {
            consecutive: 0,
            first_step: logical_step,
            last_step: logical_step,
            detail: String::new(),
        });
        pending.consecutive += 1;
        pending.last_step = logical_step;
        pending.detail = detail;
        if pending.consecutive >= PERSISTENCE_SAMPLES && pending.last_step > pending.first_step {
            let rule = key.split(':').next().unwrap_or(&key).to_owned();
            guard.report.violations.push(Violation {
                rule,
                detail: pending.detail.clone(),
                logical_step,
                mode,
            });
            pending.consecutive = 0;
            pending.first_step = logical_step;
        }
        still_pending.insert(key, pending);
    }
    guard.pending = still_pending;

    if !stable {
        return;
    }
    guard.report.stable_checkpoints += 1;

    // Conservation between stable checkpoints of the current window.
    if let Some(previous) = guard.previous_stable.clone() {
        let increase = first.total_infantry.saturating_sub(previous.total_infantry);
        let decrease = previous.total_infantry.saturating_sub(first.total_infantry);
        let casualty_delta = first
            .attacker_casualties
            .saturating_sub(previous.attacker_casualties);
        match mode {
            Mode::Strict => {
                if first.total_infantry != previous.total_infantry {
                    guard.report.violations.push(Violation {
                        rule: "strict-conservation".to_owned(),
                        detail: format!(
                            "total infantry moved from {} (step {}) to {} (step {}) with no combat or mobilization expected (order casualty delta {})",
                            previous.total_infantry,
                            previous.logical_step,
                            first.total_infantry,
                            logical_step,
                            casualty_delta
                        ),
                        logical_step,
                        mode,
                    });
                }
            }
            Mode::Combat => {
                if increase > 0 {
                    guard.report.violations.push(Violation {
                        rule: "combat-conservation".to_owned(),
                        detail: format!(
                            "total infantry increased by {increase} during combat with mobilization disabled (steps {}..{})",
                            previous.logical_step, logical_step
                        ),
                        logical_step,
                        mode,
                    });
                }
                if decrease < casualty_delta {
                    guard.report.violations.push(Violation {
                        rule: "combat-conservation".to_owned(),
                        detail: format!(
                            "orders recorded {casualty_delta} casualties but total infantry only dropped by {decrease} (steps {}..{})",
                            previous.logical_step, logical_step
                        ),
                        logical_step,
                        mode,
                    });
                }
            }
            Mode::Mobilization => {
                if first.total_population < previous.total_population
                    && front_casualties_this_step == 0
                {
                    guard.report.violations.push(Violation {
                        rule: "mobilization-conservation".to_owned(),
                        detail: format!(
                            "total population dropped from {} to {} without combat while mobilizing (steps {}..{})",
                            previous.total_population,
                            first.total_population,
                            previous.logical_step,
                            logical_step
                        ),
                        logical_step,
                        mode,
                    });
                }
            }
        }
        let window = guard.window.get_or_insert_with(|| WindowSummary {
            mode: format!("{mode:?}"),
            first_step: previous.logical_step,
            last_step: logical_step,
            start_infantry: previous.total_infantry,
            end_infantry: first.total_infantry,
            attacker_casualty_delta: 0,
            defender_loss_residual: 0,
        });
        window.last_step = logical_step;
        window.end_infantry = first.total_infantry;
        window.attacker_casualty_delta += casualty_delta;
        window.defender_loss_residual += decrease.saturating_sub(casualty_delta);
    }
    guard.previous_stable = Some(first);
}
