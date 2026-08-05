//! Pure attack-front traversal helpers used by workers and unit tests.

use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrontCell {
    pub cell_id: u32,
    pub q: i32,
    pub r: i32,
    pub owner: u16,
    pub elevation: i16,
    pub passable: bool,
    pub capturable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttackFront {
    pub source: u32,
    pub target: u32,
    pub target_owner: u16,
}

/// Find one traversable adjacent owned→enemy front per player.
///
/// Returns `Err(player_id)` for the first player without a legal contact.
pub fn find_attack_fronts(
    cells: &[FrontCell],
    player_count: u16,
    max_elevation_step: u8,
) -> Result<Vec<AttackFront>, u16> {
    const DIRECTIONS: [(i32, i32); 6] = [(1, 0), (1, -1), (0, -1), (-1, 0), (-1, 1), (0, 1)];
    let by_coordinate = cells
        .iter()
        .filter(|cell| cell.passable && cell.capturable)
        .map(|cell| ((cell.q, cell.r), cell))
        .collect::<BTreeMap<_, _>>();
    let mut fronts = Vec::with_capacity(usize::from(player_count));
    for player in 1..=player_count {
        let best = cells
            .iter()
            .filter(|cell| cell.owner == player && cell.passable && cell.capturable)
            .flat_map(|source| {
                DIRECTIONS.into_iter().filter_map(|(dq, dr)| {
                    let target = by_coordinate.get(&(source.q + dq, source.r + dr))?;
                    (target.owner != 0
                        && target.owner != player
                        && source.elevation.abs_diff(target.elevation)
                            <= u16::from(max_elevation_step))
                    .then_some(AttackFront {
                        source: source.cell_id,
                        target: target.cell_id,
                        target_owner: target.owner,
                    })
                })
            })
            .min_by_key(|front| (front.source, front.target, front.target_owner));
        fronts.push(best.ok_or(player)?);
    }
    Ok(fronts)
}

pub fn axial_distance(first_q: i32, first_r: i32, second_q: i32, second_r: i32) -> u64 {
    let dq = i64::from(first_q) - i64::from(second_q);
    let dr = i64::from(first_r) - i64::from(second_r);
    (dq.unsigned_abs() + dr.unsigned_abs() + (dq + dr).unsigned_abs()) / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attack_fronts_skip_low_id_cliffs_and_use_a_traversable_alternative() {
        let cells = [
            FrontCell {
                cell_id: 10,
                q: 0,
                r: 0,
                owner: 1,
                elevation: 0,
                passable: true,
                capturable: true,
            },
            FrontCell {
                cell_id: 20,
                q: 1,
                r: 0,
                owner: 2,
                elevation: 2,
                passable: true,
                capturable: true,
            },
            FrontCell {
                cell_id: 30,
                q: 0,
                r: 1,
                owner: 2,
                elevation: 1,
                passable: true,
                capturable: true,
            },
        ];
        assert_eq!(
            find_attack_fronts(&cells, 2, 1),
            Ok(vec![
                AttackFront {
                    source: 10,
                    target: 30,
                    target_owner: 2,
                },
                AttackFront {
                    source: 30,
                    target: 10,
                    target_owner: 1,
                },
            ])
        );
    }

    #[test]
    fn attack_fronts_report_the_first_player_without_enemy_contact() {
        let cells = [FrontCell {
            cell_id: 10,
            q: 0,
            r: 0,
            owner: 1,
            elevation: 0,
            passable: true,
            capturable: true,
        }];
        assert_eq!(find_attack_fronts(&cells, 2, 1), Err(1));
    }

    #[test]
    fn attack_fronts_require_passable_capturable_adjacency() {
        let cells = [
            FrontCell {
                cell_id: 1,
                q: 0,
                r: 0,
                owner: 1,
                elevation: 0,
                passable: true,
                capturable: true,
            },
            FrontCell {
                cell_id: 2,
                q: 1,
                r: 0,
                owner: 2,
                elevation: 0,
                passable: false,
                capturable: true,
            },
            FrontCell {
                cell_id: 3,
                q: 0,
                r: 1,
                owner: 2,
                elevation: 0,
                passable: true,
                capturable: false,
            },
            FrontCell {
                cell_id: 4,
                q: -1,
                r: 0,
                owner: 2,
                elevation: 0,
                passable: true,
                capturable: true,
            },
        ];
        assert_eq!(
            find_attack_fronts(&cells, 2, 1),
            Ok(vec![
                AttackFront {
                    source: 1,
                    target: 4,
                    target_owner: 2,
                },
                AttackFront {
                    source: 4,
                    target: 1,
                    target_owner: 1,
                },
            ])
        );
    }
}
