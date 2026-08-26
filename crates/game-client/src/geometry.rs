use bevy::prelude::*;
use hex_core::{Axial, ChunkCoord};

pub const HEX_RADIUS: f32 = 0.72;
pub const HEX_GAP_SCALE: f32 = 0.975;
/// Circular fillet at each lattice vertex. Small enough to stay a hex.
pub const HEX_FILLET_RADIUS: f32 = HEX_RADIUS * 0.16;
/// Samples along each vertex arc, including both tangent endpoints.
pub const HEX_FILLET_ARC_POINTS: usize = 3;
pub const HEX_OUTLINE_LEN: usize = 6 * HEX_FILLET_ARC_POINTS;
/// Quarter-circle from the flat top onto the vertical wall. Smaller than the
/// plan fillet so the column still reads as a step, not a pebble.
pub const HEX_LIP_RADIUS: f32 = HEX_RADIUS * 0.06;
/// Samples along the top-to-side lip, including both endpoints.
pub const HEX_LIP_ARC_POINTS: usize = 3;
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

/// Filleted top/outline ring: six cloudy vertices, still a hex.
pub fn filleted_outline(center: Vec3, y: f32) -> [Vec3; HEX_OUTLINE_LEN] {
    let sharp = std::array::from_fn::<_, 6, _>(|index| corner(center, index, y));
    let radius = HEX_FILLET_RADIUS;
    let tangent = radius / 3.0_f32.sqrt();
    let inset = 2.0 * radius / 3.0_f32.sqrt();
    let mut outline = [Vec3::ZERO; HEX_OUTLINE_LEN];
    for index in 0..6 {
        let prev = sharp[(index + 5) % 6];
        let curr = sharp[index];
        let next = sharp[(index + 1) % 6];
        let to_prev = (prev - curr).normalize_or_zero();
        let to_next = (next - curr).normalize_or_zero();
        let start = curr + to_prev * tangent;
        let end = curr + to_next * tangent;
        let focus = curr + (to_prev + to_next).normalize_or_zero() * inset;
        let base = index * HEX_FILLET_ARC_POINTS;
        for sample in 0..HEX_FILLET_ARC_POINTS {
            let t = sample as f32 / (HEX_FILLET_ARC_POINTS - 1) as f32;
            outline[base + sample] = fillet_arc_point(focus, start, end, t);
        }
    }
    outline
}

pub fn scaled_filleted_point(center: Vec3, point: Vec3, scale: f32) -> Vec3 {
    center + (point - center) * scale
}

/// `t = 0` is the inset top rim; `t = 1` is the outer wall just below the top.
/// The column base is not rounded.
pub fn hex_lip_point(center: Vec3, outline: Vec3, top_y: f32, t: f32) -> Vec3 {
    let outward = hex_lip_outward(center, outline);
    let radius = HEX_LIP_RADIUS;
    let focus = Vec3::new(outline.x, top_y, outline.z) - outward * radius - Vec3::Y * radius;
    let theta = t.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2;
    focus + outward * radius * theta.sin() + Vec3::Y * radius * theta.cos()
}

pub fn hex_lip_normal(center: Vec3, outline: Vec3, t: f32) -> Vec3 {
    let outward = hex_lip_outward(center, outline);
    let theta = t.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2;
    (outward * theta.sin() + Vec3::Y * theta.cos()).normalize_or_zero()
}

fn hex_lip_outward(center: Vec3, outline: Vec3) -> Vec3 {
    Vec3::new(outline.x - center.x, 0.0, outline.z - center.z).normalize_or_zero()
}

fn fillet_arc_point(focus: Vec3, start: Vec3, end: Vec3, t: f32) -> Vec3 {
    let from = Vec2::new(start.x - focus.x, start.z - focus.z);
    let to = Vec2::new(end.x - focus.x, end.z - focus.z);
    let radius = from.length().max(1.0e-5);
    let from = from / radius;
    let mut delta = from.angle_to(to.normalize_or_zero());
    if delta > std::f32::consts::PI {
        delta -= std::f32::consts::TAU;
    } else if delta < -std::f32::consts::PI {
        delta += std::f32::consts::TAU;
    }
    let rotated = Vec2::from_angle(delta * t).rotate(from) * radius;
    Vec3::new(focus.x + rotated.x, start.y, focus.z + rotated.y)
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

    #[test]
    fn fillet_radius_is_a_small_fraction_of_the_hex() {
        assert!((HEX_FILLET_RADIUS - HEX_RADIUS * 0.16).abs() < f32::EPSILON);
        assert!((HEX_FILLET_RADIUS - 0.1152).abs() < 1.0e-4);
        assert!((HEX_LIP_RADIUS - HEX_RADIUS * 0.06).abs() < f32::EPSILON);
        assert!((HEX_LIP_RADIUS - 0.0432).abs() < 1.0e-4);
        const {
            assert!(HEX_LIP_RADIUS < HEX_FILLET_RADIUS);
        }
    }

    #[test]
    fn lip_rounds_the_top_edge_and_leaves_the_column_base() {
        let center = world_center(Axial::ZERO, 1, false);
        let outline = filleted_outline(center, center.y);
        let rim = outline[0];
        let inner = hex_lip_point(center, rim, center.y, 0.0);
        let outer = hex_lip_point(center, rim, center.y, 1.0);
        let mid = hex_lip_point(center, rim, center.y, 0.5);

        assert!((inner.y - center.y).abs() < 1.0e-5);
        assert!((outer.y - (center.y - HEX_LIP_RADIUS)).abs() < 1.0e-5);
        assert!(mid.y < inner.y && mid.y > outer.y);
        let inner_r = Vec2::new(inner.x - center.x, inner.z - center.z).length();
        let outer_r = Vec2::new(outer.x - center.x, outer.z - center.z).length();
        assert!(inner_r + HEX_LIP_RADIUS * 0.5 < outer_r);
        assert!((outer.x - rim.x).abs() < 1.0e-5 && (outer.z - rim.z).abs() < 1.0e-5);
        assert!(COLUMN_FLOOR < outer.y);
    }

    #[test]
    fn filleted_outline_rounds_vertices_but_stays_a_hex() {
        let center = world_center(Axial::ZERO, 0, false);
        let outline = filleted_outline(center, center.y);
        assert_eq!(outline.len(), HEX_OUTLINE_LEN);

        let radius = |point: Vec3| Vec2::new(point.x - center.x, point.z - center.z).length();
        for index in 0..6 {
            let sharp = corner(center, index, center.y);
            let closest = outline
                .iter()
                .copied()
                .map(|point| point.distance(sharp))
                .fold(f32::MAX, f32::min);
            assert!(
                closest > HEX_FILLET_RADIUS * 0.08,
                "vertex {index} is still knife-sharp ({closest})"
            );

            let arc_mid = outline[index * HEX_FILLET_ARC_POINTS + HEX_FILLET_ARC_POINTS / 2];
            let start = outline[index * HEX_FILLET_ARC_POINTS];
            let end = outline[index * HEX_FILLET_ARC_POINTS + HEX_FILLET_ARC_POINTS - 1];
            let next = outline[((index + 1) % 6) * HEX_FILLET_ARC_POINTS];
            let edge_mid = end.lerp(next, 0.5);
            assert!(
                radius(arc_mid) > radius(start),
                "vertex {index} arc folded inward"
            );
            assert!(
                radius(arc_mid) > radius(edge_mid) + 0.03,
                "vertex {index} fillet flattened the hex: arc {} edge {}",
                radius(arc_mid),
                radius(edge_mid)
            );
        }
    }
}
