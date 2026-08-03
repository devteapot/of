use std::collections::{BTreeSet, VecDeque};

use crate::Axial;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirectedFrontEdge {
    pub source: Axial,
    pub target: Axial,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrontSelectionError {
    EmptySelection,
    DisconnectedSelection,
    InvalidDirection,
    NoEligibleFront,
}

/// Derives the exact outward-facing edge set for a selected source region.
///
/// Only the chosen axial direction is considered. This is important for deep
/// selections: cells painted backward supply strength and routes, but their
/// side edges do not silently become additional fronts. The whole source
/// region must be six-connected, while target eligibility may split its active
/// boundary into several independent arcs.
pub fn selected_front_edges(
    sources: &BTreeSet<Axial>,
    direction: Axial,
    mut target_is_eligible: impl FnMut(Axial, Axial) -> bool,
) -> Result<Vec<DirectedFrontEdge>, FrontSelectionError> {
    if sources.is_empty() {
        return Err(FrontSelectionError::EmptySelection);
    }
    if !is_connected(sources) {
        return Err(FrontSelectionError::DisconnectedSelection);
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

/// Derives every eligible outward edge around a selected source region.
///
/// Unlike [`selected_front_edges`], this operation has no orientation and
/// considers all six sides. Its active boundary may contain multiple
/// disconnected arcs, while the selected source region itself must still be
/// six-connected. Callers decide eligibility, which lets the authoritative
/// simulation restrict expansion to neutral, passable, capturable ground
/// without coupling this pure crate to a particular ownership policy.
pub fn selected_all_front_edges(
    sources: &BTreeSet<Axial>,
    mut target_is_eligible: impl FnMut(Axial, Axial) -> bool,
) -> Result<Vec<DirectedFrontEdge>, FrontSelectionError> {
    if sources.is_empty() {
        return Err(FrontSelectionError::EmptySelection);
    }
    if !is_connected(sources) {
        return Err(FrontSelectionError::DisconnectedSelection);
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

fn is_connected(cells: &BTreeSet<Axial>) -> bool {
    let Some(seed) = cells.first().copied() else {
        return false;
    };
    let mut visited = BTreeSet::from([seed]);
    let mut pending = VecDeque::from([seed]);
    while let Some(current) = pending.pop_front() {
        for neighbor in current.neighbors() {
            if cells.contains(&neighbor) && visited.insert(neighbor) {
                pending.push_back(neighbor);
            }
        }
    }
    visited.len() == cells.len()
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
    fn a_disconnected_source_region_is_rejected() {
        let disconnected_sources = BTreeSet::from([Axial::ZERO, Axial::new(2, 0)]);
        assert_eq!(
            selected_front_edges(&disconnected_sources, Axial::new(1, 0), |_, _| true),
            Err(FrontSelectionError::DisconnectedSelection)
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
    fn all_fronts_still_require_one_connected_source_region() {
        let disconnected = BTreeSet::from([Axial::ZERO, Axial::new(2, 0)]);
        assert_eq!(
            selected_all_front_edges(&disconnected, |_, _| true),
            Err(FrontSelectionError::DisconnectedSelection)
        );
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
