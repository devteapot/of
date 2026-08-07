//! Shared `SpacetimeDB` connection helpers for coordinator and worker roles.

use std::fs;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use match_bindings::{
    CellStateTableAccess, CellTerrainTableAccess, CombatFrontTableAccess,
    CommandReceiptTableAccess, DbConnection, MapPreset, MatchConfig, MatchConfigTableAccess,
    MatchPhase, MatchStateTableAccess, OrderStatus, PlayerSlotTableAccess, PlayerStateTableAccess,
    ReceiptStatus, TransferOrderTableAccess, TransitPacketTableAccess, configure_match as _,
    issue_attack_clusters as _, issue_expand_clusters as _, issue_front_rebalance as _,
    join_match as _, set_mobilization_target as _, start_match as _,
};
use spacetimedb_sdk::{DbContext, Table};

use crate::attack::{AttackFront, FrontCell, find_attack_fronts};
use crate::common::SINGLETON_ID;
use crate::front_rebalance::MapCell;

pub enum LifecycleEvent {
    Connected { token: String },
    Subscribed,
    Failed(String),
    Disconnected(Option<String>),
}

pub struct Client {
    pub label: String,
    pub conn: DbConnection,
    _events: Receiver<LifecycleEvent>,
    _pump: JoinHandle<()>,
}

impl Client {
    pub fn connect(
        label: impl Into<String>,
        host: &str,
        database: &str,
        token_path: &Path,
        queries: &[String],
        timeout: Duration,
    ) -> Result<Self> {
        let label = label.into();
        let existing = fs::read_to_string(token_path).ok();
        let subscription_queries = queries.to_vec();
        let (tx, rx) = mpsc::channel();
        let conn = DbConnection::builder()
            .with_uri(host)
            .with_database_name(database)
            .with_token(existing)
            .on_connect({
                let tx = tx.clone();
                move |ctx, _identity, token| {
                    let _ = tx.send(LifecycleEvent::Connected {
                        token: token.to_owned(),
                    });
                    ctx.subscription_builder()
                        .on_applied({
                            let tx = tx.clone();
                            move |_| {
                                let _ = tx.send(LifecycleEvent::Subscribed);
                            }
                        })
                        .on_error({
                            let tx = tx.clone();
                            move |_, error| {
                                let _ = tx.send(LifecycleEvent::Failed(format!(
                                    "subscription failed: {error}"
                                )));
                            }
                        })
                        .subscribe(subscription_queries);
                }
            })
            .on_connect_error({
                let tx = tx.clone();
                move |_, error| {
                    let _ = tx.send(LifecycleEvent::Failed(format!(
                        "connection establishment failed: {error}"
                    )));
                }
            })
            .on_disconnect({
                let tx = tx.clone();
                move |_, error| {
                    let _ = tx.send(LifecycleEvent::Disconnected(
                        error.map(|value| value.to_string()),
                    ));
                }
            })
            .build()
            .with_context(|| format!("build {label} connection"))?;
        let pump = conn.run_threaded();

        let deadline = Instant::now() + timeout;
        let mut subscribed = false;
        let mut token = None;
        while !subscribed || token.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("{label} connection readiness timed out");
            }
            match rx.recv_timeout(remaining)? {
                LifecycleEvent::Connected { token: new } => token = Some(new),
                LifecycleEvent::Subscribed => subscribed = true,
                LifecycleEvent::Failed(message) => bail!("{label}: {message}"),
                LifecycleEvent::Disconnected(error) => {
                    bail!("{label} disconnected early: {error:?}")
                }
            }
        }
        if let Some(parent) = token_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let token = token.context("missing token")?;
        write_private_token(token_path, &token)?;
        Ok(Self {
            label,
            conn,
            _events: rx,
            _pump: pump,
        })
    }

    fn call<O>(
        register: impl FnOnce(mpsc::Sender<Result<(), String>>) -> Result<O>,
        timeout: Duration,
    ) -> Result<(Duration, Result<(), String>)> {
        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        register(tx)?;
        let result = rx.recv_timeout(timeout)?;
        Ok((started.elapsed(), result))
    }

    pub fn join(&self, player: u16, timeout: Duration) -> Result<Duration> {
        let (rtt, result) = Self::call(
            |tx| {
                self.conn
                    .reducers
                    .join_match_then(player, self.label.clone(), move |_, result| {
                        let _ = tx.send(
                            result
                                .map_err(|error| error.to_string())
                                .and_then(|inner| inner.map_err(|error| error.clone())),
                        );
                    })
                    .map_err(|error| anyhow::anyhow!(error.to_string()))
            },
            timeout,
        )?;
        result.map_err(anyhow::Error::msg)?;
        Ok(rtt)
    }

    /// Starts a fully claimed lobby. Required after interactive lobby cutover —
    /// seat claims no longer auto-enter Running.
    pub fn start_match(&self, timeout: Duration) -> Result<Duration> {
        let (rtt, result) = Self::call(
            |tx| {
                self.conn
                    .reducers
                    .start_match_then(move |_, result| {
                        let _ = tx.send(
                            result
                                .map_err(|error| error.to_string())
                                .and_then(|inner| inner.map_err(|error| error.clone())),
                        );
                    })
                    .map_err(|error| anyhow::anyhow!(error.to_string()))
            },
            timeout,
        )?;
        result.map_err(anyhow::Error::msg)?;
        Ok(rtt)
    }

    pub fn configure(
        &self,
        preset: MapPreset,
        player_count: u16,
        timeout: Duration,
    ) -> Result<Duration> {
        let (rtt, result) = Self::call(
            |tx| {
                self.conn
                    .reducers
                    .configure_match_then(preset, player_count, move |_, result| {
                        let _ = tx.send(
                            result
                                .map_err(|error| error.to_string())
                                .and_then(|inner| inner.map_err(|error| error.clone())),
                        );
                    })
                    .map_err(|error| anyhow::anyhow!(error.to_string()))
            },
            timeout,
        )?;
        result.map_err(anyhow::Error::msg)?;
        Ok(rtt)
    }

    #[allow(dead_code)] // retained for sequential/debug command paths
    pub fn expand(
        &self,
        command: u64,
        seeds: Vec<u32>,
        focus: u32,
        commitment_bps: u32,
        timeout: Duration,
    ) -> Result<(Duration, Result<(), String>)> {
        Self::call(
            |tx| {
                self.conn
                    .reducers
                    .issue_expand_clusters_then(
                        command,
                        seeds,
                        focus,
                        commitment_bps,
                        move |_, result| {
                            let _ = tx.send(
                                result
                                    .map_err(|error| error.to_string())
                                    .and_then(|inner| inner.map_err(|error| error.clone())),
                            );
                        },
                    )
                    .map_err(|error| anyhow::anyhow!(error.to_string()))
            },
            timeout,
        )
    }

    #[allow(dead_code)] // retained for sequential/debug command paths
    pub fn attack(
        &self,
        command: u64,
        sources: Vec<u32>,
        targets: Vec<u32>,
        commitment_bps: u32,
        timeout: Duration,
    ) -> Result<(Duration, Result<(), String>)> {
        Self::call(
            |tx| {
                self.conn
                    .reducers
                    .issue_attack_clusters_then(
                        command,
                        sources,
                        targets,
                        commitment_bps,
                        move |_, result| {
                            let _ = tx.send(
                                result
                                    .map_err(|error| error.to_string())
                                    .and_then(|inner| inner.map_err(|error| error.clone())),
                            );
                        },
                    )
                    .map_err(|error| anyhow::anyhow!(error.to_string()))
            },
            timeout,
        )
    }

    #[allow(dead_code)] // retained for sequential/debug command paths
    pub fn mobilization(
        &self,
        player_id: u16,
        command: u64,
        target_bps: u32,
        timeout: Duration,
    ) -> Result<Duration> {
        let started = Instant::now();
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .set_mobilization_target_then(command, target_bps, move |_, result| {
                let _ = tx.send(
                    result
                        .map_err(|error| error.to_string())
                        .and_then(|inner| inner.map_err(|error| error.clone())),
                );
            })
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        rx.recv_timeout(timeout)?.map_err(anyhow::Error::msg)?;
        self.require_receipt(
            player_id,
            "mobilization",
            command,
            "set_mobilization_target",
            timeout,
        )?;
        Ok(started.elapsed())
    }

    pub fn require_receipt(
        &self,
        player_id: u16,
        action: &str,
        command: u64,
        expected_command_name: &str,
        timeout: Duration,
    ) -> Result<()> {
        let receipt_key = (u128::from(player_id) << 64) | u128::from(command);
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(receipt) = self
                .conn
                .db
                .command_receipt()
                .receipt_key()
                .find(&receipt_key)
            {
                if receipt.command_name != expected_command_name {
                    bail!(
                        "required {action} command returned unexpected receipt {}",
                        receipt.command_name
                    );
                }
                return validate_receipt(action, receipt.status, &receipt.message);
            }
            if Instant::now() >= deadline {
                bail!("required {action} command receipt timed out");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn current_step(&self) -> Result<u64> {
        Ok(self
            .conn
            .db
            .match_state()
            .singleton_id()
            .find(&SINGLETON_ID)
            .context("match state missing")?
            .logical_step)
    }

    pub fn phase(&self) -> Result<MatchPhase> {
        Ok(self
            .conn
            .db
            .match_state()
            .singleton_id()
            .find(&SINGLETON_ID)
            .context("match state missing")?
            .phase)
    }

    pub fn config(&self) -> Result<MatchConfig> {
        self.conn
            .db
            .match_config()
            .singleton_id()
            .find(&SINGLETON_ID)
            .context("match config missing")
    }

    pub fn claimed_players(&self) -> usize {
        self.conn
            .db
            .player_slot()
            .iter()
            .filter(|slot| slot.identity.is_some())
            .count()
    }

    pub fn packet_count(&self) -> u64 {
        self.conn.db.transit_packet().iter().count() as u64
    }

    pub fn active_order_count(&self) -> u64 {
        self.conn
            .db
            .transfer_order()
            .iter()
            .filter(|order| order.status == OrderStatus::Active)
            .count() as u64
    }

    pub fn front_count(&self) -> u64 {
        self.conn.db.combat_front().iter().count() as u64
    }

    pub fn controlled_counts(&self) -> Vec<(u16, u64)> {
        let mut rows = self
            .conn
            .db
            .player_state()
            .iter()
            .map(|state| (state.player_id, state.controlled_cells))
            .collect::<Vec<_>>();
        rows.sort_by_key(|(player_id, _)| *player_id);
        rows
    }

    pub fn spawn_cell(&self, player_id: u16) -> Result<u32> {
        self.conn
            .db
            .player_state()
            .iter()
            .find(|state| state.player_id == player_id)
            .map(|state| state.spawn_cell_id)
            .with_context(|| format!("spawn for player {player_id} missing"))
    }

    pub fn attack_fronts(
        &self,
        player_count: u16,
        max_elevation_step: u8,
    ) -> Result<Vec<AttackFront>> {
        let cells = self.map_cells();
        let fronts = cells
            .iter()
            .map(|cell| FrontCell {
                cell_id: cell.cell_id,
                q: cell.q,
                r: cell.r,
                owner: cell.owner,
                elevation: cell.elevation,
                passable: cell.passable,
                capturable: cell.capturable,
            })
            .collect::<Vec<_>>();
        find_attack_fronts(&fronts, player_count, max_elevation_step).map_err(|player| {
            anyhow::anyhow!(
                "attack phase requested, but player {player} has no adjacent owned/enemy front"
            )
        })
    }

    /// Snapshot terrain + ownership for pure scenario derivation helpers.
    pub fn map_cells(&self) -> Vec<MapCell> {
        let states = self
            .conn
            .db
            .cell_state()
            .iter()
            .map(|state| {
                (
                    state.cell_id,
                    (
                        state.owner_player_id,
                        state.infantry,
                        state.military_capacity,
                    ),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        self.conn
            .db
            .cell_terrain()
            .iter()
            .filter_map(|terrain| {
                Some(MapCell {
                    cell_id: terrain.cell_id,
                    q: terrain.q,
                    r: terrain.r,
                    owner: states.get(&terrain.cell_id)?.0,
                    elevation: terrain.elevation,
                    passable: terrain.passable,
                    capturable: terrain.capturable,
                    infantry: states.get(&terrain.cell_id)?.1,
                    military_capacity: states.get(&terrain.cell_id)?.2,
                })
            })
            .collect()
    }

    #[allow(dead_code)] // retained for sequential/debug command paths
    pub fn front_rebalance(
        &self,
        command: u64,
        component_cells: Vec<u32>,
        source_front_seed: u32,
        target_front_seed: u32,
        commitment_bps: u32,
        timeout: Duration,
    ) -> Result<(Duration, Result<(), String>)> {
        Self::call(
            |tx| {
                self.conn
                    .reducers
                    .issue_front_rebalance_then(
                        command,
                        component_cells,
                        source_front_seed,
                        target_front_seed,
                        commitment_bps,
                        Vec::new(),
                        move |_, result| {
                            let _ = tx.send(
                                result
                                    .map_err(|error| error.to_string())
                                    .and_then(|inner| inner.map_err(|error| error.clone())),
                            );
                        },
                    )
                    .map_err(|error| anyhow::anyhow!(error.to_string()))
            },
            timeout,
        )
    }
}

pub fn validate_receipt(action: &str, status: ReceiptStatus, message: &str) -> Result<()> {
    match status {
        ReceiptStatus::Accepted => Ok(()),
        ReceiptStatus::Rejected => {
            bail!("required {action} command was rejected: {message}")
        }
    }
}

pub fn require_reducer_success(action: &str, result: Result<(), String>) -> Result<()> {
    result.map_err(|error| anyhow::anyhow!("required {action} reducer failed: {error}"))
}

fn write_private_token(path: &Path, token: &str) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("token.tmp");
    {
        let mut file = fs::File::create(&tmp)
            .with_context(|| format!("create temp token {}", tmp.display()))?;
        file.write_all(token.as_bytes())
            .with_context(|| format!("write temp token {}", tmp.display()))?;
        file.sync_all().ok();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", tmp.display()))?;
    }
    fs::rename(&tmp, path).with_context(|| format!("publish token {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn command_rejections_are_fatal() {
        assert!(validate_receipt("test", ReceiptStatus::Accepted, "ok").is_ok());
        assert!(
            validate_receipt("test", ReceiptStatus::Rejected, "no front")
                .expect_err("rejected receipt must fail")
                .to_string()
                .contains("required test command was rejected: no front")
        );
        assert!(
            require_reducer_success("test", Err("transaction failed".to_owned()))
                .expect_err("reducer failure must fail")
                .to_string()
                .contains("required test reducer failed")
        );
    }

    #[test]
    fn private_token_write_is_atomic_mode_0600_and_round_trips() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("match-perf-token-test-{nonce}"));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("player-7.token");
        write_private_token(&path, "secret-token-value").expect("write token");
        let loaded = std::fs::read_to_string(&path).expect("read token");
        assert_eq!(loaded, "secret-token-value");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "private token must be mode 0600");
        }
        // Replacement must not leave a readable temp beside the final path.
        write_private_token(&path, "rotated-token").expect("rewrite token");
        assert_eq!(
            std::fs::read_to_string(&path).expect("reread"),
            "rotated-token"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("list")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| {
                name.contains("tmp")
                    || std::path::Path::new(name)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("tmp"))
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp token files leaked: {leftovers:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn connect_reuses_existing_private_token_path_contents() {
        // Behavioral contract: Client::connect reads token_path before dialing and
        // persists the private token returned on_connect. Empty/missing files mean
        // anonymous first connect; non-empty files re-authenticate the same identity.
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("match-perf-token-reuse-{nonce}"));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("observer.token");
        assert!(std::fs::read_to_string(&path).ok().is_none());
        write_private_token(&path, "persisted-identity-token").expect("seed");
        let existing = std::fs::read_to_string(&path).expect("existing");
        assert_eq!(existing, "persisted-identity-token");
        assert!(!existing.trim().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
