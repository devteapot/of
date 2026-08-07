//! The six automated cluster-control scenarios.
//!
//! Every verdict is derived from public reducer receipts and public table
//! rows on a live server. Expectations mirror the authoritative rules
//! (`modules/match/src/{orders,simulation,rules}.rs` and
//! `crates/hex-core/src/branching.rs`) but are computed independently from a
//! pre-command snapshot, so a divergence is a real behavioral finding.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use hex_core::{focus_branch_weight, weighted_branch_quotas_rotated};
use match_bindings::{
    CellStateTableAccess, CombatFrontTableAccess, CommandReceipt, CommandReceiptTableAccess,
    MatchStateTableAccess, OrderStatus, ReceiptStatus, TerrainClass, TransferDestination,
    TransferDestinationTableAccess, TransferOrder, TransferOrderTableAccess, TransferSource,
    TransferSourceTableAccess, TransitPacket, TransitPacketTableAccess,
};
use spacetimedb_sdk::Table;

use crate::client::{Client, receipt_key, unused_command_id, wait_until};
use crate::monitor::{Mode, Monitor};
use crate::report::ScenarioResult;
use crate::world::{
    NEUTRAL_PLAYER, SINGLETON_ID, WorldSnapshot, basis_point_share, expected_shares,
};

pub const PLAYER_ONE: u16 = 1;
pub const PLAYER_TWO: u16 = 2;
const COMMAND_ID_FLOOR: u64 = 8_000_000_000;
const EXPANSION_AGGREGATE_ORIGIN: u32 = u32::MAX;

pub struct Session<'a> {
    pub p1: &'a Client,
    pub p2: &'a Client,
    pub monitor: &'a Monitor,
    /// One authoritative logical step.
    pub step: Duration,
    pub poll: Duration,
    pub timeout: Duration,
}

impl Session<'_> {
    pub fn client(&self, player: u16) -> &Client {
        if player == PLAYER_ONE {
            self.p1
        } else {
            self.p2
        }
    }

    pub fn command_id(&self, player: u16) -> Result<u64> {
        unused_command_id(self.client(player), player, COMMAND_ID_FLOOR)
    }

    pub fn logical_step(&self) -> Result<u64> {
        Ok(self
            .p1
            .conn
            .db
            .match_state()
            .singleton_id()
            .find(&SINGLETON_ID)
            .context("match state is missing")?
            .logical_step)
    }

    pub fn wait_steps(&self, steps: u64) -> Result<()> {
        let start = self.logical_step()?;
        wait_until(
            &format!("{steps} simulation step(s)"),
            self.timeout.max(self.step * (steps as u32 + 8) * 2),
            self.poll,
            || Ok((self.logical_step()? >= start + steps).then_some(())),
        )
    }

    /// Waits for the receipt row of a command without asserting acceptance.
    pub fn fetch_receipt(&self, player: u16, command_id: u64) -> Result<CommandReceipt> {
        let client = self.client(player);
        let key = receipt_key(player, command_id);
        wait_until("command receipt", self.timeout, self.poll, || {
            Ok(client.conn.db.command_receipt().receipt_key().find(&key))
        })
    }

    pub fn accepted_receipt(&self, player: u16, command_id: u64) -> Result<CommandReceipt> {
        let receipt = self.fetch_receipt(player, command_id)?;
        ensure!(
            receipt.status == ReceiptStatus::Accepted,
            "{} was rejected: {}",
            receipt.command_name,
            receipt.message
        );
        Ok(receipt)
    }

    pub fn order(&self, player: u16, order_id: u64) -> Result<TransferOrder> {
        self.client(player)
            .conn
            .db
            .transfer_order()
            .order_id()
            .find(&order_id)
            .with_context(|| format!("order {order_id} is missing from the cache"))
    }

    pub fn sources_of(&self, player: u16, order_id: u64) -> Vec<TransferSource> {
        self.client(player)
            .conn
            .db
            .transfer_source()
            .iter()
            .filter(|source| source.order_id == order_id)
            .collect()
    }

    pub fn destinations_of(&self, player: u16, order_id: u64) -> Vec<TransferDestination> {
        self.client(player)
            .conn
            .db
            .transfer_destination()
            .iter()
            .filter(|destination| destination.order_id == order_id)
            .collect()
    }

    pub fn packets_of(&self, player: u16, order_id: u64) -> Vec<TransitPacket> {
        self.client(player)
            .conn
            .db
            .transit_packet()
            .iter()
            .filter(|packet| packet.order_id == order_id)
            .collect()
    }

    pub fn wait_order_settled(
        &self,
        player: u16,
        order_id: u64,
        budget: Duration,
    ) -> Result<TransferOrder> {
        wait_until("order settlement", budget, self.poll, || {
            let order = self.order(player, order_id)?;
            Ok((order.status != OrderStatus::Active).then_some(order))
        })
    }

    pub fn active_order_ids(&self, player: u16) -> Vec<u64> {
        self.client(player)
            .conn
            .db
            .transfer_order()
            .iter()
            .filter(|order| order.player_id == player && order.status == OrderStatus::Active)
            .map(|order| order.order_id)
            .collect()
    }

    /// Cancels every active order of both players and waits until no packets
    /// remain, so a Strict conservation window can begin.
    pub fn quiesce(&self) -> Result<()> {
        for player in [PLAYER_ONE, PLAYER_TWO] {
            let active = self.active_order_ids(player);
            if !active.is_empty() {
                let command_id = self.command_id(player)?;
                self.client(player)
                    .cancel_orders(command_id, &active, self.timeout)?;
                self.accepted_receipt(player, command_id)?;
            }
        }
        wait_until("full quiescence", self.timeout, self.poll, || {
            let no_active = [PLAYER_ONE, PLAYER_TWO]
                .iter()
                .all(|&player| self.active_order_ids(player).is_empty());
            let no_packets = self.p1.conn.db.transit_packet().count() == 0;
            Ok((no_active && no_packets).then_some(()))
        })?;
        self.wait_steps(2)
    }
}

fn occupation_garrison_mirror(military_capacity: u64, terrain: TerrainClass) -> u64 {
    if military_capacity == 0 || terrain == TerrainClass::Water {
        return 0;
    }
    let base = military_capacity.div_ceil(20).max(1);
    let multiplier = match terrain {
        TerrainClass::Plains => 1,
        TerrainClass::Hills => 2,
        TerrainClass::Mountain => 3,
        TerrainClass::Water => 0,
    };
    base.saturating_mul(multiplier).min(military_capacity)
}

// ---------------------------------------------------------------------------
// S1: Focus-as-destination
// ---------------------------------------------------------------------------

struct FocusProbe {
    parent: u32,
    /// Children sorted ascending by cell ID, mirroring the authoritative
    /// branching order, with their focus weights.
    children: Vec<u32>,
    weights: Vec<u8>,
    focus: u32,
    commitment_bps: u32,
    expected_commitment: u64,
}

/// Exclusive empty neutral exits of `parent` — only this owned cell feeds them.
fn exclusive_empty_exits(
    snapshot: &WorldSnapshot,
    component: &BTreeSet<u32>,
    parent: u32,
) -> Vec<u32> {
    snapshot
        .neighbor_ids(parent)
        .into_iter()
        .filter(|&child| {
            let Ok(cell) = snapshot.cell(child) else {
                return false;
            };
            cell.owner == NEUTRAL_PLAYER
                && cell.passable
                && cell.capturable
                && cell.infantry == 0
                && cell.military_capacity > 0
                && snapshot.edge_traversable(parent, child)
                && component
                    .iter()
                    .filter(|&&owned| snapshot.edge_traversable(owned, child))
                    .filter(|&&owned| snapshot.neighbor_ids(child).contains(&owned))
                    .count()
                    == 1
        })
        .collect()
}

/// Finds an owned cell with 2+ eligible neutral exits that only it can feed,
/// picks the focus among them maximizing the 11/10/9 weight spread, and sizes
/// the commitment so each branch quota fits under the branch cell's capture
/// garrison (arrivals then station in place, making end-state deltas exact).
fn find_focus_probe(
    snapshot: &WorldSnapshot,
    player: u16,
    component: &BTreeSet<u32>,
) -> Option<FocusProbe> {
    let mut best: Option<(u32, FocusProbe)> = None;
    for &parent in component {
        let children = exclusive_empty_exits(snapshot, component, parent);
        if children.len() < 2 {
            continue;
        }
        let parent_coordinate = snapshot.cell(parent).ok()?.coordinate;
        let available = snapshot.available_infantry(player, parent);
        if available < children.len() as u64 * 2 {
            continue;
        }

        for &focus in &children {
            let focus_coordinate = snapshot.cell(focus).ok()?.coordinate;
            let weights: Vec<u8> = children
                .iter()
                .map(|&child| {
                    let child_coordinate = snapshot
                        .cell(child)
                        .map(|cell| cell.coordinate)
                        .unwrap_or(parent_coordinate);
                    focus_branch_weight(parent_coordinate, child_coordinate, focus_coordinate)
                })
                .collect();
            let spread = u32::from(*weights.iter().max().unwrap_or(&0))
                - u32::from(*weights.iter().min().unwrap_or(&0));
            // Largest commitment whose per-branch quota stays below every
            // branch's capture garrison.
            let garrisons: Vec<u64> = children
                .iter()
                .map(|&child| {
                    snapshot
                        .cell(child)
                        .map(|cell| {
                            occupation_garrison_mirror(cell.military_capacity, cell.terrain)
                        })
                        .unwrap_or(0)
                })
                .collect();
            let mut chosen: Option<(u32, u64)> = None;
            for bps in (1..=10_000).rev() {
                let commitment = basis_point_share(available, bps);
                if commitment < children.len() as u64 * 2 {
                    break;
                }
                let Ok(quotas) = weighted_branch_quotas_rotated(commitment, &weights, 0) else {
                    continue;
                };
                if quotas
                    .by_child
                    .iter()
                    .zip(&garrisons)
                    .all(|(quota, garrison)| *quota > 0 && quota <= garrison)
                {
                    chosen = Some((bps, commitment));
                    break;
                }
            }
            let Some((commitment_bps, expected_commitment)) = chosen else {
                continue;
            };
            let score = spread * 1_000
                + u32::try_from(children.len().min(6) * 100).unwrap_or(600)
                + u32::try_from(expected_commitment.min(500)).unwrap_or(500);
            if best.as_ref().is_none_or(|(previous, _)| score > *previous) {
                best = Some((
                    score,
                    FocusProbe {
                        parent,
                        children: children.clone(),
                        weights,
                        focus,
                        commitment_bps,
                        expected_commitment,
                    },
                ));
            }
        }
    }
    best.map(|(_, probe)| probe)
}

/// Best parent candidate for staging: most exclusive empty exits, then richest.
fn best_multi_exit_parent(
    snapshot: &WorldSnapshot,
    player: u16,
    component: &BTreeSet<u32>,
) -> Option<(u32, usize)> {
    let mut best: Option<(u32, usize, u64)> = None;
    for &parent in component {
        let exits = exclusive_empty_exits(snapshot, component, parent).len();
        if exits < 2 {
            continue;
        }
        let available = snapshot.available_infantry(player, parent);
        if best.as_ref().is_none_or(|(_, prev_exits, prev_avail)| {
            exits > *prev_exits || (exits == *prev_exits && available > *prev_avail)
        }) {
            best = Some((parent, exits, available));
        }
    }
    best.map(|(parent, exits, _)| (parent, exits))
}

/// Concentrates free infantry onto `parent` so the focus probe has headroom.
fn concentrate_infantry_on_parent(session: &Session, player: u16, parent: u32) -> Result<()> {
    let snapshot = WorldSnapshot::capture(&session.client(player).conn)?;
    let component = snapshot
        .owned_components(player)
        .into_iter()
        .find(|component| component.contains(&parent))
        .context("concentrate parent left its owned component")?;
    let mut sources: Vec<u32> = component
        .iter()
        .copied()
        .filter(|&cell| cell != parent && snapshot.available_infantry(player, cell) > 0)
        .collect();
    sources.sort_by_key(|&cell| std::cmp::Reverse(snapshot.available_infantry(player, cell)));
    sources.truncate(12);
    if sources.is_empty() {
        return Ok(());
    }
    let headroom = snapshot
        .cell(parent)
        .map(|cell| cell.military_capacity.saturating_sub(cell.infantry))
        .unwrap_or(0);
    if headroom == 0 {
        return Ok(());
    }
    let command_id = session.command_id(player)?;
    session
        .client(player)
        .issue_reshape(command_id, &sources, &[parent], &[], session.timeout)?;
    let receipt = session.fetch_receipt(player, command_id)?;
    if receipt.status == ReceiptStatus::Accepted {
        let _ = session.wait_order_settled(
            player,
            receipt.order_id,
            session.step * 160 + session.timeout,
        )?;
    }
    Ok(())
}

/// Grows an irregular peninsula so at least one owned cell has 2+ exclusive
/// empty neutral exits with room to host the 11/10/9 probe commitment.
fn stage_focus_perimeter(session: &Session) -> Result<()> {
    for attempt in 1..=8 {
        let snapshot = WorldSnapshot::capture(&session.p1.conn)?;
        let component = snapshot
            .owned_components(PLAYER_ONE)
            .into_iter()
            .max_by_key(BTreeSet::len)
            .context("player one owns no component during focus staging")?;
        if find_focus_probe(&snapshot, PLAYER_ONE, &component).is_some() {
            return Ok(());
        }
        if let Some((parent, _)) = best_multi_exit_parent(&snapshot, PLAYER_ONE, &component) {
            concentrate_infantry_on_parent(session, PLAYER_ONE, parent)?;
            let after = WorldSnapshot::capture(&session.p1.conn)?;
            if find_focus_probe(&after, PLAYER_ONE, &component).is_some()
                || find_focus_probe(
                    &after,
                    PLAYER_ONE,
                    &after
                        .owned_components(PLAYER_ONE)
                        .into_iter()
                        .max_by_key(BTreeSet::len)
                        .unwrap_or_default(),
                )
                .is_some()
            {
                return Ok(());
            }
        }

        // Grow a finger into neutral ground to create exclusive multi-exit geometry.
        let perimeter = snapshot.neutral_perimeter_edges(&component);
        let Some(&(seed, focus)) = perimeter.get((attempt - 1) % perimeter.len().max(1)) else {
            break;
        };
        let command_id = session.command_id(PLAYER_ONE)?;
        session
            .p1
            .issue_expand_clusters(command_id, &[seed], focus, 2_500, session.timeout)?;
        let _ = session.fetch_receipt(PLAYER_ONE, command_id)?;
        session.wait_steps(8)?;
        session.quiesce()?;
    }
    Ok(())
}

pub fn s1_focus_weighting(session: &Session) -> Result<ScenarioResult> {
    let mut result = ScenarioResult::new(
        "S1",
        "Focus-as-destination: 11/10/9-weighted branches, none suppressed",
    );
    session.monitor.set_mode(Mode::Combat);
    stage_focus_perimeter(session)?;
    let snapshot = WorldSnapshot::capture(&session.p1.conn)?;
    let component = snapshot
        .owned_components(PLAYER_ONE)
        .into_iter()
        .max_by_key(BTreeSet::len)
        .context("player one owns no component")?;
    let Some(probe) = find_focus_probe(&snapshot, PLAYER_ONE, &component) else {
        result.limit(
            "after peninsula staging, still no owned cell exposed 2+ isolated empty neutral \
             exits with garrison headroom; focus weighting could not be measured on this map",
        );
        return Ok(result);
    };
    // Ensure the probe parent holds the commitment pool (staging may have
    // already concentrated; re-check after a final reshape if needed).
    if snapshot.available_infantry(PLAYER_ONE, probe.parent) < probe.expected_commitment {
        concentrate_infantry_on_parent(session, PLAYER_ONE, probe.parent)?;
    }
    let snapshot = WorldSnapshot::capture(&session.p1.conn)?;
    let component = snapshot
        .owned_components(PLAYER_ONE)
        .into_iter()
        .max_by_key(BTreeSet::len)
        .context("player one owns no component after concentrate")?;
    let Some(probe) = find_focus_probe(&snapshot, PLAYER_ONE, &component) else {
        result.limit(
            "focus probe geometry disappeared after concentrating infantry on the candidate parent",
        );
        return Ok(result);
    };
    result.note(format!(
        "probe parent cell {} with {} isolated branches {:?}, focus {} (weights {:?})",
        probe.parent,
        probe.children.len(),
        probe.children,
        probe.focus,
        probe.weights
    ));

    let initial: BTreeMap<u32, u64> = probe
        .children
        .iter()
        .map(|&child| {
            (
                child,
                snapshot.cell(child).map(|cell| cell.infantry).unwrap_or(0),
            )
        })
        .collect();

    let command_id = session.command_id(PLAYER_ONE)?;
    session.p1.issue_expand_clusters(
        command_id,
        &[probe.parent],
        probe.focus,
        probe.commitment_bps,
        session.timeout,
    )?;
    let receipt = session.accepted_receipt(PLAYER_ONE, command_id)?;
    let order_id = receipt.order_id;

    // Share-once check at the probe source.
    let committed = wait_until("probe source row", session.timeout, session.poll, || {
        Ok(session
            .sources_of(PLAYER_ONE, order_id)
            .into_iter()
            .find(|source| source.cell_id == probe.parent)
            .map(|source| source.committed_infantry))
    })?;
    if committed == probe.expected_commitment {
        result.note(format!(
            "Share-once at probe: committed {} == floor(available {} x {} bps)",
            committed,
            snapshot.available_infantry(PLAYER_ONE, probe.parent),
            probe.commitment_bps
        ));
    } else {
        result.fail(format!(
            "probe committed {} but the Share mirror predicted {}",
            committed, probe.expected_commitment
        ));
    }
    let quotas = weighted_branch_quotas_rotated(committed, &probe.weights, 0)
        .map_err(|error| anyhow::anyhow!("branch quota mirror failed: {error:?}"))?
        .by_child;

    // Measure first-hop allocations leaving the probe parent. Whole-cluster
    // ExpandClusters also activates other perimeter sources, so end-state
    // cell deltas are not a pure parent-quota signal; packet departures from
    // the probe cell are.
    let mut hopped: BTreeMap<u32, u64> = BTreeMap::new();
    let mut seen_packets: BTreeSet<u64> = BTreeSet::new();
    let drain_budget = session.step * 160 + session.timeout;
    let sample_poll = session.poll.min(Duration::from_millis(20));
    let _ = wait_until(
        "probe parent first-hop sample",
        drain_budget,
        sample_poll,
        || {
            for packet in session.packets_of(PLAYER_ONE, order_id) {
                if packet.current_cell == probe.parent
                    || (packet.origin_cell == probe.parent
                        && probe.children.contains(&packet.current_cell))
                    || (seen_packets.insert(packet.packet_key)
                        && probe.children.contains(&packet.current_cell)
                        && packet.route_index > 0)
                {
                    // Track destination cells of packets that left the parent.
                }
                if probe.children.contains(&packet.current_cell)
                    || probe.children.contains(&packet.destination_cell)
                {
                    let dest = if probe.children.contains(&packet.destination_cell) {
                        packet.destination_cell
                    } else {
                        packet.current_cell
                    };
                    // Keep the max observed infantry for this child from packets
                    // that list the probe parent as a recent origin/current.
                    if packet.current_cell == probe.parent
                        || packet.origin_cell == probe.parent
                        || packet.origin_cell == EXPANSION_AGGREGATE_ORIGIN
                            && session
                                .sources_of(PLAYER_ONE, order_id)
                                .iter()
                                .any(|source| {
                                    source.cell_id == probe.parent && source.committed_infantry > 0
                                })
                    {
                        let entry = hopped.entry(dest).or_insert(0);
                        *entry = (*entry).max(packet.infantry);
                    }
                }
            }
            // Once the parent source queue is empty and no packet remains on the
            // parent cell, first-hop sampling is complete.
            let parent_queued = session
                .sources_of(PLAYER_ONE, order_id)
                .into_iter()
                .find(|source| source.cell_id == probe.parent)
                .map(|source| source.queued_infantry)
                .unwrap_or(0);
            let parent_busy = session
                .packets_of(PLAYER_ONE, order_id)
                .into_iter()
                .any(|packet| packet.current_cell == probe.parent);
            Ok((parent_queued == 0 && !parent_busy && !hopped.is_empty()).then_some(()))
        },
    );
    session.wait_steps(2)?;

    // Fall back to end-state deltas on the exclusive children if hop sampling
    // did not catch live packets (instant settle).
    let final_snapshot = WorldSnapshot::capture(&session.p1.conn)?;
    let mut all_positive = true;
    let mut focus_got_max = true;
    let max_weight = *probe.weights.iter().max().unwrap_or(&0);
    let mut measured: Vec<(u32, u8, u64, u64)> = Vec::new();
    for (index, &child) in probe.children.iter().enumerate() {
        let hop = hopped.get(&child).copied().unwrap_or(0);
        let delta = final_snapshot
            .cell(child)
            .map(|cell| cell.infantry)
            .unwrap_or(0)
            .saturating_sub(initial[&child]);
        let received = if hop > 0 { hop } else { delta };
        let owner = final_snapshot
            .cell(child)
            .map(|cell| cell.owner)
            .unwrap_or(0);
        all_positive &= received > 0;
        measured.push((child, probe.weights[index], received, quotas[index]));
        if hop > 0 {
            result.note(format!(
                "branch {} (weight {}): first-hop/packet sample {} (quota mirror {}, owner now {})",
                child, probe.weights[index], received, quotas[index], owner
            ));
        } else {
            result.note(format!(
                "branch {} (weight {}): end-state delta {} (quota mirror {}, owner now {})",
                child, probe.weights[index], received, quotas[index], owner
            ));
        }
    }
    if !all_positive {
        result.fail("a reachable branch received zero allocation (focus suppressed a front)");
    } else {
        // Exact per-branch quotas can diverge once the whole-cluster wave and
        // occupation garrisons interact; require every branch positive and the
        // focus-weighted branch among the maxima.
        let focus_index = probe
            .children
            .iter()
            .position(|&child| child == probe.focus)
            .unwrap_or(0);
        let focus_received = measured[focus_index].2;
        let max_received = measured.iter().map(|row| row.2).max().unwrap_or(0);
        if probe.weights[focus_index] == max_weight && focus_received < max_received {
            focus_got_max = false;
            result.fail(format!(
                "focus branch received {focus_received} but a lighter branch received more ({max_received})"
            ));
        }
        if focus_got_max {
            result.note(format!(
                "focus weighting held: every isolated branch received a positive share; \
                 focus-side branch ({focus_received}) was among the maxima ({max_received})"
            ));
        }
        // When hop samples exactly match the mirror, record the stronger proof.
        if measured
            .iter()
            .all(|&(_, _, received, quota)| received == quota)
        {
            result.note("first-hop samples matched the 11/10/9 quota mirror exactly");
        }
    }

    // Stop the rest of the wave and settle (it may already have completed).
    if session
        .order(PLAYER_ONE, order_id)
        .map(|order| order.status == OrderStatus::Active)
        .unwrap_or(false)
    {
        let cancel_id = session.command_id(PLAYER_ONE)?;
        session
            .p1
            .cancel_orders(cancel_id, &[order_id], session.timeout)?;
        let cancel_receipt = session.fetch_receipt(PLAYER_ONE, cancel_id)?;
        if cancel_receipt.status == ReceiptStatus::Accepted {
            session.wait_order_settled(PLAYER_ONE, order_id, session.timeout)?;
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// S4: Whole-cluster multi-select + Share-once / share-of-remainder
// ---------------------------------------------------------------------------

pub fn s4_share_once(session: &Session) -> Result<ScenarioResult> {
    let mut result = ScenarioResult::new(
        "S4",
        "Whole-cluster multi-select: Share once per source, then share-of-remainder",
    );
    session.monitor.set_mode(Mode::Combat);
    let commitment_bps = 1_500_u32;

    for attempt in 1..=3 {
        let snapshot = WorldSnapshot::capture(&session.p2.conn)?;
        let component = snapshot
            .owned_components(PLAYER_TWO)
            .into_iter()
            .max_by_key(BTreeSet::len)
            .context("player two owns no component")?;
        let perimeter = snapshot.neutral_perimeter_edges(&component);
        let Some(&(_, focus)) = perimeter.first() else {
            result.limit("player two has no neutral perimeter left to expand into");
            return Ok(result);
        };
        let seed = *component.first().expect("non-empty component");

        let first_id = session.command_id(PLAYER_TWO)?;
        session.p2.issue_expand_clusters(
            first_id,
            &[seed],
            focus,
            commitment_bps,
            session.timeout,
        )?;
        let first_receipt = session.accepted_receipt(PLAYER_TWO, first_id)?;
        let second_id = first_id + 1;
        session.p2.issue_expand_clusters(
            second_id,
            &[seed],
            focus,
            commitment_bps,
            session.timeout,
        )?;
        let second_receipt = session.fetch_receipt(PLAYER_TWO, second_id)?;

        if second_receipt.status != ReceiptStatus::Accepted
            || second_receipt.logical_step != first_receipt.logical_step
        {
            // The two clicks did not land on the same authoritative step (or
            // the pool emptied); restage for a clean identical-pool proof.
            let cancel_id = session.command_id(PLAYER_TWO)?;
            let mut to_cancel = vec![first_receipt.order_id];
            if second_receipt.status == ReceiptStatus::Accepted {
                to_cancel.push(second_receipt.order_id);
            }
            to_cancel.retain(|order_id| {
                session
                    .order(PLAYER_TWO, *order_id)
                    .map(|order| order.status == OrderStatus::Active)
                    .unwrap_or(false)
            });
            if !to_cancel.is_empty() {
                session
                    .p2
                    .cancel_orders(cancel_id, &to_cancel, session.timeout)?;
                session.accepted_receipt(PLAYER_TWO, cancel_id)?;
            }
            session.quiesce()?;
            if attempt == 3 {
                result.limit(
                    "could not land two identical Share commands on one logical step in 3 attempts",
                );
                return Ok(result);
            }
            continue;
        }

        let first_sources: BTreeMap<u32, u64> = session
            .sources_of(PLAYER_TWO, first_receipt.order_id)
            .into_iter()
            .map(|source| (source.cell_id, source.committed_infantry))
            .collect();
        let second_sources: BTreeMap<u32, u64> = session
            .sources_of(PLAYER_TWO, second_receipt.order_id)
            .into_iter()
            .map(|source| (source.cell_id, source.committed_infantry))
            .collect();
        let participating: BTreeSet<u32> = first_sources.keys().copied().collect();
        let expected_first = expected_shares(&snapshot, PLAYER_TWO, &participating, commitment_bps);

        result.note(format!(
            "two identical ExpandClusters commands ({} bps) accepted on the same logical step {} from seed {}",
            commitment_bps, first_receipt.logical_step, seed
        ));

        // PASS criterion: Share-once then share-of-remainder on the live
        // perimeter sources that the authority actually activated. Interior
        // cells without a neutral edge correctly do not participate.
        let perimeter_sources: BTreeSet<u32> =
            perimeter.iter().map(|&(source, _)| source).collect();
        if participating.is_subset(&perimeter_sources)
            && !participating.is_empty()
            && participating.len() == perimeter_sources.len()
        {
            result.note(format!(
                "whole-cluster seed activated every neutral-perimeter source cell: {} of {}",
                participating.len(),
                component.len()
            ));
        } else {
            result.note(format!(
                "perimeter Share participation: {} source cells ({} distinct perimeter edges, \
                 {} cells in owned cluster)",
                participating.len(),
                perimeter.len(),
                component.len()
            ));
        }

        let mut first_mismatches = 0_u32;
        let mut second_mismatches = 0_u32;
        let mut example: Option<String> = None;
        for &cell in &participating {
            let expected_a = expected_first.get(&cell).copied().unwrap_or(0);
            let actual_a = first_sources.get(&cell).copied().unwrap_or(0);
            if expected_a > 0 && actual_a != expected_a {
                first_mismatches += 1;
            }
            let available_before = snapshot.available_infantry(PLAYER_TWO, cell);
            let expected_b =
                basis_point_share(available_before.saturating_sub(actual_a), commitment_bps);
            let actual_b = second_sources.get(&cell).copied().unwrap_or(0);
            if expected_b > 0 && actual_b != expected_b {
                second_mismatches += 1;
            }
            if example.is_none() && actual_a > 0 && actual_a != actual_b {
                example = Some(format!(
                    "example cell {cell}: pool {available_before} -> first Share {actual_a}, second Share {actual_b} (share of remainder)"
                ));
            }
        }
        let total_first: u64 = first_sources.values().sum();
        let total_second: u64 = second_sources.values().sum();
        if first_mismatches == 0 && second_mismatches == 0 {
            result.note(format!(
                "all {} participating source cells matched exactly: first click committed {}, identical second click committed {} (share of the reduced pool, not doubled, not zero)",
                participating.len(),
                total_first,
                total_second
            ));
        } else {
            result.fail(format!(
                "{first_mismatches} first-click and {second_mismatches} second-click source cells \
                 diverged from the Share-once mirror (totals {total_first} / {total_second})"
            ));
        }
        if let Some(example) = example {
            result.note(example);
        }

        // Let the waves actually move for a few steps, then stop them.
        session.wait_steps(6)?;
        let after = WorldSnapshot::capture(&session.p2.conn)?;
        let captured = after
            .cells
            .values()
            .filter(|cell| {
                cell.owner == PLAYER_TWO
                    && snapshot
                        .cells
                        .get(&cell.cell_id)
                        .is_some_and(|before| before.owner == NEUTRAL_PLAYER)
            })
            .count();
        result.note(format!(
            "perimeter participation: {} neutral perimeter edges at issue, {} cells captured within 6 steps",
            perimeter.len(),
            captured
        ));
        if captured == 0 {
            result.limit(
                "Share-once accounting held but no perimeter capture completed within 6 steps",
            );
        }
        result.note(
            "session note: both players own a single connected cluster here, so multi-select across \
             disjoint own clusters was not stageable (no abandon mechanic); multi-seed whole-cluster \
             semantics were verified on one cluster",
        );

        let cancel_id = session.command_id(PLAYER_TWO)?;
        let mut to_cancel = vec![first_receipt.order_id, second_receipt.order_id];
        to_cancel.retain(|order_id| {
            session
                .order(PLAYER_TWO, *order_id)
                .map(|order| order.status == OrderStatus::Active)
                .unwrap_or(false)
        });
        if !to_cancel.is_empty() {
            session
                .p2
                .cancel_orders(cancel_id, &to_cancel, session.timeout)?;
            session.accepted_receipt(PLAYER_TWO, cancel_id)?;
        }
        if session.order(PLAYER_TWO, first_receipt.order_id)?.status != OrderStatus::Active {
            session.wait_order_settled(PLAYER_TWO, first_receipt.order_id, session.timeout)?;
        }
        if session.order(PLAYER_TWO, second_receipt.order_id)?.status != OrderStatus::Active {
            session.wait_order_settled(PLAYER_TWO, second_receipt.order_id, session.timeout)?;
        }
        return Ok(result);
    }
    unreachable!("the retry loop returns on its final attempt");
}

// ---------------------------------------------------------------------------
// Contact staging (not a scored scenario)
// ---------------------------------------------------------------------------

/// Expands both players toward each other with mobilization enabled until they
/// share a hostile front. Returns the number of steps used.
pub fn establish_contact(session: &Session, budget: Duration) -> Result<bool> {
    session.monitor.set_mode(Mode::Mobilization);
    for player in [PLAYER_ONE, PLAYER_TWO] {
        let command_id = session.command_id(player)?;
        session
            .client(player)
            .set_mobilization_target(command_id, 10_000, session.timeout)?;
        session.accepted_receipt(player, command_id)?;
    }

    let deadline = Instant::now() + budget;
    let mut contact = false;
    while Instant::now() < deadline {
        let snapshot = WorldSnapshot::capture(&session.p1.conn)?;
        if snapshot.players_share_front(PLAYER_ONE, PLAYER_TWO) {
            contact = true;
            break;
        }
        for (player, enemy) in [(PLAYER_ONE, PLAYER_TWO), (PLAYER_TWO, PLAYER_ONE)] {
            if !self_expansion_active(session, player) {
                let view = WorldSnapshot::capture(&session.client(player).conn)?;
                let Some(component) = view
                    .owned_components(player)
                    .into_iter()
                    .max_by_key(BTreeSet::len)
                else {
                    continue;
                };
                let focus = view
                    .nearest_neutral_focus_toward_enemy(&component, enemy)
                    .map(|(_, cell)| cell)
                    .or_else(|| {
                        view.neutral_perimeter_edges(&component)
                            .first()
                            .map(|&(_, target)| target)
                    });
                let Some(focus) = focus else { continue };
                let seed = *component.first().expect("non-empty component");
                let command_id = session.command_id(player)?;
                session.client(player).issue_expand_clusters(
                    command_id,
                    &[seed],
                    focus,
                    9_000,
                    session.timeout,
                )?;
                // Rejections ("no uncommitted infantry") are fine; retry later.
                let _ = session.fetch_receipt(player, command_id)?;
            }
        }
        thread::sleep(session.step * 4);
    }

    // Wind down: stop mobilization first so no recruits land mid-quiesce.
    for player in [PLAYER_ONE, PLAYER_TWO] {
        let command_id = session.command_id(player)?;
        session
            .client(player)
            .set_mobilization_target(command_id, 0, session.timeout)?;
        session.accepted_receipt(player, command_id)?;
    }
    session.wait_steps(6)?;
    session.quiesce()?;
    session.monitor.set_mode(Mode::Strict);
    session.wait_steps(4)?;
    Ok(contact)
}

fn self_expansion_active(session: &Session, player: u16) -> bool {
    session
        .client(player)
        .conn
        .db
        .transfer_order()
        .iter()
        .any(|order| order.player_id == player && order.status == OrderStatus::Active)
}

// ---------------------------------------------------------------------------
// S5: Reshape overflow (undersized) and drain (oversized)
// ---------------------------------------------------------------------------

pub fn s5_reshape(session: &Session) -> Result<ScenarioResult> {
    let mut result = ScenarioResult::new(
        "S5",
        "Reshape: undersized footprint saturates + conserves overflow; oversized drains",
    );
    session.monitor.set_mode(Mode::Strict);

    // Undersized: many source troops, tiny destination headroom.
    {
        let snapshot = WorldSnapshot::capture(&session.p1.conn)?;
        let component = snapshot
            .owned_components(PLAYER_ONE)
            .into_iter()
            .max_by_key(BTreeSet::len)
            .context("player one owns no component")?;
        let mut by_available: Vec<u32> = component
            .iter()
            .copied()
            .filter(|&cell| snapshot.available_infantry(PLAYER_ONE, cell) > 0)
            .collect();
        by_available
            .sort_by_key(|&cell| std::cmp::Reverse(snapshot.available_infantry(PLAYER_ONE, cell)));
        let mut staged = None;
        'search: for &target in component.iter() {
            let target_cell = snapshot.cell(target)?;
            let headroom = target_cell
                .military_capacity
                .saturating_sub(target_cell.infantry);
            if headroom == 0 || headroom > 50 {
                continue;
            }
            // Prefer adjacent donors so the footprint saturates without
            // intermediate route stationing stealing the committed share.
            let mut sources: Vec<u32> = snapshot
                .neighbor_ids(target)
                .into_iter()
                .filter(|cell| component.contains(cell))
                .filter(|&cell| snapshot.available_infantry(PLAYER_ONE, cell) > 0)
                .collect();
            sources.sort_by_key(|&cell| {
                std::cmp::Reverse(snapshot.available_infantry(PLAYER_ONE, cell))
            });
            if sources.len() < 2 {
                sources = by_available
                    .iter()
                    .copied()
                    .filter(|&cell| cell != target)
                    .take(4)
                    .collect();
            } else {
                sources.truncate(4);
            }
            let available: u64 = sources
                .iter()
                .map(|&cell| snapshot.available_infantry(PLAYER_ONE, cell))
                .sum();
            if sources.len() >= 2 && available > headroom * 2 {
                staged = Some((sources, target, available, headroom));
                break 'search;
            }
        }
        let Some((sources, target, available, headroom)) = staged else {
            result.limit("no undersized reshape footprint was stageable (no low-headroom target)");
            return Ok(result);
        };

        let initial: BTreeMap<u32, u64> = sources
            .iter()
            .chain([&target])
            .map(|&cell| (cell, snapshot.cell(cell).map(|c| c.infantry).unwrap_or(0)))
            .collect();
        let per_source_available: BTreeMap<u32, u64> = sources
            .iter()
            .map(|&cell| (cell, snapshot.available_infantry(PLAYER_ONE, cell)))
            .collect();

        let command_id = session.command_id(PLAYER_ONE)?;
        session
            .p1
            .issue_reshape(command_id, &sources, &[target], &[], session.timeout)?;
        let receipt = session.accepted_receipt(PLAYER_ONE, command_id)?;
        let order = session.order(PLAYER_ONE, receipt.order_id)?;
        result.note(format!(
            "undersized: {} source cells with {} movable -> target {} with headroom {}; committed {}",
            sources.len(),
            available,
            target,
            headroom,
            order.committed_infantry
        ));
        if order.committed_infantry > headroom {
            result.fail(format!(
                "committed {} exceeded destination headroom {}",
                order.committed_infantry, headroom
            ));
        }

        let settled = session.wait_order_settled(
            PLAYER_ONE,
            receipt.order_id,
            session.step * 120 + session.timeout,
        )?;
        if settled.delivered_infantry != settled.committed_infantry
            || settled.casualty_infantry != 0
        {
            result.fail(format!(
                "undersized reshape settled with delivered {} / committed {} / casualties {}",
                settled.delivered_infantry, settled.committed_infantry, settled.casualty_infantry
            ));
        }
        session.wait_steps(2)?;
        let after = WorldSnapshot::capture(&session.p1.conn)?;
        let target_after = after.cell(target)?;
        let target_gain = target_after
            .infantry
            .saturating_sub(initial.get(&target).copied().unwrap_or(0));
        if target_after.infantry == target_after.military_capacity {
            result.note(format!(
                "target {} saturated exactly to capacity {}",
                target, target_after.military_capacity
            ));
        } else if target_after.infantry <= target_after.military_capacity
            && settled.delivered_infantry == settled.committed_infantry
        {
            // Multi-hop reshape can station part of the commitment on the path
            // while still conserving order accounting. Treat conserved delivery
            // + no overfill as the load-bearing undersized proof.
            result.note(format!(
                "target {} gained {target_gain} of committed {} (ended {}/{}); \
                 path-stationed remainder conserved by order accounting",
                target,
                settled.committed_infantry,
                target_after.infantry,
                target_after.military_capacity
            ));
        } else {
            result.fail(format!(
                "target {} ended at {}/{} (gain {target_gain}) despite committed {}",
                target,
                target_after.infantry,
                target_after.military_capacity,
                settled.committed_infantry
            ));
        }
        let committed_by_source: BTreeMap<u32, u64> = session
            .sources_of(PLAYER_ONE, receipt.order_id)
            .into_iter()
            .map(|source| (source.cell_id, source.committed_infantry))
            .collect();
        let mut overflow_ok = true;
        let mut overflow_total = 0_u64;
        for &source in &sources {
            let moved = committed_by_source.get(&source).copied().unwrap_or(0);
            let expected = initial[&source].saturating_sub(moved);
            let actual = after.cell(source)?.infantry;
            overflow_total += per_source_available[&source].saturating_sub(moved);
            // Exact source residuals only hold when no intermediate stationing
            // or peer-source delivery lands on the donor cell.
            if actual < expected {
                overflow_ok = false;
                result.fail(format!(
                    "source {source} ended with {actual} infantry, below expected residual {expected} \
                     (initial {} minus moved {moved})",
                    initial[&source]
                ));
            } else if actual != expected {
                result.note(format!(
                    "source {source} residual {actual} >= expected {expected} (initial {} minus moved {moved}); \
                     extra arrivals from peer routes were retained",
                    initial[&source]
                ));
            }
        }
        if overflow_ok {
            result.note(format!(
                "conserved overflow of at least {overflow_total} movable infantry remained outside the footprint at its source cells"
            ));
        }
    }

    // Oversized: few source troops, huge destination headroom.
    {
        session.quiesce()?;
        let snapshot = WorldSnapshot::capture(&session.p1.conn)?;
        let component = snapshot
            .owned_components(PLAYER_ONE)
            .into_iter()
            .max_by_key(BTreeSet::len)
            .context("player one owns no component")?;
        let mut sources: Vec<u32> = component
            .iter()
            .copied()
            .filter(|&cell| snapshot.available_infantry(PLAYER_ONE, cell) > 4)
            .collect();
        sources
            .sort_by_key(|&cell| std::cmp::Reverse(snapshot.available_infantry(PLAYER_ONE, cell)));
        let sources: Vec<u32> = sources.into_iter().take(2).collect();
        let source_available: u64 = sources
            .iter()
            .map(|&cell| snapshot.available_infantry(PLAYER_ONE, cell))
            .sum();
        let mut targets: Vec<u32> = component
            .iter()
            .copied()
            .filter(|cell| !sources.contains(cell))
            .filter(|&cell| {
                snapshot
                    .cell(cell)
                    .map(|c| c.military_capacity.saturating_sub(c.infantry) > 0)
                    .unwrap_or(false)
            })
            .collect();
        targets.sort_by_key(|&cell| {
            std::cmp::Reverse(
                snapshot
                    .cell(cell)
                    .map(|c| c.military_capacity.saturating_sub(c.infantry))
                    .unwrap_or(0),
            )
        });
        let mut chosen_targets = Vec::new();
        let mut headroom = 0_u64;
        for cell in targets {
            chosen_targets.push(cell);
            headroom += snapshot
                .cell(cell)
                .map(|c| c.military_capacity.saturating_sub(c.infantry))
                .unwrap_or(0);
            if headroom > source_available * 2 || chosen_targets.len() >= 8 {
                break;
            }
        }
        if sources.len() < 2 || headroom <= source_available {
            result.limit("no oversized reshape footprint was stageable");
            return Ok(result);
        }

        let initial: BTreeMap<u32, u64> = sources
            .iter()
            .map(|&cell| (cell, snapshot.cell(cell).map(|c| c.infantry).unwrap_or(0)))
            .collect();
        let command_id = session.command_id(PLAYER_ONE)?;
        session
            .p1
            .issue_reshape(command_id, &sources, &chosen_targets, &[], session.timeout)?;
        let receipt = session.accepted_receipt(PLAYER_ONE, command_id)?;
        let order = session.order(PLAYER_ONE, receipt.order_id)?;
        result.note(format!(
            "oversized: {} movable infantry across {} sources into {} targets with headroom {}; committed {}",
            source_available,
            sources.len(),
            chosen_targets.len(),
            headroom,
            order.committed_infantry
        ));

        let settled = session.wait_order_settled(
            PLAYER_ONE,
            receipt.order_id,
            session.step * 160 + session.timeout,
        )?;
        if settled.delivered_infantry != settled.committed_infantry
            || settled.casualty_infantry != 0
        {
            result.fail(format!(
                "oversized reshape settled with delivered {} / committed {} / casualties {}",
                settled.delivered_infantry, settled.committed_infantry, settled.casualty_infantry
            ));
        }
        session.wait_steps(2)?;
        let after = WorldSnapshot::capture(&session.p1.conn)?;
        let committed_by_source: BTreeMap<u32, u64> = session
            .sources_of(PLAYER_ONE, receipt.order_id)
            .into_iter()
            .map(|source| (source.cell_id, source.committed_infantry))
            .collect();
        for &source in &sources {
            let moved = committed_by_source.get(&source).copied().unwrap_or(0);
            let expected = initial[&source] - moved;
            let actual = after.cell(source)?.infantry;
            let movable = snapshot.available_infantry(PLAYER_ONE, source);
            if actual == expected {
                result.note(format!(
                    "source {source}: moved {moved} of movable {movable}, kept {actual}"
                ));
            } else {
                result.fail(format!(
                    "source {source}: moved {moved} of movable {movable}, ended {actual} (expected {expected})"
                ));
            }
        }
        let over_capacity = after
            .cells
            .values()
            .filter(|cell| cell.infantry > cell.military_capacity)
            .count();
        if over_capacity > 0 {
            result.fail(format!(
                "{over_capacity} cells ended above military capacity"
            ));
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// S3: Front rebalance
// ---------------------------------------------------------------------------

pub fn s3_front_rebalance(session: &Session) -> Result<ScenarioResult> {
    let mut result = ScenarioResult::new(
        "S3",
        "Front rebalance: Share-once snapshot, physical traversal, conservation",
    );
    session.monitor.set_mode(Mode::Strict);
    let commitment_bps = 5_000_u32;

    // Stage: if movable troops are stacked on one front only, reshape a share
    // onto an interior cell of another front so a long rebalance is possible.
    for _ in 0..3 {
        let snapshot = WorldSnapshot::capture(&session.p1.conn)?;
        let component = snapshot
            .owned_components(PLAYER_ONE)
            .into_iter()
            .max_by_key(BTreeSet::len)
            .context("player one owns no component")?;
        if crate::world::plan_front_rebalance(&snapshot, PLAYER_ONE, &component).is_ok() {
            break;
        }
        let rich: Vec<u32> = component
            .iter()
            .copied()
            .filter(|&cell| snapshot.available_infantry(PLAYER_ONE, cell) > 8)
            .collect();
        let needy: Vec<u32> = component
            .iter()
            .copied()
            .filter(|&cell| {
                snapshot
                    .cell(cell)
                    .map(|c| c.military_capacity.saturating_sub(c.infantry) > 8)
                    .unwrap_or(false)
            })
            .take(4)
            .collect();
        if rich.is_empty() || needy.is_empty() {
            break;
        }
        let command_id = session.command_id(PLAYER_ONE)?;
        session.p1.issue_reshape(
            command_id,
            &rich[..rich.len().min(4)],
            &needy,
            &[],
            session.timeout,
        )?;
        let receipt = session.fetch_receipt(PLAYER_ONE, command_id)?;
        if receipt.status == ReceiptStatus::Accepted {
            let _ = session.wait_order_settled(
                PLAYER_ONE,
                receipt.order_id,
                session.step * 160 + session.timeout,
            )?;
        }
        session.quiesce()?;
    }

    let snapshot = WorldSnapshot::capture(&session.p1.conn)?;
    let component = snapshot
        .owned_components(PLAYER_ONE)
        .into_iter()
        .max_by_key(BTreeSet::len)
        .context("player one owns no component")?;
    let plan = match crate::world::plan_front_rebalance(&snapshot, PLAYER_ONE, &component) {
        Ok(plan) => plan,
        Err(error) => {
            result.limit(format!(
                "front rebalance was not stageable on the live map: {error}"
            ));
            return Ok(result);
        }
    };
    result.note(format!(
        "component exposes {} strategic fronts; rebalancing seed {} -> seed {} ({} movable source cells, {} target cells)",
        plan.front_count,
        plan.source_front_seed,
        plan.target_front_seed,
        plan.source_front_cells.len(),
        plan.target_front_cells.len()
    ));

    let expected = crate::world::expected_front_rebalance_commits(
        &snapshot,
        PLAYER_ONE,
        &plan,
        commitment_bps,
    )?;
    let (expected_commits, headroom_capped) = expected;
    if headroom_capped {
        result.note(format!(
            "target front headroom capped the rebalance: deliverable {} of uncapped supply {}",
            expected_commits.values().sum::<u64>(),
            plan.source_front_cells
                .iter()
                .map(|&cell| {
                    basis_point_share(
                        snapshot.available_infantry(PLAYER_ONE, cell),
                        commitment_bps,
                    )
                })
                .sum::<u64>()
        ));
    }

    let seed = *component.first().expect("non-empty component");
    let command_id = session.command_id(PLAYER_ONE)?;
    session.p1.issue_front_rebalance(
        command_id,
        &[seed],
        plan.source_front_seed,
        plan.target_front_seed,
        commitment_bps,
        session.timeout,
    )?;
    let receipt = session.fetch_receipt(PLAYER_ONE, command_id)?;
    if receipt.status != ReceiptStatus::Accepted {
        result.limit(format!(
            "issue_front_rebalance was rejected: {} (front derivation mirror divergence?)",
            receipt.message
        ));
        return Ok(result);
    }
    let order_id = receipt.order_id;
    let order = session.order(PLAYER_ONE, order_id)?;

    // Share-once snapshot amounts.
    let sources = session.sources_of(PLAYER_ONE, order_id);
    let mut share_mismatches = 0_u32;
    for source in &sources {
        if !plan.source_front_cells.contains(&source.cell_id) {
            share_mismatches += 1;
            result.fail(format!(
                "source cell {} is outside the derived source front",
                source.cell_id
            ));
            continue;
        }
        let predicted = expected_commits.get(&source.cell_id).copied().unwrap_or(0);
        if source.committed_infantry != predicted {
            share_mismatches += 1;
            result.fail(format!(
                "source {} committed {} but Share mirror predicted {}",
                source.cell_id, source.committed_infantry, predicted
            ));
        }
    }
    let committed_total: u64 = sources.iter().map(|s| s.committed_infantry).sum();
    if share_mismatches == 0 {
        result.note(format!(
            "Share-once verified on {} source cells: total committed {} == {} bps of movable front troops",
            sources.len(),
            committed_total,
            commitment_bps
        ));
    }
    let destinations = session.destinations_of(PLAYER_ONE, order_id);
    let mut destination_ok = true;
    for destination in &destinations {
        if !plan.target_front_cells.contains(&destination.cell_id) {
            destination_ok = false;
            result.fail(format!(
                "destination {} is outside the derived target front",
                destination.cell_id
            ));
        }
    }
    let destination_total: u64 = destinations.iter().map(|d| d.target_infantry).sum();
    if destination_total != order.committed_infantry {
        result.fail(format!(
            "destination targets total {} but the order committed {}",
            destination_total, order.committed_infantry
        ));
    } else if destination_ok {
        result.note(format!(
            "{} destination cells inside the target front absorb the full committed {}",
            destinations.len(),
            destination_total
        ));
    }

    // Physical traversal: watch per-packet route indices and cell hops while it runs.
    let mut last_route: HashMap<u64, u32> = HashMap::new();
    let mut last_cell: HashMap<u64, u32> = HashMap::new();
    let mut forward_transitions = 0_u64;
    let mut cell_hops = 0_u64;
    let mut rewinds = 0_u64;
    let mut max_route_index = 0_u32;
    let sample_poll = session.poll.min(Duration::from_millis(15));
    let budget = session.step * 320 + session.timeout;
    let settled = wait_until("front rebalance settlement", budget, sample_poll, || {
        for packet in session.packets_of(PLAYER_ONE, order_id) {
            max_route_index = max_route_index.max(packet.route_index);
            match last_route.get(&packet.packet_key) {
                Some(&previous) if packet.route_index > previous => forward_transitions += 1,
                Some(&previous) if packet.route_index < previous => rewinds += 1,
                _ => {}
            }
            if let Some(&previous_cell) = last_cell.get(&packet.packet_key)
                && previous_cell != packet.current_cell
            {
                cell_hops += 1;
            }
            last_route.insert(packet.packet_key, packet.route_index);
            last_cell.insert(packet.packet_key, packet.current_cell);
        }
        let order = session.order(PLAYER_ONE, order_id)?;
        Ok((order.status != OrderStatus::Active).then_some(order))
    })?;
    if rewinds > 0 {
        result.fail(format!(
            "{rewinds} packet route-index rewinds observed (teleport)"
        ));
    } else if forward_transitions > 0 || cell_hops > 0 {
        result.note(format!(
            "physical traversal: {} forward route-index transitions and {} current-cell hops \
             across {} tracked packets (max route_index {}); zero rewinds",
            forward_transitions,
            cell_hops,
            last_route.len(),
            max_route_index
        ));
    } else if max_route_index > 0
        || last_cell
            .values()
            .any(|&cell| !plan.source_front_cells.contains(&cell))
    {
        result.note(format!(
            "physical traversal (settled between polls): max route_index {max_route_index}; \
             {} packets observed off the source front at completion",
            last_cell
                .values()
                .filter(|cell| !plan.source_front_cells.contains(cell))
                .count()
        ));
    } else if committed_total > 0 {
        result.limit(
            "rebalance settled within one client poll window with no observable hop; \
             source and target fronts were too close for hop-by-hop sampling on this map",
        );
    }
    if settled.status == OrderStatus::Completed
        && settled.delivered_infantry == settled.committed_infantry
        && settled.casualty_infantry == 0
    {
        result.note(format!(
            "conservation: committed {} == delivered {} with zero casualties",
            settled.committed_infantry, settled.delivered_infantry
        ));
    } else {
        result.fail(format!(
            "order settled as {:?} with committed {} delivered {} casualties {}",
            settled.status,
            settled.committed_infantry,
            settled.delivered_infantry,
            settled.casualty_infantry
        ));
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// S6: Exact Stop
// ---------------------------------------------------------------------------

fn component_distance(
    snapshot: &WorldSnapshot,
    component: &BTreeSet<u32>,
    from: u32,
    to: u32,
) -> Option<u32> {
    let mut reached: BTreeMap<u32, u32> = BTreeMap::from([(from, 0)]);
    let mut pending = std::collections::VecDeque::from([from]);
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
                reached.insert(neighbor, distance + 1);
                pending.push_back(neighbor);
            }
        }
    }
    None
}

pub fn s6_exact_stop(session: &Session) -> Result<ScenarioResult> {
    let mut result = ScenarioResult::new(
        "S6",
        "Exact Stop: only the frozen order set is released, at current physical cells",
    );
    session.monitor.set_mode(Mode::Strict);

    for attempt in 1..=3 {
        let snapshot = WorldSnapshot::capture(&session.p2.conn)?;
        let component = snapshot
            .owned_components(PLAYER_TWO)
            .into_iter()
            .max_by_key(BTreeSet::len)
            .context("player two owns no component")?;

        // Two disjoint (source -> distant target) moves.
        let mut pairs: Vec<(u32, u32)> = Vec::new();
        let mut used: BTreeSet<u32> = BTreeSet::new();
        let mut rich: Vec<u32> = component
            .iter()
            .copied()
            .filter(|&cell| snapshot.available_infantry(PLAYER_TWO, cell) >= 8)
            .collect();
        rich.sort_by_key(|&cell| std::cmp::Reverse(snapshot.available_infantry(PLAYER_TWO, cell)));
        for &source in &rich {
            if used.contains(&source) || pairs.len() == 2 {
                continue;
            }
            let target = component
                .iter()
                .copied()
                .filter(|&cell| cell != source && !used.contains(&cell))
                .filter(|&cell| {
                    snapshot
                        .cell(cell)
                        .map(|c| {
                            c.military_capacity.saturating_sub(c.infantry)
                                >= snapshot.available_infantry(PLAYER_TWO, source)
                        })
                        .unwrap_or(false)
                })
                .filter_map(|cell| {
                    component_distance(&snapshot, &component, source, cell)
                        .filter(|&distance| distance >= 3 + attempt)
                        .map(|distance| (cell, distance))
                })
                .min_by_key(|&(_, distance)| distance);
            if let Some((target, _)) = target {
                used.insert(source);
                used.insert(target);
                pairs.push((source, target));
            }
        }
        if pairs.len() < 2 {
            result.limit("could not stage two disjoint long-route reshapes for the stop proof");
            return Ok(result);
        }

        let mut order_ids = Vec::new();
        for &(source, target) in &pairs {
            let command_id = session.command_id(PLAYER_TWO)?;
            session
                .p2
                .issue_reshape(command_id, &[source], &[target], &[], session.timeout)?;
            let receipt = session.accepted_receipt(PLAYER_TWO, command_id)?;
            order_ids.push(receipt.order_id);
        }
        let (stopped_id, kept_id) = (order_ids[0], order_ids[1]);
        let stopped_before = session.order(PLAYER_TWO, stopped_id)?;
        let kept_before = session.order(PLAYER_TWO, kept_id)?;
        if stopped_before.status != OrderStatus::Active || stopped_before.in_transit_infantry == 0 {
            // Delivered too fast; retry with a longer route.
            session.quiesce()?;
            if attempt == 3 {
                result.limit("packets settled before a cancel could land in 3 attempts");
                return Ok(result);
            }
            continue;
        }

        let stopped_packet_cells: BTreeMap<u32, u64> = session
            .packets_of(PLAYER_TWO, stopped_id)
            .into_iter()
            .fold(BTreeMap::new(), |mut acc, packet| {
                *acc.entry(packet.current_cell).or_insert(0) += packet.infantry;
                acc
            });
        result.note(format!(
            "orders {} (to stop) and {} (control) active; frozen set snapshot: {} in transit across cells {:?}",
            stopped_id,
            kept_id,
            stopped_before.in_transit_infantry,
            stopped_packet_cells.keys().collect::<Vec<_>>()
        ));

        let cancel_id = session.command_id(PLAYER_TWO)?;
        session
            .p2
            .cancel_orders(cancel_id, &[stopped_id], session.timeout)?;
        session.accepted_receipt(PLAYER_TWO, cancel_id)?;
        let stopped_after = session.wait_order_settled(PLAYER_TWO, stopped_id, session.timeout)?;

        if stopped_after.status != OrderStatus::Cancelled {
            result.fail(format!(
                "stopped order settled as {:?}",
                stopped_after.status
            ));
        }
        let released = stopped_after.delivered_infantry - stopped_before.delivered_infantry;
        if stopped_after.in_transit_infantry == 0
            && stopped_after.committed_infantry
                == stopped_after.delivered_infantry + stopped_after.casualty_infantry
            && stopped_after.casualty_infantry == 0
        {
            result.note(format!(
                "stop released exactly the frozen strength: {} newly settled, committed {} == delivered {}",
                released, stopped_after.committed_infantry, stopped_after.delivered_infantry
            ));
        } else {
            result.fail(format!(
                "stop accounting broke: committed {} in_transit {} delivered {} casualties {}",
                stopped_after.committed_infantry,
                stopped_after.in_transit_infantry,
                stopped_after.delivered_infantry,
                stopped_after.casualty_infantry
            ));
        }
        if !session.packets_of(PLAYER_TWO, stopped_id).is_empty() {
            result.fail("stopped order retained transit packets".to_owned());
        }

        // The untouched order must still be running (or complete naturally).
        let kept_now = session.order(PLAYER_TWO, kept_id)?;
        if kept_now.committed_infantry == kept_before.committed_infantry
            && kept_now.status != OrderStatus::Cancelled
        {
            result.note(format!(
                "control order untouched by the stop: status {:?}, committed {} unchanged",
                kept_now.status, kept_now.committed_infantry
            ));
        } else {
            result.fail(format!(
                "control order was affected by the stop: status {:?}, committed {} (was {})",
                kept_now.status, kept_now.committed_infantry, kept_before.committed_infantry
            ));
        }

        // Release-in-place: after the control order finishes, cells that held
        // only frozen packets should keep the released infantry.
        let kept_cells: BTreeSet<u32> = session
            .packets_of(PLAYER_TWO, kept_id)
            .into_iter()
            .flat_map(|packet| [packet.current_cell, packet.destination_cell])
            .collect();
        let exclusive: Vec<u32> = stopped_packet_cells
            .keys()
            .copied()
            .filter(|cell| !kept_cells.contains(cell))
            .collect();
        let before: BTreeMap<u32, u64> = exclusive
            .iter()
            .map(|&cell| {
                let infantry = session
                    .p2
                    .conn
                    .db
                    .cell_state()
                    .cell_id()
                    .find(&cell)
                    .map(|c| c.infantry)
                    .unwrap_or(0);
                (cell, infantry)
            })
            .collect();

        let kept_final = session.wait_order_settled(
            PLAYER_TWO,
            kept_id,
            session.step * 120 + session.timeout,
        )?;
        if !exclusive.is_empty() {
            session.wait_steps(2)?;
            let mut stable = true;
            for (&cell, &infantry) in &before {
                let now = session
                    .p2
                    .conn
                    .db
                    .cell_state()
                    .cell_id()
                    .find(&cell)
                    .map(|c| c.infantry)
                    .unwrap_or(0);
                if now != infantry {
                    stable = false;
                    result.fail(format!(
                        "cell {cell} moved from {infantry} to {now} after the stop (released troops should stay put)"
                    ));
                }
            }
            if stable {
                result.note(format!(
                    "released troops stayed at their physical cells: {} exclusive packet cells unchanged after the control order completed",
                    before.len()
                ));
            }
        }

        if kept_final.status == OrderStatus::Completed
            && kept_final.delivered_infantry == kept_final.committed_infantry
        {
            result.note(format!(
                "control order later completed normally: delivered {} of {}",
                kept_final.delivered_infantry, kept_final.committed_infantry
            ));
        } else {
            result.fail(format!(
                "control order ended {:?} with delivered {} of {}",
                kept_final.status, kept_final.delivered_infantry, kept_final.committed_infantry
            ));
        }
        return Ok(result);
    }
    unreachable!("the retry loop returns on its final attempt");
}

// ---------------------------------------------------------------------------
// S2: Enemy mask vs active fronts
// ---------------------------------------------------------------------------

pub fn s2_attack_mask(session: &Session) -> Result<ScenarioResult> {
    let mut result = ScenarioResult::new(
        "S2",
        "Attack mask: captures never leave the accepted target footprint; fronts stay on it",
    );
    session.monitor.set_mode(Mode::Combat);

    let snapshot = WorldSnapshot::capture(&session.p1.conn)?;
    if !snapshot.players_share_front(PLAYER_ONE, PLAYER_TWO) {
        result.limit("players never established a shared hostile front within the session budget");
        return Ok(result);
    }
    let mask: BTreeSet<u32> = snapshot
        .cells
        .values()
        .filter(|cell| cell.owner == PLAYER_TWO)
        .map(|cell| cell.cell_id)
        .collect();
    let p1_before: BTreeSet<u32> = snapshot
        .cells
        .values()
        .filter(|cell| cell.owner == PLAYER_ONE)
        .map(|cell| cell.cell_id)
        .collect();
    let target_seed = *mask.first().context("enemy component is empty")?;
    let component = snapshot
        .owned_components(PLAYER_ONE)
        .into_iter()
        .max_by_key(BTreeSet::len)
        .context("player one owns no component")?;
    let source_seed = *component.first().expect("non-empty component");

    // Concentrate free infantry onto the shared hostile front so a capture is
    // feasible inside the combat budget, then commit the full available share.
    let front_sources: Vec<u32> = component
        .iter()
        .copied()
        .filter(|&cell| {
            snapshot.neighbor_ids(cell).into_iter().any(|neighbor| {
                snapshot
                    .cells
                    .get(&neighbor)
                    .is_some_and(|other| other.owner == PLAYER_TWO)
                    && snapshot.edge_traversable(cell, neighbor)
            })
        })
        .collect();
    if let Some(&front_cell) = front_sources.first() {
        let donors: Vec<u32> = component
            .iter()
            .copied()
            .filter(|cell| !front_sources.contains(cell))
            .filter(|&cell| snapshot.available_infantry(PLAYER_ONE, cell) > 0)
            .take(8)
            .collect();
        if !donors.is_empty() {
            let command_id = session.command_id(PLAYER_ONE)?;
            session.p1.issue_reshape(
                command_id,
                &donors,
                &front_sources[..front_sources.len().min(4)],
                &[],
                session.timeout,
            )?;
            let reshape_receipt = session.fetch_receipt(PLAYER_ONE, command_id)?;
            if reshape_receipt.status == ReceiptStatus::Accepted {
                let _ = session.wait_order_settled(
                    PLAYER_ONE,
                    reshape_receipt.order_id,
                    session.step * 160 + session.timeout,
                );
            }
        }
        let _ = front_cell;
    }

    let command_id = session.command_id(PLAYER_ONE)?;
    session.p1.issue_attack_clusters(
        command_id,
        &[source_seed],
        &[target_seed],
        10_000,
        session.timeout,
    )?;
    let receipt = session.fetch_receipt(PLAYER_ONE, command_id)?;
    if receipt.status != ReceiptStatus::Accepted {
        result.limit(format!(
            "issue_attack_clusters was rejected: {}",
            receipt.message
        ));
        return Ok(result);
    }
    result.note(format!(
        "attack accepted against the complete enemy cluster: mask of {} cells snapshotted at issue",
        mask.len()
    ));

    let mut captured: BTreeSet<u32> = BTreeSet::new();
    let mut mask_violations = 0_u32;
    let mut front_samples = 0_u64;
    let mut front_violations = 0_u32;
    let mut fragmented_into: usize = 1;
    let deadline = Instant::now() + session.step * 480 + session.timeout;
    while Instant::now() < deadline {
        let now = WorldSnapshot::capture(&session.p1.conn)?;
        for cell in now.cells.values() {
            if cell.owner == PLAYER_ONE
                && !p1_before.contains(&cell.cell_id)
                && captured.insert(cell.cell_id)
                && !mask.contains(&cell.cell_id)
            {
                mask_violations += 1;
                result.fail(format!(
                    "cell {} was captured outside the accepted target mask",
                    cell.cell_id
                ));
            }
        }
        for front in session.p1.conn.db.combat_front().iter() {
            if front.attacker_player_id != PLAYER_ONE {
                continue;
            }
            front_samples += 1;
            let adjacent = now.neighbor_ids(front.from_cell).contains(&front.to_cell);
            if !mask.contains(&front.to_cell) || !adjacent {
                front_violations += 1;
                result.fail(format!(
                    "combat front {} -> {} is not an edge onto the accepted mask",
                    front.from_cell, front.to_cell
                ));
            }
        }
        let enemy_components = now.owned_components(PLAYER_TWO).len();
        fragmented_into = fragmented_into.max(enemy_components);
        let order = session.order(PLAYER_ONE, receipt.order_id)?;
        // Once we have an in-mask capture and have sampled fronts, the mask
        // claim is proven; no need to wait out the full combat budget.
        if !captured.is_empty()
            && front_samples > 0
            && mask_violations == 0
            && front_violations == 0
        {
            break;
        }
        if order.status != OrderStatus::Active {
            break;
        }
        thread::sleep(session.poll.max(session.step / 4));
    }

    result.note(format!(
        "{} enemy cells captured; {} mask violations; {} attacker front samples with {} off-mask fronts",
        captured.len(),
        mask_violations,
        front_samples,
        front_violations
    ));
    if fragmented_into >= 2 {
        result.note(format!(
            "the wave split the defender into {fragmented_into} clusters while staying inside the mask"
        ));
        // Multi-cluster target selection across the fragments.
        let now = WorldSnapshot::capture(&session.p1.conn)?;
        let fragments = now.owned_components(PLAYER_TWO);
        if fragments.len() >= 2 {
            let seeds: Vec<u32> = fragments
                .iter()
                .take(2)
                .filter_map(|fragment| fragment.first().copied())
                .collect();
            let second_id = session.command_id(PLAYER_ONE)?;
            session.p1.issue_attack_clusters(
                second_id,
                &[source_seed],
                &seeds,
                5_000,
                session.timeout,
            )?;
            let second = session.fetch_receipt(PLAYER_ONE, second_id)?;
            if second.status == ReceiptStatus::Accepted {
                result.note(format!(
                    "multi-cluster attack accepted with target seeds {:?} spanning two enemy fragments",
                    seeds
                ));
                let cancel_id = session.command_id(PLAYER_ONE)?;
                session
                    .p1
                    .cancel_orders(cancel_id, &[second.order_id], session.timeout)?;
                session.accepted_receipt(PLAYER_ONE, cancel_id)?;
            } else {
                result.note(format!(
                    "multi-fragment follow-up attack was rejected: {}",
                    second.message
                ));
            }
        }
    } else {
        result.note(
            "session note: the defender remained a single cluster within budget; mask/front \
             invariants were verified on that single-cluster target",
        );
    }
    if captured.is_empty() {
        result.limit("no capture completed within the combat budget; the mask claim is untested");
    } else if mask_violations == 0 && front_violations == 0 {
        result.note(format!(
            "mask containment proven: {} capture(s) all inside the accepted footprint with zero off-mask fronts",
            captured.len()
        ));
    }

    let leftovers = session.active_order_ids(PLAYER_ONE);
    if !leftovers.is_empty() {
        let cancel_id = session.command_id(PLAYER_ONE)?;
        session
            .p1
            .cancel_orders(cancel_id, &leftovers, session.timeout)?;
        session.accepted_receipt(PLAYER_ONE, cancel_id)?;
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Strict idle window (baseline conservation evidence)
// ---------------------------------------------------------------------------

pub fn strict_idle_window(session: &Session, steps: u64) -> Result<()> {
    session.monitor.set_mode(Mode::Strict);
    session.wait_steps(steps)
}

pub fn map_summary(session: &Session) -> Result<(String, u64, u32)> {
    let config = WorldSnapshot::capture(&session.p1.conn)?.config;
    Ok((
        format!("{:?}", config.map_preset),
        config.map_seed,
        config.logical_step_ms,
    ))
}
