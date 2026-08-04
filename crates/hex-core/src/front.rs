use std::{
    borrow::Borrow,
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, BinaryHeap},
};

use crate::Axial;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
}
