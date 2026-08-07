use std::collections::{BTreeMap, BTreeSet};

use crate::{
    coord::{Axial, HexEdge},
    map::{HexMap, LogisticsConfig, MovementConfig, PlayerId, Strength, ground_traversal},
    pathfinding::Path,
};

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferRequest {
    pub owner: PlayerId,
    pub sources: Vec<Axial>,
    pub destinations: Vec<Axial>,
    pub amount: Strength,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanError {
    EmptySources,
    EmptyDestinations,
    ZeroAmount,
    OverlappingSelections(Axial),
    UnknownSource(Axial),
    UnknownDestination(Axial),
    SourceNotOwned(Axial),
    DestinationNotOwned(Axial),
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferLeg {
    pub source: Axial,
    pub destination: Axial,
    pub amount: Strength,
    pub path: Path,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferPlan {
    pub owner: PlayerId,
    pub requested: Strength,
    pub planned: Strength,
    pub unplanned: Strength,
    pub legs: Vec<TransferLeg>,
    pub unreachable_sources: Vec<Axial>,
    pub unreachable_destinations: Vec<Axial>,
    pub full_destinations: Vec<Axial>,
}

/// Plans a friendly source-to-destination transfer without mutating the map.
///
/// Candidate pairs are considered by `(route cost, source, destination)`, so
/// allocation and route choice remain stable across selection gesture order.
///
/// Routing runs one reverse Dijkstra per destination over the owner's cells
/// (instead of one A* per source×destination pair), so the pair cost matrix
/// and therefore the allocation order match the previous per-pair search.
pub fn plan_transfer(
    map: &HexMap,
    request: &TransferRequest,
    movement: &MovementConfig,
) -> Result<TransferPlan, PlanError> {
    let sources: BTreeSet<_> = request.sources.iter().copied().collect();
    let destinations: BTreeSet<_> = request.destinations.iter().copied().collect();
    if sources.is_empty() {
        return Err(PlanError::EmptySources);
    }
    if destinations.is_empty() {
        return Err(PlanError::EmptyDestinations);
    }
    if request.amount == 0 {
        return Err(PlanError::ZeroAmount);
    }
    if let Some(overlap) = sources.intersection(&destinations).next() {
        return Err(PlanError::OverlappingSelections(*overlap));
    }

    for &source in &sources {
        let cell = map.get(source).ok_or(PlanError::UnknownSource(source))?;
        if cell.owner != Some(request.owner) {
            return Err(PlanError::SourceNotOwned(source));
        }
    }
    for &destination in &destinations {
        let cell = map
            .get(destination)
            .ok_or(PlanError::UnknownDestination(destination))?;
        if cell.owner != Some(request.owner) {
            return Err(PlanError::DestinationNotOwned(destination));
        }
    }

    #[derive(Clone)]
    struct Candidate {
        source: Axial,
        destination: Axial,
        path: Path,
    }

    let mut candidates = Vec::new();
    let mut reachable_sources = BTreeSet::new();
    let mut reachable_destinations = BTreeSet::new();
    for &destination in &destinations {
        let field = destination_route_field(map, destination, movement, request.owner);
        for &source in &sources {
            let Some(path) = field.route_from(source) else {
                continue;
            };
            reachable_sources.insert(source);
            reachable_destinations.insert(destination);
            candidates.push(Candidate {
                source,
                destination,
                path,
            });
        }
    }
    candidates.sort_unstable_by_key(|candidate| {
        (
            candidate.path.total_cost,
            candidate.source,
            candidate.destination,
        )
    });

    let mut source_available: BTreeMap<_, _> = sources
        .iter()
        .map(|&source| (source, map.get(source).expect("validated source").force()))
        .collect();
    let mut destination_available: BTreeMap<_, _> = destinations
        .iter()
        .map(|&destination| {
            (
                destination,
                map.get(destination)
                    .expect("validated destination")
                    .free_military_capacity(),
            )
        })
        .collect();
    let full_destinations = destination_available
        .iter()
        .filter_map(|(&coordinate, &available)| (available == 0).then_some(coordinate))
        .collect();

    let mut remaining = request.amount;
    let mut legs = Vec::new();
    for candidate in candidates {
        if remaining == 0 {
            break;
        }
        let available_source = source_available[&candidate.source];
        let available_destination = destination_available[&candidate.destination];
        let amount = remaining.min(available_source).min(available_destination);
        if amount == 0 {
            continue;
        }
        *source_available
            .get_mut(&candidate.source)
            .expect("source was initialized") -= amount;
        *destination_available
            .get_mut(&candidate.destination)
            .expect("destination was initialized") -= amount;
        remaining -= amount;
        legs.push(TransferLeg {
            source: candidate.source,
            destination: candidate.destination,
            amount,
            path: candidate.path,
        });
    }

    let planned = request.amount - remaining;
    Ok(TransferPlan {
        owner: request.owner,
        requested: request.amount,
        planned,
        unplanned: remaining,
        legs,
        unreachable_sources: sources.difference(&reachable_sources).copied().collect(),
        unreachable_destinations: destinations
            .difference(&reachable_destinations)
            .copied()
            .collect(),
        full_destinations,
    })
}

/// Shortest-route costs from every cell of one owner's component toward a
/// single destination, with forward `next` pointers for path reconstruction.
struct DestinationRouteField {
    destination: Axial,
    /// Cost of the best route to the destination and the next hop toward it.
    routes: BTreeMap<Axial, (u64, Axial)>,
}

impl DestinationRouteField {
    fn route_from(&self, source: Axial) -> Option<Path> {
        if source == self.destination {
            return Some(Path {
                cells: vec![source],
                total_cost: 0,
            });
        }
        let (total_cost, _) = *self.routes.get(&source)?;
        let mut cells = vec![source];
        let mut current = source;
        while current != self.destination {
            let (_, next) = *self.routes.get(&current)?;
            cells.push(next);
            current = next;
        }
        Some(Path { cells, total_cost })
    }
}

/// One deterministic reverse Dijkstra from `destination` across cells owned by
/// `owner`. Edge costs are directional, so relaxation walks edges in the
/// travel direction (`neighbor -> current`). The ordered frontier, sorted
/// neighbor iteration, and strict-improvement rule make the field independent
/// of map insertion order.
fn destination_route_field(
    map: &HexMap,
    destination: Axial,
    movement: &MovementConfig,
    owner: PlayerId,
) -> DestinationRouteField {
    let mut field = DestinationRouteField {
        destination,
        routes: BTreeMap::new(),
    };
    let Some(destination_cell) = map.get(destination) else {
        return field;
    };
    if destination_cell.owner != Some(owner) {
        return field;
    }

    let mut frontier = BTreeSet::from([(0_u64, destination)]);
    let mut distances = BTreeMap::from([(destination, 0_u64)]);
    let mut visited = BTreeSet::new();
    while let Some((cost, current)) = frontier.pop_first() {
        if !visited.insert(current) {
            continue;
        }
        let current_cell = map.get(current).expect("visited cells exist");
        let mut neighbors = current.neighbors();
        neighbors.sort_unstable();
        for neighbor in neighbors {
            if visited.contains(&neighbor) {
                continue;
            }
            let Some(neighbor_cell) = map.get(neighbor) else {
                continue;
            };
            if neighbor_cell.owner != Some(owner) {
                continue;
            }
            // Travel direction is neighbor -> current, toward the destination.
            let Some(traversal) = ground_traversal(neighbor_cell, current_cell, movement) else {
                continue;
            };
            let Some(candidate) = cost.checked_add(u64::from(traversal.cost)) else {
                continue;
            };
            let best = distances.get(&neighbor).copied().unwrap_or(u64::MAX);
            if candidate < best {
                if best != u64::MAX {
                    frontier.remove(&(best, neighbor));
                }
                distances.insert(neighbor, candidate);
                field.routes.insert(neighbor, (candidate, current));
                frontier.insert((candidate, neighbor));
            }
        }
    }
    field
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MovementIntent {
    pub id: u64,
    /// Lower values are served first. ID is the stable tie-break.
    pub priority: u32,
    pub owner: PlayerId,
    pub from: Axial,
    pub to: Axial,
    pub requested: Strength,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MovementLimit {
    ZeroRequest,
    MissingSource,
    MissingDestination,
    NonAdjacent,
    Impassable,
    SourceNotOwned,
    DestinationNotOwned,
    SourceStrength,
    EdgeThroughput,
    DestinationCapacity,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MovementOutcome {
    pub intent_id: u64,
    pub requested: Strength,
    pub approved: Strength,
    pub limits: BTreeSet<MovementLimit>,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MovementError {
    DuplicateIntentId(u64),
    ArithmeticOverflow,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MovementStep {
    pub outcomes: BTreeMap<u64, MovementOutcome>,
    /// Sum crossing edges this step. This is not a unique-unit count when a
    /// pipeline contains multiple independently occupied cells.
    pub moved_total: Strength,
    /// Total force over the cells referenced by the submitted intents (their
    /// existing sources and destinations) before the step. The release hot
    /// path deliberately never scans the whole map; full-map conservation is
    /// still asserted in debug builds.
    pub strength_before: Strength,
    /// Total force over the same referenced cells after the step.
    pub strength_after: Strength,
}

#[derive(Clone, Copy, Debug)]
struct ValidIntent {
    intent: MovementIntent,
    edge: HexEdge,
}

#[derive(Default)]
struct FlowTotals {
    incoming: BTreeMap<Axial, Strength>,
    outgoing: BTreeMap<Axial, Strength>,
}

/// Applies one friendly logical movement step atomically.
///
/// The function first approves source/edge flow, then propagates destination
/// backpressure until every final occupancy is within capacity, and only then
/// commits all deltas. This allows a full column to advance as a simultaneous
/// pipeline while preventing a blocked downstream cell from creating overflow.
///
/// Failure locality: all checks are scoped to the cells the intents actually
/// touch. A cell that is already over capacity (a pre-existing inconsistency
/// elsewhere in the map, or on a touched route) never blocks unrelated
/// movement; such a cell may drain but is never approved for a net gain.
pub fn movement_step(
    map: &mut HexMap,
    intents: &[MovementIntent],
    movement: &MovementConfig,
    logistics: &LogisticsConfig,
) -> Result<MovementStep, MovementError> {
    let mut seen = BTreeSet::new();
    for intent in intents {
        if !seen.insert(intent.id) {
            return Err(MovementError::DuplicateIntentId(intent.id));
        }
    }
    let referenced: BTreeSet<Axial> = intents
        .iter()
        .flat_map(|intent| [intent.from, intent.to])
        .filter(|&coordinate| map.get(coordinate).is_some())
        .collect();
    let strength_before = total_force_at(map, &referenced)?;
    #[cfg(debug_assertions)]
    let full_strength_before = checked_total_force(map)?;

    let mut sorted = intents.to_vec();
    sorted.sort_unstable_by_key(|intent| (intent.priority, intent.id));
    let mut outcomes = BTreeMap::new();
    let mut valid = BTreeMap::<u64, ValidIntent>::new();
    let mut approvals = BTreeMap::<u64, Strength>::new();
    let mut source_remaining = BTreeMap::<Axial, Strength>::new();
    let mut edge_remaining = BTreeMap::<HexEdge, Strength>::new();

    for intent in sorted {
        let mut outcome = MovementOutcome {
            intent_id: intent.id,
            requested: intent.requested,
            approved: 0,
            limits: BTreeSet::new(),
        };
        if intent.requested == 0 {
            outcome.limits.insert(MovementLimit::ZeroRequest);
            outcomes.insert(intent.id, outcome);
            continue;
        }
        let Some(from_cell) = map.get(intent.from) else {
            outcome.limits.insert(MovementLimit::MissingSource);
            outcomes.insert(intent.id, outcome);
            continue;
        };
        let Some(to_cell) = map.get(intent.to) else {
            outcome.limits.insert(MovementLimit::MissingDestination);
            outcomes.insert(intent.id, outcome);
            continue;
        };
        let Some(edge) = HexEdge::new(intent.from, intent.to) else {
            outcome.limits.insert(MovementLimit::NonAdjacent);
            outcomes.insert(intent.id, outcome);
            continue;
        };
        if from_cell.owner != Some(intent.owner) {
            outcome.limits.insert(MovementLimit::SourceNotOwned);
            outcomes.insert(intent.id, outcome);
            continue;
        }
        if to_cell.owner != Some(intent.owner) {
            outcome.limits.insert(MovementLimit::DestinationNotOwned);
            outcomes.insert(intent.id, outcome);
            continue;
        }
        if ground_traversal(from_cell, to_cell, movement).is_none() {
            outcome.limits.insert(MovementLimit::Impassable);
            outcomes.insert(intent.id, outcome);
            continue;
        }

        let available_source = *source_remaining
            .entry(intent.from)
            .or_insert_with(|| from_cell.force());
        let available_edge = *edge_remaining.entry(edge).or_insert_with(|| {
            map.edge_limits(intent.from, intent.to, logistics)
                .expect("validated map edge")
                .throughput
        });
        let approved = intent.requested.min(available_source).min(available_edge);
        if available_source < intent.requested {
            outcome.limits.insert(MovementLimit::SourceStrength);
        }
        if available_edge < intent.requested {
            outcome.limits.insert(MovementLimit::EdgeThroughput);
        }
        *source_remaining
            .get_mut(&intent.from)
            .expect("source remainder was initialized") -= approved;
        *edge_remaining
            .get_mut(&edge)
            .expect("edge remainder was initialized") -= approved;
        outcome.approved = approved;
        approvals.insert(intent.id, approved);
        valid.insert(intent.id, ValidIntent { intent, edge });
        outcomes.insert(intent.id, outcome);
    }

    // Reductions only move downward, so this reaches a stable capacity-safe
    // solution after at most one propagation per participating route edge.
    loop {
        let totals = flow_totals(&valid, &approvals)?;
        let mut overfull = None;
        for (&coordinate, &amount_in) in &totals.incoming {
            let cell = map.get(coordinate).expect("validated destination");
            let amount_out = totals.outgoing.get(&coordinate).copied().unwrap_or(0);
            let final_force =
                u128::from(cell.force()) + u128::from(amount_in) - u128::from(amount_out);
            // A cell already over capacity may drain but never gains: its
            // effective bound is its current force, so reducing inbound flow
            // to zero always suffices and the loop still terminates.
            let effective_capacity =
                u128::from(cell.military_capacity).max(u128::from(cell.force()));
            if final_force > effective_capacity {
                overfull = Some((coordinate, (final_force - effective_capacity) as u64));
                break;
            }
        }
        let Some((coordinate, mut overflow)) = overfull else {
            break;
        };

        let mut inbound: Vec<_> = valid
            .values()
            .filter(|entry| entry.intent.to == coordinate && approvals[&entry.intent.id] > 0)
            .copied()
            .collect();
        // Later-served flows yield first when capacity applies backpressure.
        inbound.sort_unstable_by_key(|entry| (entry.intent.priority, entry.intent.id));
        inbound.reverse();
        for entry in inbound {
            let approved = approvals
                .get_mut(&entry.intent.id)
                .expect("valid intent has approval");
            let reduction = overflow.min(*approved);
            *approved -= reduction;
            overflow -= reduction;
            if reduction > 0 {
                let outcome = outcomes
                    .get_mut(&entry.intent.id)
                    .expect("valid intent has outcome");
                outcome.approved -= reduction;
                outcome.limits.insert(MovementLimit::DestinationCapacity);
            }
            if overflow == 0 {
                break;
            }
        }
        debug_assert_eq!(overflow, 0, "initial cells were capacity-safe");
    }

    let totals = flow_totals(&valid, &approvals)?;
    let touched: BTreeSet<_> = totals
        .incoming
        .keys()
        .chain(totals.outgoing.keys())
        .copied()
        .collect();
    for coordinate in touched {
        let amount_in = totals.incoming.get(&coordinate).copied().unwrap_or(0);
        let amount_out = totals.outgoing.get(&coordinate).copied().unwrap_or(0);
        let cell = map.get_mut(coordinate).expect("validated movement cell");
        let effective_capacity = cell.military_capacity.max(cell.force());
        let remaining = cell
            .forces
            .infantry
            .checked_sub(amount_out)
            .ok_or(MovementError::ArithmeticOverflow)?;
        cell.forces.infantry = remaining
            .checked_add(amount_in)
            .ok_or(MovementError::ArithmeticOverflow)?;
        debug_assert!(cell.force() <= effective_capacity);
    }

    let moved_total = approvals.values().try_fold(0_u64, |total, amount| {
        total
            .checked_add(*amount)
            .ok_or(MovementError::ArithmeticOverflow)
    })?;
    let strength_after = total_force_at(map, &referenced)?;
    debug_assert_eq!(strength_before, strength_after);
    #[cfg(debug_assertions)]
    {
        let full_strength_after = checked_total_force(map)?;
        debug_assert_eq!(
            full_strength_before, full_strength_after,
            "movement must conserve map-wide strength"
        );
    }

    Ok(MovementStep {
        outcomes,
        moved_total,
        strength_before,
        strength_after,
    })
}

fn flow_totals(
    valid: &BTreeMap<u64, ValidIntent>,
    approvals: &BTreeMap<u64, Strength>,
) -> Result<FlowTotals, MovementError> {
    let mut totals = FlowTotals::default();
    for (&id, entry) in valid {
        let amount = approvals.get(&id).copied().unwrap_or(0);
        let in_total = totals.incoming.entry(entry.intent.to).or_default();
        *in_total = in_total
            .checked_add(amount)
            .ok_or(MovementError::ArithmeticOverflow)?;
        let out_total = totals.outgoing.entry(entry.intent.from).or_default();
        *out_total = out_total
            .checked_add(amount)
            .ok_or(MovementError::ArithmeticOverflow)?;
        let _ = entry.edge;
    }
    Ok(totals)
}

fn total_force_at(map: &HexMap, coordinates: &BTreeSet<Axial>) -> Result<Strength, MovementError> {
    coordinates.iter().try_fold(0_u64, |total, &coordinate| {
        total
            .checked_add(map.get(coordinate).map_or(0, |cell| cell.force()))
            .ok_or(MovementError::ArithmeticOverflow)
    })
}

#[cfg(debug_assertions)]
fn checked_total_force(map: &HexMap) -> Result<Strength, MovementError> {
    map.cells().try_fold(0_u64, |total, cell| {
        total
            .checked_add(cell.force())
            .ok_or(MovementError::ArithmeticOverflow)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{Cell, EdgeLimits, ForceComposition};

    fn owned_line(length: i32, capacity: u64) -> HexMap {
        let mut map = HexMap::new();
        for q in 0..length {
            map.insert(Cell::ground(Axial::new(q, 0), 0, Some(1), capacity));
        }
        map
    }

    fn intent(id: u64, from: i32, to: i32, requested: u64) -> MovementIntent {
        MovementIntent {
            id,
            priority: 0,
            owner: 1,
            from: Axial::new(from, 0),
            to: Axial::new(to, 0),
            requested,
        }
    }

    #[test]
    fn planner_distributes_to_capacity_without_mutating_strength() {
        let mut map = owned_line(4, 100);
        map.get_mut(Axial::new(0, 0)).unwrap().forces = ForceComposition::infantry(70);
        map.get_mut(Axial::new(1, 0)).unwrap().forces = ForceComposition::infantry(50);
        map.get_mut(Axial::new(3, 0)).unwrap().forces = ForceComposition::infantry(80);
        let before = map.clone();
        let plan = plan_transfer(
            &map,
            &TransferRequest {
                owner: 1,
                sources: vec![Axial::new(1, 0), Axial::new(0, 0)],
                destinations: vec![Axial::new(3, 0), Axial::new(2, 0)],
                amount: 110,
            },
            &MovementConfig::default(),
        )
        .unwrap();
        assert_eq!(plan.planned, 110);
        assert_eq!(plan.unplanned, 0);
        assert_eq!(plan.legs.iter().map(|leg| leg.amount).sum::<u64>(), 110);
        assert_eq!(map, before);
    }

    #[test]
    fn planner_reports_components_separated_by_a_cliff() {
        let mut map = owned_line(3, 100);
        map.get_mut(Axial::new(0, 0)).unwrap().forces = ForceComposition::infantry(50);
        map.get_mut(Axial::new(1, 0)).unwrap().elevation = 2;
        let plan = plan_transfer(
            &map,
            &TransferRequest {
                owner: 1,
                sources: vec![Axial::new(0, 0)],
                destinations: vec![Axial::new(2, 0)],
                amount: 50,
            },
            &MovementConfig::default(),
        )
        .unwrap();
        assert_eq!(plan.planned, 0);
        assert_eq!(plan.unreachable_sources, vec![Axial::new(0, 0)]);
        assert_eq!(plan.unreachable_destinations, vec![Axial::new(2, 0)]);
    }

    #[test]
    fn movement_obeys_shared_edge_throughput_and_capacity() {
        let mut map = owned_line(2, 100);
        map.get_mut(Axial::new(0, 0)).unwrap().forces = ForceComposition::infantry(100);
        map.get_mut(Axial::new(1, 0)).unwrap().forces = ForceComposition::infantry(90);
        map.set_edge_limits(
            Axial::new(0, 0),
            Axial::new(1, 0),
            EdgeLimits {
                throughput: 20,
                frontage: 25,
            },
        );
        let result = movement_step(
            &mut map,
            &[intent(1, 0, 1, 50)],
            &MovementConfig::default(),
            &LogisticsConfig::default(),
        )
        .unwrap();
        assert_eq!(result.outcomes[&1].approved, 10);
        assert!(
            result.outcomes[&1]
                .limits
                .contains(&MovementLimit::DestinationCapacity)
        );
        assert_eq!(map.get(Axial::new(0, 0)).unwrap().force(), 90);
        assert_eq!(map.get(Axial::new(1, 0)).unwrap().force(), 100);
        assert_eq!(result.strength_before, result.strength_after);
    }

    #[test]
    fn full_cells_can_advance_as_a_pipeline() {
        let mut map = owned_line(3, 100);
        map.get_mut(Axial::new(0, 0)).unwrap().forces = ForceComposition::infantry(100);
        map.get_mut(Axial::new(1, 0)).unwrap().forces = ForceComposition::infantry(100);
        map.get_mut(Axial::new(2, 0)).unwrap().forces = ForceComposition::infantry(80);
        let result = movement_step(
            &mut map,
            &[intent(1, 0, 1, 20), intent(2, 1, 2, 20)],
            &MovementConfig::default(),
            &LogisticsConfig::default(),
        )
        .unwrap();
        assert_eq!(result.outcomes[&1].approved, 20);
        assert_eq!(result.outcomes[&2].approved, 20);
        assert_eq!(map.get(Axial::new(0, 0)).unwrap().force(), 80);
        assert_eq!(map.get(Axial::new(1, 0)).unwrap().force(), 100);
        assert_eq!(map.get(Axial::new(2, 0)).unwrap().force(), 100);
    }

    #[test]
    fn downstream_blockage_propagates_backpressure_upstream() {
        let mut map = owned_line(3, 100);
        for q in 0..3 {
            map.get_mut(Axial::new(q, 0)).unwrap().forces = ForceComposition::infantry(100);
        }
        let result = movement_step(
            &mut map,
            &[intent(1, 0, 1, 20), intent(2, 1, 2, 20)],
            &MovementConfig::default(),
            &LogisticsConfig::default(),
        )
        .unwrap();
        assert_eq!(result.outcomes[&1].approved, 0);
        assert_eq!(result.outcomes[&2].approved, 0);
        assert_eq!(map.total_force(), 300);
    }

    #[test]
    fn service_is_deterministic_under_input_permutation() {
        let setup = || {
            let mut map = HexMap::new();
            for coordinate in [Axial::new(-1, 0), Axial::ZERO, Axial::new(1, 0)] {
                map.insert(Cell::ground(coordinate, 0, Some(1), 100));
            }
            map.get_mut(Axial::new(-1, 0)).unwrap().forces = ForceComposition::infantry(50);
            map.get_mut(Axial::new(1, 0)).unwrap().forces = ForceComposition::infantry(50);
            map
        };
        let left = MovementIntent {
            id: 2,
            priority: 0,
            owner: 1,
            from: Axial::new(-1, 0),
            to: Axial::ZERO,
            requested: 50,
        };
        let right = MovementIntent {
            id: 1,
            priority: 0,
            owner: 1,
            from: Axial::new(1, 0),
            to: Axial::ZERO,
            requested: 50,
        };
        let mut first_map = setup();
        let first = movement_step(
            &mut first_map,
            &[left, right],
            &MovementConfig::default(),
            &LogisticsConfig::default(),
        );
        let mut second_map = setup();
        let second = movement_step(
            &mut second_map,
            &[right, left],
            &MovementConfig::default(),
            &LogisticsConfig::default(),
        );
        assert_eq!(first, second);
        assert_eq!(first_map, second_map);
    }

    #[test]
    fn generated_steps_preserve_capacity_and_total_strength() {
        // A tiny deterministic LCG supplies broad state coverage without making
        // authoritative code or tests depend on an RNG crate.
        let mut state = 0x4d59_5df4_d0f3_3173_u64;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            state
        };

        for _case in 0..200 {
            let mut map = owned_line(4, 100);
            for q in 0..4 {
                map.get_mut(Axial::new(q, 0)).unwrap().forces =
                    ForceComposition::infantry(next() % 101);
            }
            let before = map.total_force();
            let intents = [
                intent(1, 0, 1, next() % 80),
                intent(2, 1, 2, next() % 80),
                intent(3, 2, 3, next() % 80),
                intent(4, 3, 2, next() % 80),
            ];
            let step = movement_step(
                &mut map,
                &intents,
                &MovementConfig::default(),
                &LogisticsConfig::default(),
            )
            .unwrap();
            assert_eq!(step.strength_before, before);
            assert_eq!(step.strength_after, before);
            assert!(
                map.cells()
                    .all(|cell| cell.force() <= cell.military_capacity)
            );
        }
    }

    #[test]
    fn planner_matches_the_legacy_per_pair_search_on_a_nontrivial_map() {
        use crate::pathfinding::shortest_path;

        // Two disjoint clusters of sources/destinations joined by a ridge so
        // uphill/downhill asymmetry, unequal route costs, and capacity limits
        // all participate in the allocation order.
        let mut map = HexMap::new();
        for q in 0..6 {
            for r in 0..3 {
                map.insert(Cell::ground(Axial::new(q, r), 0, Some(1), 100));
            }
        }
        map.get_mut(Axial::new(2, 1)).unwrap().elevation = 1;
        map.get_mut(Axial::new(3, 0)).unwrap().elevation = 1;
        map.get_mut(Axial::new(3, 2)).unwrap().elevation = 2;
        map.get_mut(Axial::new(0, 0)).unwrap().forces = ForceComposition::infantry(60);
        map.get_mut(Axial::new(0, 2)).unwrap().forces = ForceComposition::infantry(40);
        map.get_mut(Axial::new(1, 1)).unwrap().forces = ForceComposition::infantry(70);
        map.get_mut(Axial::new(5, 0)).unwrap().forces = ForceComposition::infantry(55);
        let request = TransferRequest {
            owner: 1,
            sources: vec![
                Axial::new(0, 0),
                Axial::new(0, 2),
                Axial::new(1, 1),
                Axial::new(5, 0),
            ],
            destinations: vec![Axial::new(4, 1), Axial::new(5, 2), Axial::new(2, 0)],
            amount: 200,
        };
        let movement = MovementConfig::default();

        let plan = plan_transfer(&map, &request, &movement).unwrap();

        // Legacy reference: one A* per pair, sorted and allocated identically.
        let mut candidates = Vec::new();
        for &source in &request.sources {
            for &destination in &request.destinations {
                if let Some(path) = shortest_path(&map, source, destination, &movement, |cell| {
                    cell.owner == Some(request.owner)
                }) {
                    candidates.push((path.total_cost, source, destination));
                }
            }
        }
        candidates.sort_unstable();
        let mut source_available: BTreeMap<_, _> = request
            .sources
            .iter()
            .map(|&source| (source, map.get(source).unwrap().force()))
            .collect();
        let mut destination_available: BTreeMap<_, _> = request
            .destinations
            .iter()
            .map(|&destination| {
                (
                    destination,
                    map.get(destination).unwrap().free_military_capacity(),
                )
            })
            .collect();
        let mut remaining = request.amount;
        let mut expected_legs = Vec::new();
        for (total_cost, source, destination) in candidates {
            if remaining == 0 {
                break;
            }
            let amount = remaining
                .min(source_available[&source])
                .min(destination_available[&destination]);
            if amount == 0 {
                continue;
            }
            *source_available.get_mut(&source).unwrap() -= amount;
            *destination_available.get_mut(&destination).unwrap() -= amount;
            remaining -= amount;
            expected_legs.push((source, destination, amount, total_cost));
        }

        assert!(!expected_legs.is_empty());
        assert_eq!(
            plan.legs
                .iter()
                .map(|leg| (leg.source, leg.destination, leg.amount, leg.path.total_cost))
                .collect::<Vec<_>>(),
            expected_legs
        );
        assert_eq!(plan.unplanned, remaining);
        // Every planned route is a valid owned walk whose cost matches the
        // reported total.
        for leg in &plan.legs {
            assert_eq!(leg.path.cells.first(), Some(&leg.source));
            assert_eq!(leg.path.cells.last(), Some(&leg.destination));
            let mut walked = 0_u64;
            for pair in leg.path.cells.windows(2) {
                let from = map.get(pair[0]).unwrap();
                let to = map.get(pair[1]).unwrap();
                assert_eq!(pair[0].distance(pair[1]), 1);
                assert_eq!(to.owner, Some(1));
                walked += u64::from(ground_traversal(from, to, &movement).unwrap().cost);
            }
            assert_eq!(walked, leg.path.total_cost);
        }
    }

    #[test]
    fn over_capacity_cell_elsewhere_does_not_block_unrelated_movement() {
        let mut map = owned_line(4, 100);
        map.get_mut(Axial::new(0, 0)).unwrap().forces = ForceComposition::infantry(50);
        // Pre-existing inconsistency far from the moving units.
        map.get_mut(Axial::new(3, 0)).unwrap().forces = ForceComposition::infantry(150);
        let logistics = LogisticsConfig {
            default_edge_throughput: 40,
            ..LogisticsConfig::default()
        };
        let result = movement_step(
            &mut map,
            &[intent(1, 0, 1, 30)],
            &MovementConfig::default(),
            &logistics,
        )
        .unwrap();
        assert_eq!(result.outcomes[&1].approved, 30);
        assert_eq!(map.get(Axial::new(1, 0)).unwrap().force(), 30);
        assert_eq!(result.strength_before, result.strength_after);
        // The distant over-capacity cell is untouched.
        assert_eq!(map.get(Axial::new(3, 0)).unwrap().force(), 150);
    }

    #[test]
    fn over_capacity_cell_may_drain_but_never_gains() {
        let build = || {
            let mut map = owned_line(3, 100);
            map.get_mut(Axial::new(0, 0)).unwrap().forces = ForceComposition::infantry(50);
            map.get_mut(Axial::new(1, 0)).unwrap().forces = ForceComposition::infantry(150);
            map
        };
        let logistics = LogisticsConfig {
            default_edge_throughput: 40,
            ..LogisticsConfig::default()
        };

        let mut gaining = build();
        let result = movement_step(
            &mut gaining,
            &[intent(1, 0, 1, 30)],
            &MovementConfig::default(),
            &logistics,
        )
        .unwrap();
        assert_eq!(result.outcomes[&1].approved, 0);
        assert!(
            result.outcomes[&1]
                .limits
                .contains(&MovementLimit::DestinationCapacity)
        );
        assert_eq!(gaining.get(Axial::new(1, 0)).unwrap().force(), 150);

        let mut draining = build();
        let result = movement_step(
            &mut draining,
            &[intent(1, 1, 2, 30)],
            &MovementConfig::default(),
            &logistics,
        )
        .unwrap();
        assert_eq!(result.outcomes[&1].approved, 30);
        assert_eq!(draining.get(Axial::new(1, 0)).unwrap().force(), 120);
        assert_eq!(draining.get(Axial::new(2, 0)).unwrap().force(), 30);
    }

    #[test]
    fn duplicate_ids_fail_without_mutating_the_map() {
        let mut map = owned_line(2, 100);
        map.get_mut(Axial::new(0, 0)).unwrap().forces = ForceComposition::infantry(50);
        let before = map.clone();
        let duplicate = intent(1, 0, 1, 10);
        assert_eq!(
            movement_step(
                &mut map,
                &[duplicate, duplicate],
                &MovementConfig::default(),
                &LogisticsConfig::default()
            ),
            Err(MovementError::DuplicateIntentId(1))
        );
        assert_eq!(map, before);
    }
}
