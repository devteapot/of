use std::{
    borrow::Borrow,
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, BinaryHeap},
};

use crate::Axial;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DirectedFrontEdge {
    pub source: Axial,
    pub target: Axial,
}

/// One source's deterministic route to a local contact edge.
///
/// `cells` contains the complete forward route: the selected source, zero or
/// more selected interior cells, `edge.source`, and finally the unselected
/// `edge.target`. `interior_cost` covers only traversal inside the selected
/// region; callers can add the final contact step explicitly for ETA because
/// the supplied front edge is already considered eligible.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalFrontRoute {
    pub edge: DirectedFrontEdge,
    pub cells: Vec<Axial>,
    pub interior_cost: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrontSelectionError {
    EmptySelection,
    InvalidDirection,
    NoEligibleFront,
}

/// Derives the exact outward-facing edge set for every selected source region.
///
/// Only the chosen axial direction is considered. This is important for deep
/// selections: cells painted backward supply strength and routes, but their
/// side edges do not silently become additional fronts. Disconnected source
/// regions are intentional: each region contributes its own eligible boundary.
pub fn selected_front_edges(
    sources: &BTreeSet<Axial>,
    direction: Axial,
    mut target_is_eligible: impl FnMut(Axial, Axial) -> bool,
) -> Result<Vec<DirectedFrontEdge>, FrontSelectionError> {
    if sources.is_empty() {
        return Err(FrontSelectionError::EmptySelection);
    }
    if !Axial::DIRECTIONS.contains(&direction) {
        return Err(FrontSelectionError::InvalidDirection);
    }

    let edges = sources
        .iter()
        .filter_map(|source| {
            let target = *source + direction;
            (!sources.contains(&target) && target_is_eligible(*source, target)).then_some(
                DirectedFrontEdge {
                    source: *source,
                    target,
                },
            )
        })
        .collect::<Vec<_>>();
    if edges.is_empty() {
        return Err(FrontSelectionError::NoEligibleFront);
    }
    Ok(edges)
}

/// Routes each selected source straight along the commanded direction to an
/// eligible boundary. Sources do not drift sideways into a different lane.
pub fn selected_directional_routes(
    sources: &BTreeSet<Axial>,
    direction: Axial,
    front_sources: &BTreeSet<Axial>,
    mut can_traverse: impl FnMut(Axial, Axial) -> bool,
) -> BTreeMap<Axial, Vec<Axial>> {
    if !Axial::DIRECTIONS.contains(&direction) {
        return BTreeMap::new();
    }

    sources
        .iter()
        .filter_map(|&source| {
            let mut current = source;
            let mut route = vec![source];
            while !front_sources.contains(&current) {
                let next = current + direction;
                if !sources.contains(&next) || !can_traverse(current, next) {
                    return None;
                }
                route.push(next);
                current = next;
            }
            Some((source, route))
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LocalFrontLabel {
    cost: u64,
    boundary: Axial,
    next: Option<Axial>,
}

impl LocalFrontLabel {
    const fn search_key(self) -> (u64, Axial) {
        (self.cost, self.boundary)
    }

    const fn route_key(self) -> (u64, Axial, Option<Axial>) {
        (self.cost, self.boundary, self.next)
    }
}

/// Assigns every reachable selected source to exactly one local contact edge.
///
/// Valid front edges start at a selected coordinate, end at an unselected
/// adjacent coordinate, and use one of the six axial directions. Malformed and
/// duplicate edges are ignored. A reverse multi-source Dijkstra search grows
/// inward from the remaining boundary edges. The callback is evaluated in the
/// eventual forward direction (`neighbor -> current`), which preserves
/// asymmetric uphill/downhill costs. `None` and zero-cost steps are treated as
/// non-traversable.
///
/// Ties are resolved by `(cost, boundary, next cell)`. Sources are assigned to
/// their nearest boundary coordinate first; the sorted sources feeding each
/// boundary are then distributed round-robin over that boundary's sorted
/// eligible edges. This activates corner arcs fairly without assigning any
/// source twice, while distinct boundary coordinates may still enter the same
/// outside target from different local normals. Disconnected cells with no
/// reachable front are omitted.
pub fn selected_local_front_routes<S, E, F>(
    selected_coordinates: S,
    eligible_edges: E,
    mut traversal_cost: F,
) -> BTreeMap<Axial, LocalFrontRoute>
where
    S: IntoIterator,
    S::Item: Borrow<Axial>,
    E: IntoIterator,
    E::Item: Borrow<DirectedFrontEdge>,
    F: FnMut(Axial, Axial) -> Option<u64>,
{
    let selected = selected_coordinates
        .into_iter()
        .map(|coordinate| *coordinate.borrow())
        .collect::<BTreeSet<_>>();
    if selected.is_empty() {
        return BTreeMap::new();
    }

    let edges_by_boundary = eligible_edges
        .into_iter()
        .map(|edge| *edge.borrow())
        .filter(|edge| {
            selected.contains(&edge.source)
                && !selected.contains(&edge.target)
                && Axial::DIRECTIONS.contains(&(edge.target - edge.source))
        })
        .fold(
            BTreeMap::<Axial, BTreeMap<Axial, DirectedFrontEdge>>::new(),
            |mut grouped, edge| {
                grouped
                    .entry(edge.source)
                    .or_default()
                    .insert(edge.target, edge);
                grouped
            },
        )
        .into_iter()
        .map(|(boundary, edges)| (boundary, edges.into_values().collect::<Vec<_>>()))
        .collect::<BTreeMap<_, _>>();
    if edges_by_boundary.is_empty() {
        return BTreeMap::new();
    }

    let mut labels = BTreeMap::<Axial, LocalFrontLabel>::new();
    let mut pending = BinaryHeap::new();
    for &boundary in edges_by_boundary.keys() {
        let candidate = LocalFrontLabel {
            cost: 0,
            boundary,
            next: None,
        };
        labels.insert(boundary, candidate);
        pending.push(Reverse((0_u64, boundary, boundary)));
    }

    while let Some(Reverse((cost, boundary, current))) = pending.pop() {
        let Some(label) = labels.get(&current).copied() else {
            continue;
        };
        if label.search_key() != (cost, boundary) {
            continue;
        }

        let mut neighbors = current.neighbors();
        neighbors.sort_unstable();
        for neighbor in neighbors {
            if !selected.contains(&neighbor) {
                continue;
            }
            let Some(step_cost) = traversal_cost(neighbor, current).filter(|cost| *cost > 0) else {
                continue;
            };
            let Some(candidate_cost) = cost.checked_add(step_cost) else {
                continue;
            };
            let candidate = LocalFrontLabel {
                cost: candidate_cost,
                boundary: label.boundary,
                next: Some(current),
            };
            let current_label = labels.get(&neighbor).copied();
            if current_label.is_some_and(|existing| existing.route_key() <= candidate.route_key()) {
                continue;
            }
            let search_changed = current_label
                .is_none_or(|existing| existing.search_key() != candidate.search_key());
            labels.insert(neighbor, candidate);
            if search_changed {
                pending.push(Reverse((candidate.cost, candidate.boundary, neighbor)));
            }
        }
    }

    let mut routes_by_boundary = BTreeMap::<Axial, Vec<(Axial, Vec<Axial>, u64)>>::new();
    for (&source, &label) in &labels {
        let route = (|| {
            let mut cells = vec![source];
            let mut current = source;
            while let Some(next) = labels.get(&current)?.next {
                cells.push(next);
                current = next;
                if cells.len() > selected.len() {
                    return None;
                }
            }
            (current == label.boundary).then_some(cells)
        })();
        if let Some(cells) = route {
            routes_by_boundary
                .entry(label.boundary)
                .or_default()
                .push((source, cells, label.cost));
        }
    }

    let mut routes = BTreeMap::new();
    for (boundary, mut candidates) in routes_by_boundary {
        candidates.sort_unstable_by_key(|(source, _, _)| *source);
        let edges = &edges_by_boundary[&boundary];
        for (index, (source, mut cells, interior_cost)) in candidates.into_iter().enumerate() {
            let edge = edges[index % edges.len()];
            cells.push(edge.target);
            routes.insert(
                source,
                LocalFrontRoute {
                    edge,
                    cells,
                    interior_cost,
                },
            );
        }
    }
    routes
}

/// Derives every eligible outward edge around all selected source regions.
///
/// Unlike [`selected_front_edges`], this operation has no orientation and
/// considers all six sides. Both the source selection and its active boundary
/// may contain disconnected regions; callers route each region independently.
/// Callers decide eligibility, which lets the authoritative simulation restrict
/// expansion to neutral, passable, capturable ground without coupling this pure
/// crate to a particular ownership policy.
pub fn selected_all_front_edges(
    sources: &BTreeSet<Axial>,
    mut target_is_eligible: impl FnMut(Axial, Axial) -> bool,
) -> Result<Vec<DirectedFrontEdge>, FrontSelectionError> {
    if sources.is_empty() {
        return Err(FrontSelectionError::EmptySelection);
    }

    let mut edges = Vec::new();
    for &source in sources {
        for direction in Axial::DIRECTIONS {
            let target = source + direction;
            if !sources.contains(&target) && target_is_eligible(source, target) {
                edges.push(DirectedFrontEdge { source, target });
            }
        }
    }
    if edges.is_empty() {
        return Err(FrontSelectionError::NoEligibleFront);
    }
    edges.sort_unstable_by_key(|edge| (edge.source, edge.target));
    Ok(edges)
}

/// Keeps one deterministic incoming lane for every target cell.
///
/// Concave selections can expose the same outside hex from more than one
/// boundary cell. A stable target anchor must identify exactly one lane, so the
/// lowest source coordinate wins after sorting by `(target, source)`.
pub fn unique_target_front_edges(edges: &[DirectedFrontEdge]) -> Vec<DirectedFrontEdge> {
    let mut candidates = edges.to_vec();
    candidates.sort_unstable_by_key(|edge| (edge.target, edge.source));
    candidates.dedup_by_key(|edge| edge.target);
    candidates.sort_unstable_by_key(|edge| (edge.source, edge.target));
    candidates
}

/// One strategic perimeter arc of an owned component.
///
/// `opponent == None` marks an all-neutral front. `Some(player)` marks a front
/// that faces that opponent, including any neutral boundary edges that only
/// bridge hostile runs against the same opponent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StrategicFront {
    pub opponent: Option<u32>,
    /// Boundary edges sorted by `(source, target)`.
    pub edges: Vec<DirectedFrontEdge>,
}

impl StrategicFront {
    /// Owned source cells that expose at least one edge of this front.
    pub fn source_cells(&self) -> BTreeSet<Axial> {
        self.edges.iter().map(|edge| edge.source).collect()
    }

    /// Number of exposed directed edges leaving each source cell.
    pub fn edge_count_by_source(&self) -> BTreeMap<Axial, u32> {
        let mut counts = BTreeMap::<Axial, u32>::new();
        for edge in &self.edges {
            *counts.entry(edge.source).or_insert(0) += 1;
        }
        counts
    }

    pub fn contains_source(&self, seed: Axial) -> bool {
        self.edges.iter().any(|edge| edge.source == seed)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StrategicFrontError {
    EmptyComponent,
}

/// Classification of one cell adjacent to an owned component boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StrategicExterior {
    /// Off-map or impassable terrain: not a selectable or deployable front.
    Ignored,
    /// Unclaimed passable terrain.
    Neutral,
    /// Passable terrain held by another player.
    Opponent(u32),
}

/// Derives deterministic strategic fronts for one owned traversable component.
///
/// # Inputs
/// - `component`: cells of exactly one owned connected component. Connectivity
///   is the caller's responsibility; this function only inspects membership.
/// - `exterior(source, target)`: classifies a non-component neighbor as
///   ignored, neutral, or hostile. Same-owner and non-traversable edges should
///   be ignored by callers.
///
/// # Output
/// Fronts sorted by `(opponent.is_some(), opponent, min edge)`. Edges inside
/// each front are sorted by `(source, target)`. No durable front IDs are
/// assigned.
///
/// # Topology rules
/// 1. Boundary edges are component → non-component directed axial edges.
/// 2. Complete geometric perimeter cycles are walked with the component on the
///    left using axial `DIRECTIONS` order, **including** ignored half-edges as
///    markers. Holes yield additional cycles; each is independent.
/// 3. Ignored runs split a cycle into maximal active open chains. A cycle with
///    no ignored edges is processed as one closed ring. Ignored edges never
///    appear in resulting fronts.
/// 4. Within a closed cycle or open chain, maximal same-label active runs are
///    formed (`None` = neutral, `Some(id)` = hostile).
/// 5. A neutral run bridges two hostile runs against the **same** opponent into
///    one front (local H–Neutral–H merge). This applies to open chains and
///    closed cycles; first/last wrap-merge and cyclic triple merge apply only
///    to closed cycles. Different opponents always split fronts; neutrals
///    strictly between different opponents remain their own neutral front.
/// 6. A single hostile run does **not** absorb adjacent neutrals that are not
///    between two hostile runs of that opponent. Those neutrals stay neutral
///    fronts, so a blob contacting one enemy still exposes a rebalanceable
///    neutral arc on the far side.
/// 7. When two or more same-opponent hostile runs exist on one **closed** cycle,
///    every neutral run on that cycle that never touches another opponent is
///    treated as a bridge (including the long way around). Open chains do not
///    apply this whole-cycle absorption.
/// 8. Neutral edges are grouped by their ordered hostile context on each
///    geometric perimeter cycle. Sections bounded by different hostile fronts
///    remain independent; sections interrupted only by repeated contact with
///    the same hostile front stay one neutral front. Ignored markers do not by
///    themselves create extra neutral fronts.
/// 9. Neutral edges used to bridge a hostile front remain in their neutral
///    front too, so fronts may overlap at edges and source cells.
/// 10. An all-neutral perimeter cycle becomes one neutral front.
/// 11. Ambiguous/missing exterior is neutral (`None`).
pub fn strategic_fronts<I, F>(
    component: I,
    mut exterior_owner: F,
) -> Result<Vec<StrategicFront>, StrategicFrontError>
where
    I: IntoIterator<Item = Axial>,
    F: FnMut(Axial, Axial) -> StrategicExterior,
{
    let component = component.into_iter().collect::<BTreeSet<_>>();
    if component.is_empty() {
        return Err(StrategicFrontError::EmptyComponent);
    }

    // Include ignored half-edges as walk markers so a single geometric perimeter
    // is not split at an arbitrary BTree start when impassable edges exist.
    let mut half_edges = Vec::<(Axial, usize, BoundaryMark)>::new();
    for &source in &component {
        for (dir, step) in Axial::DIRECTIONS.iter().copied().enumerate() {
            let target = source + step;
            if component.contains(&target) {
                continue;
            }
            let mark = match exterior_owner(source, target) {
                StrategicExterior::Ignored => BoundaryMark::Ignored,
                StrategicExterior::Neutral => BoundaryMark::Active(None),
                StrategicExterior::Opponent(player_id) => BoundaryMark::Active(Some(player_id)),
            };
            half_edges.push((source, dir, mark));
        }
    }
    if half_edges.is_empty() {
        return Ok(Vec::new());
    }
    half_edges.sort_unstable();

    let mut pending = half_edges
        .iter()
        .map(|&(source, dir, _)| (source, dir))
        .collect::<BTreeSet<_>>();
    let mark_by_half = half_edges
        .into_iter()
        .map(|(source, dir, mark)| ((source, dir), mark))
        .collect::<BTreeMap<_, _>>();

    let mut fronts = Vec::new();
    while let Some(&(start_source, start_dir)) = pending.iter().next() {
        let mut cycle = Vec::new();
        let mut source = start_source;
        let mut dir = start_dir;
        loop {
            if !pending.remove(&(source, dir)) {
                break;
            }
            let target = source + Axial::DIRECTIONS[dir];
            let mark = mark_by_half[&(source, dir)];
            cycle.push((DirectedFrontEdge { source, target }, mark));
            let (next_source, next_dir) = next_outline_half_edge(source, dir, &component);
            source = next_source;
            dir = next_dir;
            if source == start_source && dir == start_dir {
                break;
            }
            // Bound walk length on malformed adjacency.
            if cycle.len() > mark_by_half.len() {
                break;
            }
        }
        fronts.extend(fronts_from_marked_cycle(&cycle));
    }

    for front in &mut fronts {
        front
            .edges
            .sort_unstable_by_key(|edge| (edge.source, edge.target));
        front.edges.dedup();
    }
    fronts.retain(|front| !front.edges.is_empty());
    fronts.sort_unstable_by(|left, right| {
        front_sort_key(left)
            .cmp(&front_sort_key(right))
            .then_with(|| left.edges.cmp(&right.edges))
    });
    Ok(fronts)
}

fn front_sort_key(front: &StrategicFront) -> (u8, u32, Axial, Axial) {
    let min_edge = front.edges.first().copied().unwrap_or(DirectedFrontEdge {
        source: Axial::ZERO,
        target: Axial::ZERO,
    });
    (
        u8::from(front.opponent.is_none()),
        front.opponent.unwrap_or(0),
        min_edge.source,
        min_edge.target,
    )
}

/// Next outline half-edge walking clockwise around the component (interior on
/// the right). `dir` is the outward axial direction index from `cell`.
fn next_outline_half_edge(
    mut cell: Axial,
    mut dir: usize,
    component: &BTreeSet<Axial>,
) -> (Axial, usize) {
    for _ in 0..8 {
        dir = (dir + 1) % 6;
        let neighbor = cell + Axial::DIRECTIONS[dir];
        if !component.contains(&neighbor) {
            return (cell, dir);
        }
        // Pivot into the interior neighbor and continue rotating from the
        // reverse of the edge just crossed.
        cell = neighbor;
        dir = (dir + 3) % 6;
    }
    (cell, dir)
}

/// Perimeter half-edge label used while walking complete geometric cycles.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum BoundaryMark {
    /// Off-map / impassable: splits active chains; never emitted as a front edge.
    Ignored,
    /// Deployable exterior: `None` = neutral, `Some(id)` = hostile owner.
    Active(Option<u32>),
}

#[derive(Clone, Debug)]
struct BoundaryRun {
    opponent: Option<u32>,
    edges: Vec<DirectedFrontEdge>,
}

/// Split a marked geometric cycle into closed/open active sequences and derive fronts.
fn fronts_from_marked_cycle(cycle: &[(DirectedFrontEdge, BoundaryMark)]) -> Vec<StrategicFront> {
    if cycle.is_empty()
        || cycle
            .iter()
            .all(|(_, mark)| matches!(mark, BoundaryMark::Ignored))
    {
        return Vec::new();
    }

    // Neutral topology is based on the complete geometric cycle rather than on
    // deployability-chain cuts. This keeps one neutral section intact across an
    // ignored marker while distinct ordered hostile boundaries still section it.
    let mut fronts = neutral_fronts_from_marked_cycle(cycle);
    let mut append_hostile = |derived: Vec<StrategicFront>| {
        fronts.extend(derived.into_iter().filter(|front| front.opponent.is_some()));
    };

    let has_ignored = cycle
        .iter()
        .any(|(_, mark)| matches!(mark, BoundaryMark::Ignored));
    if !has_ignored {
        let active = cycle
            .iter()
            .filter_map(|(edge, mark)| match mark {
                BoundaryMark::Active(owner) => Some((*edge, *owner)),
                BoundaryMark::Ignored => None,
            })
            .collect::<Vec<_>>();
        append_hostile(fronts_from_active_sequence(&active, true));
        return fronts;
    }

    // Rotate so the cycle begins at an ignored edge; then each maximal active
    // run between ignored runs is one open chain in geometric order.
    let Some(start) = cycle
        .iter()
        .position(|(_, mark)| matches!(mark, BoundaryMark::Ignored))
    else {
        return fronts;
    };
    let mut chain = Vec::new();
    for offset in 0..cycle.len() {
        let (edge, mark) = cycle[(start + offset) % cycle.len()];
        match mark {
            BoundaryMark::Ignored => {
                if !chain.is_empty() {
                    append_hostile(fronts_from_active_sequence(&chain, false));
                    chain.clear();
                }
            }
            BoundaryMark::Active(owner) => chain.push((edge, owner)),
        }
    }
    if !chain.is_empty() {
        append_hostile(fronts_from_active_sequence(&chain, false));
    }
    fronts
}

/// Groups neutral edges by the hostile fronts encountered immediately before
/// and after them around one geometric perimeter cycle.
///
/// The ordered pair matters: on a ring containing hostile fronts A and B, the
/// neutral A→B section and the neutral B→A section are independent. Repeated
/// A→A sections coalesce and may overlap the hostile A front as bridge edges.
fn neutral_fronts_from_marked_cycle(
    cycle: &[(DirectedFrontEdge, BoundaryMark)],
) -> Vec<StrategicFront> {
    let hostile_positions = cycle
        .iter()
        .enumerate()
        .filter_map(|(index, (_, mark))| match mark {
            BoundaryMark::Active(Some(opponent)) => Some((index, *opponent)),
            BoundaryMark::Ignored | BoundaryMark::Active(None) => None,
        })
        .collect::<Vec<_>>();
    let neutral_edges = cycle
        .iter()
        .filter_map(|(edge, mark)| matches!(mark, BoundaryMark::Active(None)).then_some(*edge))
        .collect::<Vec<_>>();
    if neutral_edges.is_empty() {
        return Vec::new();
    }
    if hostile_positions.is_empty() {
        return vec![StrategicFront {
            opponent: None,
            edges: neutral_edges,
        }];
    }

    let mut edges_by_context = BTreeMap::<(u32, u32), Vec<DirectedFrontEdge>>::new();
    for (index, (edge, mark)) in cycle.iter().enumerate() {
        if !matches!(mark, BoundaryMark::Active(None)) {
            continue;
        }
        let next_at = hostile_positions.partition_point(|(position, _)| *position <= index);
        let previous_at = hostile_positions.partition_point(|(position, _)| *position < index);
        let previous = if previous_at == 0 {
            hostile_positions
                .last()
                .expect("hostile positions is nonempty")
                .1
        } else {
            hostile_positions[previous_at - 1].1
        };
        let next = if next_at == hostile_positions.len() {
            hostile_positions[0].1
        } else {
            hostile_positions[next_at].1
        };
        edges_by_context
            .entry((previous, next))
            .or_default()
            .push(*edge);
    }

    edges_by_context
        .into_values()
        .map(|edges| StrategicFront {
            opponent: None,
            edges,
        })
        .collect()
}

/// Derive fronts from one active perimeter sequence.
///
/// `closed == true` means the sequence is a full ring (first and last edges are
/// adjacent). `closed == false` means an open chain bounded by ignored runs or
/// walk ends: no wrap of first/last runs, no cyclic H–N–H triple, and no
/// whole-cycle long-way neutral absorption.
fn fronts_from_active_sequence(
    sequence: &[(DirectedFrontEdge, Option<u32>)],
    closed: bool,
) -> Vec<StrategicFront> {
    if sequence.is_empty() {
        return Vec::new();
    }
    if sequence.iter().all(|(_, owner)| owner.is_none()) {
        return vec![StrategicFront {
            opponent: None,
            edges: sequence.iter().map(|(edge, _)| *edge).collect(),
        }];
    }

    let mut runs = Vec::<BoundaryRun>::new();
    for &(edge, owner) in sequence {
        match runs.last_mut() {
            Some(run) if run.opponent == owner => run.edges.push(edge),
            _ => runs.push(BoundaryRun {
                opponent: owner,
                edges: vec![edge],
            }),
        }
    }
    // Only closed rings may join first/last runs across the seam.
    if closed
        && runs.len() > 1
        && runs.first().is_some_and(|run| {
            runs.last()
                .is_some_and(|last| last.opponent == run.opponent)
        })
    {
        let mut first = runs.remove(0);
        if let Some(last) = runs.last_mut() {
            last.edges.append(&mut first.edges);
        }
    }

    let initial_hostile_runs_by_opponent = {
        let mut counts = BTreeMap::<u32, usize>::new();
        for run in &runs {
            if let Some(opponent) = run.opponent {
                *counts.entry(opponent).or_insert(0) += 1;
            }
        }
        counts
    };

    while let Some(merge_at) = (0..runs.len()).find(|&index| {
        if runs.len() < 3 {
            return false;
        }
        let linear = index + 2 < runs.len();
        // Cyclic H–N–H across the seam is only meaningful on a closed ring.
        let cyclic = closed && index + 2 >= runs.len();
        if !linear && !cyclic {
            return false;
        }
        let next = (index + 1) % runs.len();
        let after = (index + 2) % runs.len();
        if next == index || after == index {
            return false;
        }
        runs[index].opponent.is_some()
            && runs[next].opponent.is_none()
            && runs[after].opponent == runs[index].opponent
    }) {
        let next = (merge_at + 1) % runs.len();
        let after = (merge_at + 2) % runs.len();
        let mut merged_edges = runs[merge_at].edges.clone();
        merged_edges.extend(runs[next].edges.iter().copied());
        merged_edges.extend(runs[after].edges.iter().copied());
        let opponent = runs[merge_at].opponent;

        // Remove the three runs carefully under modular indices.
        let mut remove = [merge_at, next, after];
        remove.sort_unstable();
        for &index in remove.iter().rev() {
            runs.remove(index);
        }
        let insert_at = remove[0];
        runs.insert(
            insert_at,
            BoundaryRun {
                opponent,
                edges: merged_edges,
            },
        );
    }

    // When two+ same-opponent hostile runs began on a closed cycle, leftover pure
    // neutral runs only touch that opponent and are bridges the long way around.
    // Open chains must not absorb dangling neutral ends this way.
    if closed
        && runs.iter().filter(|run| run.opponent.is_some()).count() == 1
        && let Some(opponent) = runs.iter().find_map(|run| run.opponent)
        && initial_hostile_runs_by_opponent
            .get(&opponent)
            .copied()
            .unwrap_or(0)
            >= 2
        && runs
            .iter()
            .all(|run| run.opponent.is_none() || run.opponent == Some(opponent))
    {
        let mut hostile_edges = Vec::new();
        let mut absorbed_neutrals = Vec::<DirectedFrontEdge>::new();
        for run in runs.drain(..) {
            if run.opponent.is_some() {
                hostile_edges.extend(run.edges);
            } else {
                absorbed_neutrals.extend(run.edges);
            }
        }
        hostile_edges.extend(absorbed_neutrals);
        return vec![StrategicFront {
            opponent: Some(opponent),
            edges: hostile_edges,
        }];
    }

    runs.into_iter()
        .map(|run| StrategicFront {
            opponent: run.opponent,
            edges: run.edges,
        })
        .collect()
}

/// Finds the front whose edge sources include `seed`.
pub fn strategic_front_index_for_seed(fronts: &[StrategicFront], seed: Axial) -> Option<usize> {
    fronts.iter().position(|front| front.contains_source(seed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_cells_feed_the_front_without_exposing_their_sides() {
        let sources = BTreeSet::from([Axial::new(0, 0), Axial::new(1, 0), Axial::new(2, 0)]);
        let edges = selected_front_edges(&sources, Axial::new(1, 0), |_, _| true)
            .expect("the line has one east-facing front");

        assert_eq!(
            edges,
            vec![DirectedFrontEdge {
                source: Axial::new(2, 0),
                target: Axial::new(3, 0),
            }]
        );
    }

    #[test]
    fn directional_routes_never_feed_a_side_lane() {
        let sources = BTreeSet::from([
            Axial::new(0, -1),
            Axial::new(0, 0),
            Axial::new(0, 1),
            Axial::new(1, -1),
            Axial::new(1, 0),
            Axial::new(1, 1),
            Axial::new(2, -1),
            Axial::new(2, 0),
            Axial::new(2, 1),
        ]);
        let fronts = BTreeSet::from([Axial::new(2, -1), Axial::new(2, 0), Axial::new(2, 1)]);

        let routes = selected_directional_routes(&sources, Axial::new(1, 0), &fronts, |_, _| true);

        assert_eq!(
            routes[&Axial::new(0, 0)],
            vec![Axial::new(0, 0), Axial::new(1, 0), Axial::new(2, 0)]
        );
        assert!(routes.values().all(|route| {
            route
                .windows(2)
                .all(|edge| edge[1] - edge[0] == Axial::new(1, 0))
        }));
    }

    #[test]
    fn local_routes_keep_all_six_normals_around_an_enclosed_pocket() {
        let pocket = Axial::ZERO;
        let selected = pocket.neighbors().into_iter().collect::<BTreeSet<_>>();
        let edges = selected
            .iter()
            .map(|&source| DirectedFrontEdge {
                source,
                target: pocket,
            })
            .collect::<Vec<_>>();

        let routes = selected_local_front_routes(&selected, &edges, |_, _| Some(1));

        assert_eq!(routes.len(), 6);
        assert_eq!(routes.keys().copied().collect::<BTreeSet<_>>(), selected);
        assert_eq!(
            routes
                .values()
                .map(|route| route.edge.target - route.edge.source)
                .collect::<BTreeSet<_>>(),
            Axial::DIRECTIONS.into_iter().collect()
        );
        for (&source, route) in &routes {
            assert_eq!(route.edge.source, source);
            assert_eq!(route.cells, vec![source, pocket]);
            assert_eq!(route.interior_cost, 0);
        }
    }

    #[test]
    fn deeper_sources_choose_the_nearest_local_arc() {
        let selected = (0..=4).map(|q| Axial::new(q, 0)).collect::<BTreeSet<_>>();
        let west = DirectedFrontEdge {
            source: Axial::new(0, 0),
            target: Axial::new(-1, 0),
        };
        let east = DirectedFrontEdge {
            source: Axial::new(4, 0),
            target: Axial::new(5, 0),
        };

        let routes = selected_local_front_routes(&selected, [east, west], |_, _| Some(1));

        assert_eq!(routes[&Axial::new(0, 0)].edge, west);
        assert_eq!(routes[&Axial::new(1, 0)].edge, west);
        assert_eq!(routes[&Axial::new(2, 0)].edge, west);
        assert_eq!(routes[&Axial::new(2, 0)].interior_cost, 2);
        assert_eq!(routes[&Axial::new(3, 0)].edge, east);
        assert_eq!(routes[&Axial::new(4, 0)].edge, east);
        assert_eq!(
            routes[&Axial::new(2, 0)].cells,
            vec![
                Axial::new(2, 0),
                Axial::new(1, 0),
                Axial::new(0, 0),
                Axial::new(-1, 0),
            ]
        );
    }

    #[test]
    fn local_routes_omit_disconnected_and_cliff_isolated_sources() {
        let rear = Axial::new(0, 0);
        let middle = Axial::new(1, 0);
        let boundary = Axial::new(2, 0);
        let disconnected = Axial::new(10, 0);
        let selected = BTreeSet::from([rear, middle, boundary, disconnected]);
        let edge = DirectedFrontEdge {
            source: boundary,
            target: Axial::new(3, 0),
        };

        let routes = selected_local_front_routes(&selected, [edge], |from, to| {
            if (from, to) == (middle, boundary) {
                Some(7)
            } else {
                None
            }
        });

        assert_eq!(
            routes.keys().copied().collect::<BTreeSet<_>>(),
            BTreeSet::from([middle, boundary])
        );
        assert_eq!(routes[&middle].interior_cost, 7);
        assert_eq!(routes[&middle].cells, vec![middle, boundary, edge.target]);
        assert!(!routes.contains_key(&rear));
        assert!(!routes.contains_key(&disconnected));

        let zero_cost_is_blocked =
            selected_local_front_routes([middle, boundary], [edge], |_, _| Some(0));
        assert_eq!(
            zero_cost_is_blocked.keys().copied().collect::<Vec<_>>(),
            vec![boundary]
        );
    }

    #[test]
    fn local_routes_are_input_order_independent_and_share_corner_edges_fairly() {
        let selected_forward = vec![
            Axial::new(-3, 0),
            Axial::new(-2, 0),
            Axial::new(-1, 0),
            Axial::ZERO,
        ];
        let mut selected_reverse = selected_forward.clone();
        selected_reverse.reverse();
        let boundary = Axial::ZERO;
        let valid_targets = [Axial::new(1, 0), Axial::new(1, -1), Axial::new(0, -1)];
        let valid_edges = valid_targets
            .into_iter()
            .map(|target| DirectedFrontEdge {
                source: boundary,
                target,
            })
            .collect::<Vec<_>>();
        let mut mixed_edges = vec![
            DirectedFrontEdge {
                source: Axial::new(9, 0),
                target: Axial::new(10, 0),
            },
            DirectedFrontEdge {
                source: boundary,
                target: Axial::new(-1, 0),
            },
            DirectedFrontEdge {
                source: boundary,
                target: Axial::new(2, 0),
            },
        ];
        mixed_edges.extend(valid_edges.iter().copied());
        mixed_edges.push(valid_edges[0]);
        let mut reversed_edges = mixed_edges.clone();
        reversed_edges.reverse();

        let forward = selected_local_front_routes(selected_forward, mixed_edges, |_, _| Some(1));
        let reversed =
            selected_local_front_routes(selected_reverse, reversed_edges, |_, _| Some(1));

        assert_eq!(forward, reversed);
        assert_eq!(forward.len(), 4);
        let mut sorted_targets = valid_targets.to_vec();
        sorted_targets.sort_unstable();
        assert_eq!(
            forward
                .values()
                .map(|route| route.edge.target)
                .collect::<Vec<_>>(),
            vec![
                sorted_targets[0],
                sorted_targets[1],
                sorted_targets[2],
                sorted_targets[0],
            ]
        );
        for route in forward.values() {
            let [.., edge_source, edge_target] = route.cells.as_slice() else {
                panic!("a local route must include its final front edge");
            };
            assert_eq!(
                (*edge_source, *edge_target),
                (route.edge.source, route.edge.target)
            );
            assert!(Axial::DIRECTIONS.contains(&(*edge_target - *edge_source)));
        }
    }

    #[test]
    fn a_wide_front_keeps_every_connected_outward_edge() {
        let sources = BTreeSet::from([
            Axial::new(0, -1),
            Axial::new(0, 0),
            Axial::new(0, 1),
            Axial::new(-1, 0),
        ]);
        let edges = selected_front_edges(&sources, Axial::new(1, 0), |_, _| true)
            .expect("the east-facing boundary is connected");

        assert_eq!(edges.len(), 3);
        assert!(
            edges
                .iter()
                .all(|edge| edge.target == edge.source + Axial::new(1, 0))
        );
    }

    #[test]
    fn disconnected_source_regions_each_contribute_a_directional_front() {
        let sources = BTreeSet::from([Axial::ZERO, Axial::new(2, 0)]);
        assert_eq!(
            selected_front_edges(&sources, Axial::new(1, 0), |_, _| true),
            Ok(vec![
                DirectedFrontEdge {
                    source: Axial::ZERO,
                    target: Axial::new(1, 0),
                },
                DirectedFrontEdge {
                    source: Axial::new(2, 0),
                    target: Axial::new(3, 0),
                },
            ])
        );
    }

    #[test]
    fn separated_eligible_arcs_share_one_connected_source_region() {
        let connected_sources =
            BTreeSet::from([Axial::new(0, -1), Axial::new(0, 0), Axial::new(0, 1)]);
        assert_eq!(
            selected_front_edges(&connected_sources, Axial::new(1, 0), |source, _| source.r
                != 0,),
            Ok(vec![
                DirectedFrontEdge {
                    source: Axial::new(0, -1),
                    target: Axial::new(1, -1),
                },
                DirectedFrontEdge {
                    source: Axial::new(0, 1),
                    target: Axial::new(1, 1),
                },
            ])
        );
    }

    #[test]
    fn direction_and_eligibility_are_validated() {
        let source = BTreeSet::from([Axial::ZERO]);
        assert_eq!(
            selected_front_edges(&source, Axial::new(2, 0), |_, _| true),
            Err(FrontSelectionError::InvalidDirection)
        );
        assert_eq!(
            selected_front_edges(&source, Axial::new(1, 0), |_, _| false),
            Err(FrontSelectionError::NoEligibleFront)
        );
    }

    #[test]
    fn all_fronts_include_every_eligible_arc_without_requiring_arc_connectivity() {
        let sources = BTreeSet::from([Axial::new(0, 0), Axial::new(1, 0), Axial::new(2, 0)]);
        let west = Axial::new(-1, 0);
        let east = Axial::new(3, 0);
        let edges =
            selected_all_front_edges(&sources, |_, target| target == west || target == east)
                .expect("opposite boundary arcs are both valid");

        assert_eq!(
            edges,
            vec![
                DirectedFrontEdge {
                    source: Axial::new(0, 0),
                    target: west,
                },
                DirectedFrontEdge {
                    source: Axial::new(2, 0),
                    target: east,
                },
            ]
        );
    }

    #[test]
    fn all_fronts_exclude_selected_neighbors_and_rejected_targets() {
        let sources = BTreeSet::from([Axial::ZERO, Axial::new(1, 0)]);
        let edges = selected_all_front_edges(&sources, |_, target| target.r == -1)
            .expect("the northern perimeter is eligible");

        assert!(edges.iter().all(|edge| !sources.contains(&edge.target)));
        assert!(edges.iter().all(|edge| edge.target.r == -1));
        assert_eq!(edges.len(), 4);
    }

    #[test]
    fn all_fronts_include_disconnected_source_regions() {
        let disconnected = BTreeSet::from([Axial::ZERO, Axial::new(2, 0)]);
        let edges = selected_all_front_edges(&disconnected, |_, _| true)
            .expect("each selected region has an eligible perimeter");
        assert_eq!(edges.len(), 12);
    }

    #[test]
    fn shared_concave_target_gets_one_stable_lane_anchor() {
        let shared = Axial::new(1, 0);
        let high_source = Axial::new(1, -1);
        let low_source = Axial::ZERO;
        let other_target = Axial::new(-1, 0);
        let edges = vec![
            DirectedFrontEdge {
                source: high_source,
                target: shared,
            },
            DirectedFrontEdge {
                source: low_source,
                target: other_target,
            },
            DirectedFrontEdge {
                source: low_source,
                target: shared,
            },
        ];

        assert_eq!(
            unique_target_front_edges(&edges),
            vec![
                DirectedFrontEdge {
                    source: low_source,
                    target: other_target,
                },
                DirectedFrontEdge {
                    source: low_source,
                    target: shared,
                },
            ]
        );
    }

    #[test]
    fn single_hex_all_neutral_is_one_neutral_front() {
        let component = BTreeSet::from([Axial::ZERO]);
        let fronts =
            strategic_fronts(component, |_, _| StrategicExterior::Neutral).expect("fronts");
        assert_eq!(fronts.len(), 1);
        assert_eq!(fronts[0].opponent, None);
        assert_eq!(fronts[0].edges.len(), 6);
    }

    #[test]
    fn one_hostile_contact_keeps_a_separate_neutral_arc() {
        let component = BTreeSet::from([Axial::ZERO]);
        let enemy = Axial::new(1, 0);
        let fronts = strategic_fronts(component, |_, target| {
            if target == enemy {
                StrategicExterior::Opponent(2)
            } else {
                StrategicExterior::Neutral
            }
        })
        .expect("fronts");
        assert_eq!(fronts.len(), 2);
        let hostile = fronts
            .iter()
            .find(|front| front.opponent == Some(2))
            .expect("hostile front");
        let neutral = fronts
            .iter()
            .find(|front| front.opponent.is_none())
            .expect("neutral front");
        assert_eq!(hostile.edges.len(), 1);
        assert_eq!(hostile.edges[0].target, enemy);
        assert_eq!(neutral.edges.len(), 5);
    }

    #[test]
    fn neutral_gap_bridges_same_opponent_hostile_runs() {
        // Line of three: enemy contacts on both ends face player 7.
        let component = BTreeSet::from([Axial::new(0, 0), Axial::new(1, 0), Axial::new(2, 0)]);
        let left = Axial::new(-1, 0);
        let right = Axial::new(3, 0);
        let fronts = strategic_fronts(component, |_, target| {
            if target == left || target == right {
                StrategicExterior::Opponent(7)
            } else {
                StrategicExterior::Neutral
            }
        })
        .expect("fronts");
        let hostile = fronts
            .iter()
            .filter(|front| front.opponent == Some(7))
            .collect::<Vec<_>>();
        assert_eq!(
            hostile.len(),
            1,
            "same-opponent end contacts must bridge through the neutral sides: {fronts:?}"
        );
        assert!(hostile[0].edges.len() >= 2);
        let left_seed = strategic_front_index_for_seed(&fronts, Axial::new(0, 0))
            .expect("left end belongs to a front");
        let right_seed = strategic_front_index_for_seed(&fronts, Axial::new(2, 0))
            .expect("right end belongs to a front");
        assert_eq!(
            left_seed, right_seed,
            "both end contacts must resolve to the same bridged front index"
        );
        assert_eq!(fronts[left_seed].opponent, Some(7));
        let targets = fronts[left_seed]
            .edges
            .iter()
            .map(|edge| edge.target)
            .collect::<BTreeSet<_>>();
        assert!(targets.contains(&left));
        assert!(targets.contains(&right));
        // Closed-cycle long-way absorption must keep neutral bridge edges.
        assert!(
            fronts[left_seed].edges.len() > 2,
            "bridged front must include neutral perimeter edges, got {fronts:?}"
        );
        assert!(
            fronts[left_seed]
                .edges
                .iter()
                .any(|edge| edge.target != left && edge.target != right),
            "bridged front must contain at least one neutral bridge edge: {fronts:?}"
        );
    }

    #[test]
    fn different_opponents_split_fronts_with_neutral_between() {
        let component = BTreeSet::from([Axial::ZERO]);
        let a = Axial::new(1, 0);
        let b = Axial::new(-1, 0);
        let fronts = strategic_fronts(component, |_, target| {
            if target == a {
                StrategicExterior::Opponent(2)
            } else if target == b {
                StrategicExterior::Opponent(3)
            } else {
                StrategicExterior::Neutral
            }
        })
        .expect("fronts");
        assert!(fronts.iter().any(|front| front.opponent == Some(2)));
        assert!(fronts.iter().any(|front| front.opponent == Some(3)));
        let neutral = fronts
            .iter()
            .filter(|front| front.opponent.is_none())
            .collect::<Vec<_>>();
        assert_eq!(
            neutral.len(),
            2,
            "opposite neutral sections bounded by different hostile fronts must stay independent: {fronts:?}"
        );
        assert!(neutral.iter().all(|front| front.edges.len() == 2));
        // Stable sort: numbered opponents before pure neutral.
        assert!(fronts[0].opponent.is_some());
        assert_eq!(fronts.last().map(|front| front.opponent), Some(None));
    }

    #[test]
    fn strategic_fronts_are_input_order_independent() {
        let cells = [Axial::new(0, 0), Axial::new(1, 0), Axial::new(0, 1)];
        let forward = strategic_fronts(cells, |_, _| StrategicExterior::Neutral).expect("forward");
        let reverse = strategic_fronts(cells.into_iter().rev(), |_, _| StrategicExterior::Neutral)
            .expect("reverse");
        assert_eq!(forward, reverse);
    }

    #[test]
    fn ignored_exterior_edges_are_not_fronts() {
        let component = BTreeSet::from([Axial::ZERO]);
        let ignored = Axial::new(1, 0);
        let fronts = strategic_fronts(component, |_, target| {
            if target == ignored {
                StrategicExterior::Ignored
            } else {
                StrategicExterior::Neutral
            }
        })
        .expect("fronts");
        let edges = fronts
            .iter()
            .flat_map(|front| front.edges.iter())
            .copied()
            .collect::<BTreeSet<_>>();
        assert_eq!(edges.len(), 5);
        assert!(!edges.iter().any(|edge| edge.target == ignored));
        assert_eq!(
            fronts.len(),
            1,
            "all-neutral active chain split by one ignored edge must stay one front: {fronts:?}"
        );
        assert_eq!(fronts[0].opponent, None);
    }

    #[test]
    fn ignored_edge_elsewhere_keeps_same_opponent_neutral_bridge_one_front() {
        // Single hex, DIRECTIONS order:
        // 0 Neutral, 1 Neutral, 2 Opponent(7), 3 Ignored, 4 Opponent(7), 5 Neutral.
        // BTree walk without ignored markers would split the open chain through the
        // multi-edge neutral gap and emit two opponent fronts; the geometric chain
        // Opp@4 - N@5 - N@0 - N@1 - Opp@2 must stay one bridged front.
        let component = BTreeSet::from([Axial::ZERO]);
        let n0 = Axial::new(1, 0);
        let n1 = Axial::new(1, -1);
        let hostile_a = Axial::new(0, -1);
        let ignored = Axial::new(-1, 0);
        let hostile_b = Axial::new(-1, 1);
        let n5 = Axial::new(0, 1);
        let fronts = strategic_fronts(component, |_, target| {
            if target == ignored {
                StrategicExterior::Ignored
            } else if target == hostile_a || target == hostile_b {
                StrategicExterior::Opponent(7)
            } else {
                StrategicExterior::Neutral
            }
        })
        .expect("fronts");

        let hostile = fronts
            .iter()
            .filter(|front| front.opponent == Some(7))
            .collect::<Vec<_>>();
        assert_eq!(
            hostile.len(),
            1,
            "same-opponent stretches separated by neutrals must bridge despite ignored edge elsewhere: {fronts:?}"
        );
        let targets = hostile[0]
            .edges
            .iter()
            .map(|edge| edge.target)
            .collect::<BTreeSet<_>>();
        assert!(targets.contains(&hostile_a));
        assert!(targets.contains(&hostile_b));
        assert!(
            targets.contains(&n0),
            "bridge must include neutral {n0:?}: {targets:?}"
        );
        assert!(
            targets.contains(&n1),
            "bridge must include neutral {n1:?}: {targets:?}"
        );
        assert!(
            targets.contains(&n5),
            "bridge must include neutral {n5:?}: {targets:?}"
        );
        assert!(!targets.contains(&ignored));
        assert_eq!(hostile[0].edges.len(), 5);
        assert!(
            fronts
                .iter()
                .flat_map(|front| front.edges.iter())
                .all(|edge| edge.target != ignored),
            "ignored edges must never appear in fronts: {fronts:?}"
        );

        let neutral = fronts
            .iter()
            .filter(|front| front.opponent.is_none())
            .collect::<Vec<_>>();
        assert_eq!(
            neutral.len(),
            1,
            "all unowned-facing edges must form one neutral front: {fronts:?}"
        );
        let neutral_targets = neutral[0]
            .edges
            .iter()
            .map(|edge| edge.target)
            .collect::<BTreeSet<_>>();
        assert_eq!(neutral_targets, BTreeSet::from([n0, n1, n5]));
        assert!(
            neutral[0]
                .edges
                .iter()
                .all(|edge| hostile[0].edges.contains(edge)),
            "neutral bridge edges must be allowed to belong to both fronts: {fronts:?}"
        );
    }

    #[test]
    fn all_neutral_open_chain_with_ignored_edge_is_one_front() {
        // Ignored in the middle of the direction ring forces an open chain whose
        // BTree start is not adjacent to the ignored edge on both sides.
        let component = BTreeSet::from([Axial::ZERO]);
        let ignored = Axial::new(-1, 0); // DIRECTIONS[3]
        let fronts = strategic_fronts(component, |_, target| {
            if target == ignored {
                StrategicExterior::Ignored
            } else {
                StrategicExterior::Neutral
            }
        })
        .expect("fronts");
        assert_eq!(
            fronts.len(),
            1,
            "all-neutral open chain must remain a single front: {fronts:?}"
        );
        assert_eq!(fronts[0].opponent, None);
        assert_eq!(fronts[0].edges.len(), 5);
        assert!(fronts[0].edges.iter().all(|edge| edge.target != ignored));
    }

    #[test]
    fn empty_component_is_rejected() {
        assert_eq!(
            strategic_fronts(std::iter::empty(), |_, _| StrategicExterior::Neutral),
            Err(StrategicFrontError::EmptyComponent)
        );
    }
}
