//! Live-server connection plumbing shared by the playtest scenarios.
//!
//! This mirrors the proven `tools/match-e2e` client harness: one threaded
//! SDK connection per participant, lifecycle events over a channel, and
//! persistent anonymous identity tokens below an ignored directory.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use match_bindings::{
    CommandReceiptTableAccess, DbConnection, MapPreset, cancel_orders as _, configure_match as _,
    issue_attack_clusters as _, issue_expand_clusters as _, issue_front_rebalance as _,
    issue_reshape as _, join_match as _, set_mobilization_target as _, start_match as _,
};
use spacetimedb_sdk::{DbContext, Identity};

pub const fn receipt_key(player_id: u16, command_id: u64) -> u128 {
    (player_id as u128) << 64 | command_id as u128
}

pub enum LifecycleEvent {
    Connected { identity: Identity, token: String },
    Subscribed,
    Failed(String),
    Disconnected(Option<String>),
}

pub struct Client {
    pub label: &'static str,
    pub conn: DbConnection,
    events: Receiver<LifecycleEvent>,
    pump: Option<JoinHandle<()>>,
    stopped: bool,
}

impl Client {
    pub fn connect(
        label: &'static str,
        token_path: &Path,
        host: &str,
        database: &str,
        timeout: Duration,
    ) -> Result<Self> {
        let existing_token = read_token(token_path)?;
        let (event_tx, event_rx) = mpsc::channel();
        let connected_tx = event_tx.clone();
        let connect_error_tx = event_tx.clone();
        let disconnect_tx = event_tx;

        let conn = DbConnection::builder()
            .with_uri(host)
            .with_database_name(database)
            .with_token(existing_token)
            .on_connect(move |ctx, identity, token| {
                let _ = connected_tx.send(LifecycleEvent::Connected {
                    identity,
                    token: token.to_owned(),
                });
                let applied_tx = connected_tx.clone();
                let subscription_error_tx = connected_tx.clone();
                ctx.subscription_builder()
                    .on_applied(move |_| {
                        let _ = applied_tx.send(LifecycleEvent::Subscribed);
                    })
                    .on_error(move |_, error| {
                        let _ = subscription_error_tx.send(LifecycleEvent::Failed(format!(
                            "subscription failed: {error}"
                        )));
                    })
                    .subscribe_to_all_tables();
            })
            .on_connect_error(move |_, error| {
                let _ = connect_error_tx.send(LifecycleEvent::Failed(format!(
                    "connection establishment failed: {error}"
                )));
            })
            .on_disconnect(move |_, error| {
                let _ = disconnect_tx.send(LifecycleEvent::Disconnected(
                    error.map(|value| value.to_string()),
                ));
            })
            .build()
            .with_context(|| format!("build {label} connection to {host}/{database}"))?;
        let pump = conn.run_threaded();

        let deadline = Instant::now() + timeout;
        let mut connected = None;
        let mut subscribed = false;
        while connected.is_none() || !subscribed {
            match receive_before(
                &event_rx,
                deadline,
                &format!("{label} connection readiness"),
            )? {
                LifecycleEvent::Connected { identity, token } => {
                    connected = Some((identity, token));
                }
                LifecycleEvent::Subscribed => subscribed = true,
                LifecycleEvent::Failed(message) => bail!("{label}: {message}"),
                LifecycleEvent::Disconnected(error) => {
                    bail!(
                        "{label} disconnected before its subscription was ready: {}",
                        error.as_deref().unwrap_or("no server error")
                    );
                }
            }
        }

        let (_identity, token) = connected.context("connection callback omitted identity")?;
        write_token(token_path, &token).with_context(|| {
            format!("persist {label} identity token at {}", token_path.display())
        })?;
        Ok(Self {
            label,
            conn,
            events: event_rx,
            pump: Some(pump),
            stopped: false,
        })
    }

    pub fn configure_match(
        &self,
        preset: MapPreset,
        player_count: u16,
        timeout: Duration,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .configure_match_then(preset, player_count, move |_, result| {
                let _ = tx.send(flatten_reducer_result(result));
            })
            .with_context(|| format!("send configure_match for {}", self.label))?;
        wait_for_reducer(&rx, timeout, &format!("{} configure_match", self.label))
    }

    pub fn join_match(&self, player_id: u16, display_name: &str, timeout: Duration) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .join_match_then(player_id, display_name.to_owned(), move |_, result| {
                let _ = tx.send(flatten_reducer_result(result));
            })
            .with_context(|| format!("send join_match for {}", self.label))?;
        wait_for_reducer(&rx, timeout, &format!("{} join_match", self.label))
    }

    pub fn start_match(&self, timeout: Duration) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .start_match_then(move |_, result| {
                let _ = tx.send(flatten_reducer_result(result));
            })
            .with_context(|| format!("send start_match for {}", self.label))?;
        wait_for_reducer(&rx, timeout, &format!("{} start_match", self.label))
    }

    pub fn set_mobilization_target(
        &self,
        command_id: u64,
        target_bps: u32,
        timeout: Duration,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .set_mobilization_target_then(command_id, target_bps, move |_, result| {
                let _ = tx.send(flatten_reducer_result(result));
            })
            .context("send set_mobilization_target")?;
        wait_for_reducer(&rx, timeout, "set_mobilization_target")
    }

    pub fn issue_expand_clusters(
        &self,
        command_id: u64,
        source_seed_cells: &[u32],
        focus_cell_id: u32,
        commitment_bps: u32,
        timeout: Duration,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .issue_expand_clusters_then(
                command_id,
                source_seed_cells.to_vec(),
                focus_cell_id,
                commitment_bps,
                move |_, result| {
                    let _ = tx.send(flatten_reducer_result(result));
                },
            )
            .context("send issue_expand_clusters")?;
        wait_for_reducer(&rx, timeout, "issue_expand_clusters")
    }

    pub fn issue_attack_clusters(
        &self,
        command_id: u64,
        source_seed_cells: &[u32],
        target_seed_cells: &[u32],
        commitment_bps: u32,
        timeout: Duration,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .issue_attack_clusters_then(
                command_id,
                source_seed_cells.to_vec(),
                target_seed_cells.to_vec(),
                commitment_bps,
                move |_, result| {
                    let _ = tx.send(flatten_reducer_result(result));
                },
            )
            .context("send issue_attack_clusters")?;
        wait_for_reducer(&rx, timeout, "issue_attack_clusters")
    }

    pub fn issue_reshape(
        &self,
        command_id: u64,
        source_cells: &[u32],
        target_cells: &[u32],
        supersede_order_ids: &[u64],
        timeout: Duration,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .issue_reshape_then(
                command_id,
                source_cells.to_vec(),
                target_cells.to_vec(),
                supersede_order_ids.to_vec(),
                move |_, result| {
                    let _ = tx.send(flatten_reducer_result(result));
                },
            )
            .context("send issue_reshape")?;
        wait_for_reducer(&rx, timeout, "issue_reshape")
    }

    pub fn issue_front_rebalance(
        &self,
        command_id: u64,
        source_component_cells: &[u32],
        source_front_seed: u32,
        target_front_seed: u32,
        commitment_bps: u32,
        timeout: Duration,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .issue_front_rebalance_then(
                command_id,
                source_component_cells.to_vec(),
                source_front_seed,
                target_front_seed,
                commitment_bps,
                Vec::new(),
                move |_, result| {
                    let _ = tx.send(flatten_reducer_result(result));
                },
            )
            .context("send issue_front_rebalance")?;
        wait_for_reducer(&rx, timeout, "issue_front_rebalance")
    }

    pub fn cancel_orders(
        &self,
        command_id: u64,
        selected_order_ids: &[u64],
        timeout: Duration,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .cancel_orders_then(command_id, selected_order_ids.to_vec(), move |_, result| {
                let _ = tx.send(flatten_reducer_result(result));
            })
            .context("send cancel_orders")?;
        wait_for_reducer(&rx, timeout, "cancel_orders")
    }

    pub fn disconnect(&mut self, timeout: Duration) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.conn
            .disconnect()
            .with_context(|| format!("request {} disconnect", self.label))?;
        let deadline = Instant::now() + timeout;
        loop {
            match receive_before(
                &self.events,
                deadline,
                &format!("{} disconnect callback", self.label),
            )? {
                LifecycleEvent::Disconnected(error) => {
                    if let Some(error) = error {
                        bail!("{} disconnected with an error: {error}", self.label);
                    }
                    break;
                }
                LifecycleEvent::Failed(message) => bail!("{}: {message}", self.label),
                LifecycleEvent::Connected { .. } | LifecycleEvent::Subscribed => {}
            }
        }
        self.stopped = true;
        self.finish_pump(timeout);
        Ok(())
    }

    fn finish_pump(&mut self, timeout: Duration) {
        let Some(pump) = self.pump.take() else {
            return;
        };
        let deadline = Instant::now() + timeout;
        while !pump.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if pump.is_finished() {
            let _ = pump.join();
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = self.conn.disconnect();
        }
    }
}

fn flatten_reducer_result<E: std::fmt::Debug>(
    result: std::result::Result<std::result::Result<(), String>, E>,
) -> std::result::Result<(), String> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(message)) => Err(message),
        Err(error) => Err(format!("SDK reducer callback failed: {error:?}")),
    }
}

fn wait_for_reducer(
    receiver: &Receiver<std::result::Result<(), String>>,
    timeout: Duration,
    label: &str,
) -> Result<()> {
    match receiver.recv_timeout(timeout) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(message)) => bail!("{label} was rejected by the reducer: {message}"),
        Err(RecvTimeoutError::Timeout) => bail!("timed out after {timeout:?} waiting for {label}"),
        Err(RecvTimeoutError::Disconnected) => {
            bail!("callback channel closed while waiting for {label}")
        }
    }
}

fn receive_before(
    receiver: &Receiver<LifecycleEvent>,
    deadline: Instant,
    label: &str,
) -> Result<LifecycleEvent> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        bail!("timed out waiting for {label}");
    }
    match receiver.recv_timeout(remaining) {
        Ok(event) => Ok(event),
        Err(RecvTimeoutError::Timeout) => bail!("timed out waiting for {label}"),
        Err(RecvTimeoutError::Disconnected) => {
            bail!("lifecycle channel closed while waiting for {label}")
        }
    }
}

pub fn wait_until<T>(
    label: &str,
    timeout: Duration,
    poll: Duration,
    mut inspect: impl FnMut() -> Result<Option<T>>,
) -> Result<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = inspect().with_context(|| format!("while waiting for {label}"))? {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            bail!("timed out after {timeout:?} waiting for {label}");
        }
        thread::sleep(poll.min(deadline.saturating_duration_since(Instant::now())));
    }
}

pub fn unused_command_id(client: &Client, player_id: u16, start: u64) -> Result<u64> {
    let mut candidate = start;
    loop {
        let key = receipt_key(player_id, candidate);
        if client
            .conn
            .db
            .command_receipt()
            .receipt_key()
            .find(&key)
            .is_none()
        {
            return Ok(candidate);
        }
        candidate = candidate
            .checked_add(1)
            .context("exhausted client command ID range")?;
    }
}

fn read_token(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            let token = contents.trim();
            ensure!(!token.is_empty(), "token file {} is empty", path.display());
            Ok(Some(token.to_owned()))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read token file {}", path.display())),
    }
}

fn write_token(path: &Path, token: &str) -> Result<()> {
    ensure!(
        !token.trim().is_empty(),
        "server returned an empty identity token"
    );
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create token directory {}", parent.display()))?;
    }
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .with_context(|| format!("open token file {}", path.display()))?;
    file.write_all(token.as_bytes())
        .with_context(|| format!("write token file {}", path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("finish token file {}", path.display()))?;
    Ok(())
}
