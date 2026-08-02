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
}
