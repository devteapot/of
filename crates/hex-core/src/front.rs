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
    DisconnectedFront,
}

/// Derives the exact outward-facing edge set for a selected source region.
///
/// Only the chosen axial direction is considered. This is important for deep
/// selections: cells painted backward supply strength and routes, but their
/// side edges do not silently become additional fronts. Both the whole source
/// region and its active boundary must be six-connected.
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

    let active_sources = edges.iter().map(|edge| edge.source).collect();
    if !is_connected(&active_sources) {
        return Err(FrontSelectionError::DisconnectedFront);
    }
    Ok(edges)
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
    fn disconnected_source_or_front_is_rejected() {
        let disconnected_sources = BTreeSet::from([Axial::ZERO, Axial::new(2, 0)]);
        assert_eq!(
            selected_front_edges(&disconnected_sources, Axial::new(1, 0), |_, _| true),
            Err(FrontSelectionError::DisconnectedSelection)
        );

        let connected_sources =
            BTreeSet::from([Axial::new(0, -1), Axial::new(0, 0), Axial::new(0, 1)]);
        assert_eq!(
            selected_front_edges(&connected_sources, Axial::new(1, 0), |source, _| source.r
                != 0,),
            Err(FrontSelectionError::DisconnectedFront)
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
}
