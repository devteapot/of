use bevy::prelude::*;
use hex_core::{Axial, ChunkCoord};

pub const HEX_RADIUS: f32 = 0.72;
pub const HEX_GAP_SCALE: f32 = 0.975;
pub const ELEVATION_STEP: f32 = 0.36;
pub const SEA_LEVEL: f32 = 0.02;
pub const COLUMN_FLOOR: f32 = -0.42;
pub const CHUNK_SIZE: u32 = 8;

pub fn axial_to_plane(coord: Axial) -> Vec2 {
    let q = coord.q as f32;
    let r = coord.r as f32;
    Vec2::new(
        HEX_RADIUS * 1.5 * q,
        HEX_RADIUS * 3.0_f32.sqrt() * (r + q * 0.5),
    )
}

/// Inverse of [`axial_to_plane`]: map a world XZ plane point to the nearest axial hex.
pub fn plane_to_axial(plane: Vec2) -> Axial {
    let q = plane.x / (HEX_RADIUS * 1.5);
    let r = plane.y / (HEX_RADIUS * 3.0_f32.sqrt()) - q * 0.5;
    axial_round(q, r)
}

fn axial_round(q: f32, r: f32) -> Axial {
    let s = -q - r;
    let mut rq = q.round();
    let mut rr = r.round();
    let rs = s.round();
    let q_diff = (rq - q).abs();
    let r_diff = (rr - r).abs();
    let s_diff = (rs - s).abs();
    if q_diff > r_diff && q_diff > s_diff {
        rq = -rr - rs;
    } else if r_diff > s_diff {
        rr = -rq - rs;
    }
    Axial::new(rq as i32, rr as i32)
}

pub fn cell_top(elevation: i16, water: bool) -> f32 {
    if water {
        SEA_LEVEL
    } else {
        SEA_LEVEL + 0.12 + f32::from(elevation.max(0)) * ELEVATION_STEP
    }
}

pub fn world_center(coord: Axial, elevation: i16, water: bool) -> Vec3 {
    let plane = axial_to_plane(coord);
    Vec3::new(plane.x, cell_top(elevation, water), plane.y)
}

pub fn corner(center: Vec3, index: usize, y: f32) -> Vec3 {
    let angle = index as f32 * std::f32::consts::TAU / 6.0;
    Vec3::new(
        center.x + angle.cos() * HEX_RADIUS * HEX_GAP_SCALE,
        y,
        center.z + angle.sin() * HEX_RADIUS * HEX_GAP_SCALE,
    )
}

/// Maps an [`Axial::DIRECTIONS`] index to the matching geometric hex edge.
///
/// Axial directions are ordered clockwise, while [`corner`] advances
/// counter-clockwise. Edge zero (between corners zero and one) is the one
/// exception where both sequences begin at the same positive-q side.
pub(crate) const fn edge_index_for_direction(direction_index: usize) -> usize {
    (6 - direction_index % 6) % 6
}

pub fn chunk_of(coord: Axial) -> ChunkCoord {
    coord
        .chunk_address(CHUNK_SIZE)
        .expect("the fixed client chunk size is non-zero")
        .chunk
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjacent_centers_are_equally_spaced() {
        let origin = axial_to_plane(Axial::ZERO);
        let distances = Axial::ZERO
            .neighbors()
            .map(|neighbor| axial_to_plane(neighbor).distance(origin));
        for distance in distances {
            assert!((distance - HEX_RADIUS * 3.0_f32.sqrt()).abs() < 0.0001);
        }
    }

    #[test]
    fn plane_to_axial_inverts_axial_to_plane_for_nearby_cells() {
        for coord in [
            Axial::ZERO,
            Axial::new(1, 0),
            Axial::new(0, 1),
            Axial::new(-3, 2),
            Axial::new(7, -4),
            Axial::new(-12, -8),
        ] {
            let plane = axial_to_plane(coord);
            assert_eq!(plane_to_axial(plane), coord);
            // Small offsets toward the center still round back to the same hex.
            assert_eq!(plane_to_axial(plane + Vec2::new(0.05, -0.04)), coord);
        }
    }

    #[test]
    fn each_direction_maps_to_the_edge_facing_its_neighbor() {
        let center = world_center(Axial::ZERO, 0, false);
        for (direction_index, neighbor) in Axial::DIRECTIONS.into_iter().enumerate() {
            let edge = edge_index_for_direction(direction_index);
            let edge_midpoint =
                (corner(center, edge, center.y) + corner(center, (edge + 1) % 6, center.y)) * 0.5;
            let neighbor_center = world_center(neighbor, 0, false);
            let edge_direction = (edge_midpoint - center).normalize_or_zero();
            let neighbor_direction = (neighbor_center - center).normalize_or_zero();

            assert!(
                edge_direction.dot(neighbor_direction) > 0.999,
                "direction {direction_index} mapped to edge {edge}"
            );
        }
    }
}
