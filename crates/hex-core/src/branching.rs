use core::cmp::Ordering;

use crate::Axial;

/// Mild deterministic bias used by a branching wave relative to a focus hex.
///
/// The comparison is deliberately local to `parent`: the clicked focus acts
/// like a scalar potential, not one global heading copied onto every front.
/// Consequently, opposed edges around an enclosed focus point in opposed
/// directions. Every branch remains eligible: moving toward the focus receives
/// weight 3, moving along an equal-distance contour receives weight 2, and
/// moving away receives weight 1.
pub fn focus_branch_weight(parent: Axial, child: Axial, focus: Axial) -> u8 {
    match child.distance(focus).cmp(&parent.distance(focus)) {
        Ordering::Less => 3,
        Ordering::Equal => 2,
        Ordering::Greater => 1,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BranchAllocationError {
    EmptyChildren,
    ZeroWeight { child_index: usize },
    ArithmeticOverflow,
    AllocationMismatch,
}

/// Exact child quotas for one branching node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WeightedBranchQuotas {
    pub by_child: Vec<u64>,
    /// Opaque child-index cursor to feed into the next allocation at this node.
    pub next_cursor: usize,
}

/// One conserved part of an input contribution assigned to one child.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BranchContributionAllocation {
    pub contribution_index: usize,
    pub child_index: usize,
    pub amount: u64,
}

/// Contribution-preserving allocations for an authoritative branching node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WeightedBranchAllocations {
    pub allocations: Vec<BranchContributionAllocation>,
    /// Opaque child-index cursor to feed into the next allocation at this node.
    pub next_cursor: usize,
}

/// Divides `total` across positive child weights with exact conservation.
///
/// When strength permits, every child first receives a one-unit baseline so a
/// focus bias never switches off the other fronts. If there is not enough for
/// every child, distinct children receive one unit in cursor order. Remaining
/// strength is apportioned by weight using integer largest remainders; equal
/// remainders use the rotating cursor as their deterministic tie-break. The
/// returned cursor should be persisted by the caller for fair later arrivals.
pub fn weighted_branch_quotas_rotated(
    total: u64,
    child_weights: &[u8],
    start_cursor: usize,
) -> Result<WeightedBranchQuotas, BranchAllocationError> {
    if child_weights.is_empty() {
        return Err(BranchAllocationError::EmptyChildren);
    }
    if let Some(child_index) = child_weights.iter().position(|weight| *weight == 0) {
        return Err(BranchAllocationError::ZeroWeight { child_index });
    }

    let child_count = child_weights.len();
    let child_count_u64 =
        u64::try_from(child_count).map_err(|_| BranchAllocationError::ArithmeticOverflow)?;
    let cursor = start_cursor % child_count;
    let mut by_child = vec![0_u64; child_count];

    if total < child_count_u64 {
        let assigned =
            usize::try_from(total).map_err(|_| BranchAllocationError::ArithmeticOverflow)?;
        for offset in 0..assigned {
            by_child[(cursor + offset) % child_count] = 1;
        }
        return Ok(WeightedBranchQuotas {
            by_child,
            next_cursor: (cursor + assigned) % child_count,
        });
    }

    by_child.fill(1);
    let weighted_strength = total - child_count_u64;
    let weight_sum = child_weights
        .iter()
        .try_fold(0_u128, |sum, weight| sum.checked_add(u128::from(*weight)))
        .ok_or(BranchAllocationError::ArithmeticOverflow)?;

    let mut distributed = 0_u64;
    let mut remainders = Vec::with_capacity(child_count);
    for (child_index, &weight) in child_weights.iter().enumerate() {
        let numerator = u128::from(weighted_strength)
            .checked_mul(u128::from(weight))
            .ok_or(BranchAllocationError::ArithmeticOverflow)?;
        let weighted_quota = u64::try_from(numerator / weight_sum)
            .map_err(|_| BranchAllocationError::ArithmeticOverflow)?;
        by_child[child_index] = by_child[child_index]
            .checked_add(weighted_quota)
            .ok_or(BranchAllocationError::ArithmeticOverflow)?;
        distributed = distributed
            .checked_add(weighted_quota)
            .ok_or(BranchAllocationError::ArithmeticOverflow)?;
        remainders.push((child_index, numerator % weight_sum));
    }

    let leftover = weighted_strength
        .checked_sub(distributed)
        .ok_or(BranchAllocationError::AllocationMismatch)?;
    let leftover =
        usize::try_from(leftover).map_err(|_| BranchAllocationError::ArithmeticOverflow)?;
    if leftover >= child_count {
        return Err(BranchAllocationError::AllocationMismatch);
    }

    remainders.sort_unstable_by(
        |(left_index, left_remainder), (right_index, right_remainder)| {
            right_remainder.cmp(left_remainder).then_with(|| {
                let left_rank = (*left_index + child_count - cursor) % child_count;
                let right_rank = (*right_index + child_count - cursor) % child_count;
                left_rank.cmp(&right_rank)
            })
        },
    );
    let mut last_remainder_child = None;
    for &(child_index, _) in remainders.iter().take(leftover) {
        by_child[child_index] = by_child[child_index]
            .checked_add(1)
            .ok_or(BranchAllocationError::ArithmeticOverflow)?;
        last_remainder_child = Some(child_index);
    }

    if by_child
        .iter()
        .try_fold(0_u64, |sum, quota| sum.checked_add(*quota))
        != Some(total)
    {
        return Err(BranchAllocationError::AllocationMismatch);
    }

    Ok(WeightedBranchQuotas {
        by_child,
        next_cursor: last_remainder_child.map_or(cursor, |child| (child + 1) % child_count),
    })
}

/// Preserves input contribution identity while filling weighted child quotas.
///
/// Contributions and children are consumed in their supplied order. Zero
/// contributions are omitted from the sparse result.
pub fn weighted_branch_allocations_rotated(
    contribution_amounts: &[u64],
    child_weights: &[u8],
    start_cursor: usize,
) -> Result<WeightedBranchAllocations, BranchAllocationError> {
    let total = contribution_amounts
        .iter()
        .try_fold(0_u64, |sum, amount| sum.checked_add(*amount))
        .ok_or(BranchAllocationError::ArithmeticOverflow)?;
    let quotas = weighted_branch_quotas_rotated(total, child_weights, start_cursor)?;
    let mut remaining_by_child = quotas.by_child;
    let mut allocations = Vec::new();
    let mut child_index = 0;

    for (contribution_index, &amount) in contribution_amounts.iter().enumerate() {
        let mut remaining = amount;
        while remaining > 0 {
            while child_index < remaining_by_child.len() && remaining_by_child[child_index] == 0 {
                child_index += 1;
            }
            if child_index == remaining_by_child.len() {
                return Err(BranchAllocationError::AllocationMismatch);
            }
            let assigned = remaining.min(remaining_by_child[child_index]);
            allocations.push(BranchContributionAllocation {
                contribution_index,
                child_index,
                amount: assigned,
            });
            remaining -= assigned;
            remaining_by_child[child_index] -= assigned;
        }
    }

    if remaining_by_child.iter().any(|remaining| *remaining != 0) {
        return Err(BranchAllocationError::AllocationMismatch);
    }
    Ok(WeightedBranchAllocations {
        allocations,
        next_cursor: quotas.next_cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_weights_are_three_two_one_by_hex_distance() {
        let parent = Axial::ZERO;
        let focus = Axial::new(3, 0);

        assert_eq!(focus_branch_weight(parent, Axial::new(1, 0), focus), 3);
        assert_eq!(focus_branch_weight(parent, Axial::new(1, -1), focus), 2);
        assert_eq!(focus_branch_weight(parent, Axial::new(-1, 0), focus), 1);
    }

    #[test]
    fn focus_on_the_parent_keeps_every_outward_weight_positive() {
        for child in Axial::ZERO.neighbors() {
            assert_eq!(focus_branch_weight(Axial::ZERO, child, Axial::ZERO), 1);
        }
    }

    #[test]
    fn enclosed_focus_reverses_the_preferred_vector_at_every_local_front() {
        let focus = Axial::ZERO;

        for direction in Axial::DIRECTIONS {
            let parent = focus + direction;
            let inward = focus;
            let outward = parent + direction;

            assert_eq!(focus_branch_weight(parent, inward, focus), 3);
            assert_eq!(focus_branch_weight(parent, outward, focus), 1);

            let split = weighted_branch_quotas_rotated(
                20,
                &[
                    focus_branch_weight(parent, inward, focus),
                    focus_branch_weight(parent, outward, focus),
                ],
                0,
            )
            .unwrap();
            assert_eq!(split.by_child, vec![15, 5]);
            assert_eq!(split.by_child.iter().sum::<u64>(), 20);
            assert!(split.by_child[1] > 0, "the opposite perimeter stays active");
        }
    }

    #[test]
    fn weighted_quotas_keep_a_positive_baseline_and_conserve_exactly() {
        let result = weighted_branch_quotas_rotated(12, &[3, 2, 1], 0).unwrap();

        assert_eq!(result.by_child, vec![6, 4, 2]);
        assert_eq!(result.by_child.iter().sum::<u64>(), 12);
        assert!(result.by_child.into_iter().all(|quota| quota > 0));
    }

    #[test]
    fn insufficient_strength_visits_distinct_children_in_cursor_order() {
        let first = weighted_branch_quotas_rotated(2, &[3, 2, 1, 3], 1).unwrap();
        assert_eq!(first.by_child, vec![0, 1, 1, 0]);
        assert_eq!(first.next_cursor, 3);

        let second = weighted_branch_quotas_rotated(2, &[3, 2, 1, 3], first.next_cursor).unwrap();
        assert_eq!(second.by_child, vec![1, 0, 0, 1]);
        assert_eq!(second.next_cursor, 1);
    }

    #[test]
    fn equal_weight_remainders_rotate_fairly() {
        let mut cursor = 0;
        let mut results = Vec::new();
        for _ in 0..3 {
            let result = weighted_branch_quotas_rotated(4, &[1, 1, 1], cursor).unwrap();
            cursor = result.next_cursor;
            results.push(result.by_child);
        }

        assert_eq!(results, vec![vec![2, 1, 1], vec![1, 2, 1], vec![1, 1, 2]]);
        assert_eq!(cursor, 0);
    }

    #[test]
    fn sparse_equal_remainders_rotate_past_the_child_that_won_the_tie() {
        let first = weighted_branch_quotas_rotated(5, &[1, 2, 1], 1).unwrap();
        assert_eq!(first.by_child, vec![1, 2, 2]);
        assert_eq!(first.next_cursor, 0);

        let second = weighted_branch_quotas_rotated(5, &[1, 2, 1], first.next_cursor).unwrap();
        assert_eq!(second.by_child, vec![2, 2, 1]);
        assert_eq!(second.next_cursor, 1);
    }

    #[test]
    fn quotas_conserve_across_totals_weights_and_cursors() {
        let weight_sets: &[&[u8]] = &[&[1], &[1, 1], &[3, 2, 1], &[1, 3, 2, 1, 2, 3]];
        for weights in weight_sets {
            for total in 0..=200 {
                for cursor in 0..weights.len() * 2 {
                    let result = weighted_branch_quotas_rotated(total, weights, cursor).unwrap();
                    assert_eq!(result.by_child.iter().sum::<u64>(), total);
                    if total >= weights.len() as u64 {
                        assert!(result.by_child.iter().all(|quota| *quota > 0));
                    } else {
                        assert_eq!(
                            result.by_child.iter().filter(|quota| **quota == 1).count(),
                            total as usize
                        );
                    }
                    assert!(result.next_cursor < weights.len());
                }
            }
        }
    }

    #[test]
    fn contribution_allocations_preserve_sources_and_fill_child_quotas() {
        let result = weighted_branch_allocations_rotated(&[2, 5, 4], &[3, 2, 1], 0).unwrap();

        assert_eq!(
            result.allocations,
            vec![
                BranchContributionAllocation {
                    contribution_index: 0,
                    child_index: 0,
                    amount: 2,
                },
                BranchContributionAllocation {
                    contribution_index: 1,
                    child_index: 0,
                    amount: 3,
                },
                BranchContributionAllocation {
                    contribution_index: 1,
                    child_index: 1,
                    amount: 2,
                },
                BranchContributionAllocation {
                    contribution_index: 2,
                    child_index: 1,
                    amount: 2,
                },
                BranchContributionAllocation {
                    contribution_index: 2,
                    child_index: 2,
                    amount: 2,
                },
            ]
        );
        assert_eq!(
            result
                .allocations
                .iter()
                .map(|allocation| allocation.amount)
                .sum::<u64>(),
            11
        );
    }

    #[test]
    fn invalid_inputs_are_rejected_without_partial_results() {
        assert_eq!(
            weighted_branch_quotas_rotated(1, &[], 0),
            Err(BranchAllocationError::EmptyChildren)
        );
        assert_eq!(
            weighted_branch_quotas_rotated(1, &[1, 0, 1], 0),
            Err(BranchAllocationError::ZeroWeight { child_index: 1 })
        );
        assert_eq!(
            weighted_branch_allocations_rotated(&[u64::MAX, 1], &[1], 0),
            Err(BranchAllocationError::ArithmeticOverflow)
        );
    }
}
