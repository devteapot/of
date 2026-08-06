//! Subscription query sets for coordinator and worker connections.

use crate::common::SubscriptionMode;

const PACKET_TABLE: &str = "transit_packet";
const ROUTE_TABLE: &str = "transit_route";

const HIGH_SCALE_PLAYER_THRESHOLD: u16 = 8;

/// Exact full-client projection mode recorded in worker logs / metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FullClientMode {
    /// `player_count <= 8`: full `cell_state` / combat / tactical like the game client.
    LowScale,
    /// `player_count > 8`: local-owned + spatial cell interest + filtered tactical.
    HighScale,
}

impl FullClientMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::LowScale => "full-client-low-scale",
            Self::HighScale => "full-client-high-scale",
        }
    }
}

pub fn full_client_mode(match_players: u16) -> FullClientMode {
    if match_players <= HIGH_SCALE_PLAYER_THRESHOLD {
        FullClientMode::LowScale
    } else {
        FullClientMode::HighScale
    }
}

/// Full telemetry observer used by the coordinator (and optional diagnostics).
pub fn coordinator_observer_queries() -> Vec<String> {
    [
        "SELECT * FROM cell_state",
        "SELECT * FROM cell_terrain",
        "SELECT * FROM combat_front",
        "SELECT * FROM match_config",
        "SELECT * FROM match_state",
        "SELECT * FROM player_slot",
        "SELECT * FROM player_state",
        "SELECT * FROM transfer_order",
        "SELECT * FROM transit_packet",
        "SELECT * FROM command_receipt",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// Shared per-worker map/phase observer: enough to join, expand, and attack.
/// Intentionally omits the tactical flood; one observer per worker is enough
/// for scenario derivation while per-seat connections carry client load.
pub fn worker_observer_queries() -> Vec<String> {
    [
        "SELECT * FROM cell_state",
        "SELECT * FROM cell_terrain",
        "SELECT * FROM combat_front",
        "SELECT * FROM match_config",
        "SELECT * FROM match_state",
        "SELECT * FROM player_slot",
        "SELECT * FROM player_state",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn packet_query(player_filter: Option<u16>) -> String {
    match player_filter {
        Some(player) => format!("SELECT * FROM {PACKET_TABLE} WHERE owner_player_id = {player}"),
        None => format!("SELECT * FROM {PACKET_TABLE}"),
    }
}

fn route_query(player_filter: Option<u16>) -> String {
    match player_filter {
        Some(player) => format!("SELECT * FROM {ROUTE_TABLE} WHERE player_id = {player}"),
        None => format!("SELECT * FROM {ROUTE_TABLE}"),
    }
}

/// Per-seat command connection queries.
///
/// - `command-only`: receipts for that seat only.
/// - `full-client`: mirrors game-client bootstrap + tactical (+ spatial at high
///   scale). Pass the configured match player count so `<=8` uses full
///   `cell_state` / combat / tactical while `>8` stays filtered/spatial.
///   Packet/route tables follow the same cfg raw-vs-visible split as the game
///   client.
pub fn worker_command_queries(
    player_id: u16,
    mode: SubscriptionMode,
    match_players: u16,
    spawn_chunk_q: i16,
    spawn_chunk_r: i16,
    interest_radius: i16,
) -> Vec<String> {
    match mode {
        SubscriptionMode::CommandOnly => vec![format!(
            "SELECT * FROM command_receipt WHERE player_id = {player_id}"
        )],
        SubscriptionMode::FullClient => match full_client_mode(match_players) {
            FullClientMode::LowScale => vec![
                "SELECT * FROM cell_terrain".to_owned(),
                "SELECT * FROM match_config".to_owned(),
                "SELECT * FROM match_state".to_owned(),
                "SELECT * FROM player_slot".to_owned(),
                "SELECT * FROM player_state".to_owned(),
                "SELECT * FROM cell_state".to_owned(),
                "SELECT * FROM combat_front".to_owned(),
                format!("SELECT * FROM command_receipt WHERE player_id = {player_id}"),
                "SELECT * FROM mobilization_policy".to_owned(),
                "SELECT * FROM transfer_destination".to_owned(),
                "SELECT * FROM transfer_order".to_owned(),
                "SELECT * FROM transfer_source".to_owned(),
                route_query(None),
                packet_query(None),
            ],
            FullClientMode::HighScale => {
                let qmin = spawn_chunk_q.saturating_sub(interest_radius);
                let qmax = spawn_chunk_q.saturating_add(interest_radius);
                let rmin = spawn_chunk_r.saturating_sub(interest_radius);
                let rmax = spawn_chunk_r.saturating_add(interest_radius);
                vec![
                    "SELECT * FROM cell_terrain".to_owned(),
                    "SELECT * FROM match_config".to_owned(),
                    "SELECT * FROM match_state".to_owned(),
                    "SELECT * FROM player_slot".to_owned(),
                    "SELECT * FROM player_state".to_owned(),
                    format!("SELECT * FROM cell_state WHERE owner_player_id = {player_id}"),
                    format!(
                        "SELECT * FROM cell_state WHERE chunk_q >= {qmin} AND chunk_q <= {qmax} AND chunk_r >= {rmin} AND chunk_r <= {rmax}"
                    ),
                    format!("SELECT * FROM combat_front WHERE attacker_player_id = {player_id}"),
                    format!("SELECT * FROM combat_front WHERE defender_player_id = {player_id}"),
                    format!("SELECT * FROM command_receipt WHERE player_id = {player_id}"),
                    format!("SELECT * FROM mobilization_policy WHERE player_id = {player_id}"),
                    format!("SELECT * FROM transfer_destination WHERE player_id = {player_id}"),
                    format!("SELECT * FROM transfer_order WHERE player_id = {player_id}"),
                    format!("SELECT * FROM transfer_source WHERE player_id = {player_id}"),
                    route_query(Some(player_id)),
                    packet_query(Some(player_id)),
                ]
            }
        },
    }
}

/// Label recorded for the exact subscription projection used by a seat.
pub fn subscription_mode_detail(mode: SubscriptionMode, match_players: u16) -> &'static str {
    match mode {
        SubscriptionMode::CommandOnly => "command-only",
        SubscriptionMode::FullClient => full_client_mode(match_players).label(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_command_queries_filter_single_player_in_command_only() {
        let queries = worker_command_queries(42, SubscriptionMode::CommandOnly, 500, 0, 0, 2);
        assert_eq!(queries.len(), 1);
        assert_eq!(
            queries[0],
            "SELECT * FROM command_receipt WHERE player_id = 42"
        );
        assert!(!queries.iter().any(|q| q.contains("cell_state")));
        assert!(!queries.iter().any(|q| q.contains("transfer_order")));
    }

    #[test]
    fn worker_full_client_low_scale_mirrors_game_client_full_tables() {
        let queries = worker_command_queries(3, SubscriptionMode::FullClient, 8, 0, 0, 2);
        assert!(queries.iter().any(|q| q == "SELECT * FROM cell_state"));
        assert!(queries.iter().any(|q| q == "SELECT * FROM combat_front"));
        assert!(queries.iter().any(|q| q == "SELECT * FROM transfer_order"));
        assert!(queries.iter().any(|q| q == &packet_query(None)));
        assert!(queries.iter().any(|q| q == &route_query(None)));
        assert!(queries.iter().all(|q| !q.contains("chunk_q")));
        assert_eq!(
            subscription_mode_detail(SubscriptionMode::FullClient, 8),
            "full-client-low-scale"
        );
    }

    #[test]
    fn worker_full_client_queries_mirror_high_scale_game_client() {
        let queries = worker_command_queries(500, SubscriptionMode::FullClient, 500, 3, -1, 2);
        assert!(queries.iter().any(|q| q.contains("cell_terrain")));
        assert!(
            queries
                .iter()
                .any(|q| q == "SELECT * FROM cell_state WHERE owner_player_id = 500")
        );
        assert!(queries.iter().any(|q| {
            q.contains("chunk_q >= 1") && q.contains("chunk_q <= 5") && q.contains("chunk_r >= -3")
        }));
        assert!(queries.iter().any(|q| q == &packet_query(Some(500))));
        // No unfiltered full tactical flood.
        assert!(queries.iter().all(|q| q != "SELECT * FROM cell_state"));
        assert!(queries.iter().all(|q| q != "SELECT * FROM transfer_order"));
        assert_eq!(
            subscription_mode_detail(SubscriptionMode::FullClient, 500),
            "full-client-high-scale"
        );
    }

    #[test]
    fn packet_and_route_queries_use_explicit_order_tables() {
        assert!(packet_query(None).contains("transit_packet"));
        assert!(route_query(Some(9)).contains("transit_route"));
    }

    #[test]
    fn worker_observer_omits_tactical_flood() {
        let queries = worker_observer_queries();
        assert!(queries.iter().any(|q| q.contains("match_state")));
        assert!(queries.iter().any(|q| q.contains("cell_terrain")));
        assert!(queries.iter().all(|q| !q.contains("transit_packet")));
        assert!(queries.iter().all(|q| !q.contains("transfer_order")));
        assert!(queries.iter().all(|q| !q.contains("command_receipt")));
    }

    #[test]
    fn coordinator_observer_includes_load_metrics_tables() {
        let queries = coordinator_observer_queries();
        assert!(queries.iter().any(|q| q.contains("transit_packet")));
        assert!(queries.iter().any(|q| q.contains("transfer_order")));
        assert!(queries.iter().any(|q| q.contains("combat_front")));
        assert!(queries.iter().any(|q| q.contains("player_state")));
    }
}
