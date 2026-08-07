use core::ops::{Add, Sub};

/// An axial hex coordinate. The implicit cube coordinate is `y = -q - r`.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Axial {
    pub q: i32,
    pub r: i32,
}

impl Axial {
    pub const ZERO: Self = Self::new(0, 0);

    /// Clockwise direction order, beginning at positive q.
    pub const DIRECTIONS: [Self; 6] = [
        Self::new(1, 0),
        Self::new(1, -1),
        Self::new(0, -1),
        Self::new(-1, 0),
        Self::new(-1, 1),
        Self::new(0, 1),
    ];

    pub const fn new(q: i32, r: i32) -> Self {
        Self { q, r }
    }

    pub fn cube(self) -> Cube {
        Cube {
            x: i64::from(self.q),
            y: -i64::from(self.q) - i64::from(self.r),
            z: i64::from(self.r),
        }
    }

    pub fn neighbor(self, direction: HexDirection) -> Self {
        self + Self::DIRECTIONS[direction.index()]
    }

    pub fn neighbors(self) -> [Self; 6] {
        Self::DIRECTIONS.map(|direction| self + direction)
    }

    /// Hex distance, calculated through cube coordinates without i32 overflow.
    pub fn distance(self, other: Self) -> u64 {
        let delta = self.cube() - other.cube();
        (delta.x.unsigned_abs() + delta.y.unsigned_abs() + delta.z.unsigned_abs()) / 2
    }

    /// Resolves this coordinate into an axial parallelogram chunk.
    ///
    /// Euclidean division makes local coordinates non-negative for negative map
    /// coordinates as well.
    pub fn chunk_address(self, chunk_size: u32) -> Option<ChunkAddress> {
        let size = i32::try_from(chunk_size).ok()?;
        if size == 0 {
            return None;
        }

        Some(ChunkAddress {
            chunk: ChunkCoord {
                q: self.q.div_euclid(size),
                r: self.r.div_euclid(size),
            },
            local_q: self.q.rem_euclid(size) as u32,
            local_r: self.r.rem_euclid(size) as u32,
        })
    }
}

impl Add for Axial {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self::new(self.q + rhs.q, self.r + rhs.r)
    }
}

impl Sub for Axial {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self::new(self.q - rhs.q, self.r - rhs.r)
    }
}

/// A cube-coordinate view used for geometry calculations.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Cube {
    pub x: i64,
    pub y: i64,
    pub z: i64,
}

impl Cube {
    pub const fn is_valid(self) -> bool {
        self.x + self.y + self.z == 0
    }
}

impl Sub for Cube {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            x: self.x - rhs.x,
            y: self.y - rhs.y,
            z: self.z - rhs.z,
        }
    }
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum HexDirection {
    East = 0,
    NorthEast = 1,
    NorthWest = 2,
    West = 3,
    SouthWest = 4,
    SouthEast = 5,
}

impl HexDirection {
    pub const ALL: [Self; 6] = [
        Self::East,
        Self::NorthEast,
        Self::NorthWest,
        Self::West,
        Self::SouthWest,
        Self::SouthEast,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn opposite(self) -> Self {
        Self::ALL[(self.index() + 3) % 6]
    }
}

/// A canonical undirected edge between adjacent hexes.
///
/// Deserialization enforces the constructor invariants: the endpoints must be
/// adjacent and stored in canonical `a <= b` order, so a snapshot cannot smuggle
/// in an edge that API-built state could never contain.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(try_from = "RawHexEdge"))]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HexEdge {
    pub a: Axial,
    pub b: Axial,
}

#[cfg(feature = "serde")]
#[derive(serde::Deserialize)]
struct RawHexEdge {
    a: Axial,
    b: Axial,
}

#[cfg(feature = "serde")]
impl TryFrom<RawHexEdge> for HexEdge {
    type Error = String;

    fn try_from(raw: RawHexEdge) -> Result<Self, Self::Error> {
        let edge = HexEdge::new(raw.a, raw.b).ok_or_else(|| {
            format!(
                "hex edge endpoints are not adjacent: {:?} and {:?}",
                raw.a, raw.b
            )
        })?;
        if edge.a != raw.a {
            return Err(format!(
                "hex edge is not in canonical a <= b order: {:?} and {:?}",
                raw.a, raw.b
            ));
        }
        Ok(edge)
    }
}

impl HexEdge {
    pub fn new(first: Axial, second: Axial) -> Option<Self> {
        if first.distance(second) != 1 {
            return None;
        }
        let (a, b) = if first <= second {
            (first, second)
        } else {
            (second, first)
        };
        Some(Self { a, b })
    }

    pub fn contains(self, coordinate: Axial) -> bool {
        self.a == coordinate || self.b == coordinate
    }
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ChunkCoord {
    pub q: i32,
    pub r: i32,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ChunkAddress {
    pub chunk: ChunkCoord,
    pub local_q: u32,
    pub local_r: u32,
}

impl ChunkAddress {
    pub fn to_axial(self, chunk_size: u32) -> Option<Axial> {
        if chunk_size == 0 || self.local_q >= chunk_size || self.local_r >= chunk_size {
            return None;
        }
        let size = i64::from(chunk_size);
        let q = i64::from(self.chunk.q)
            .checked_mul(size)?
            .checked_add(i64::from(self.local_q))?;
        let r = i64::from(self.chunk.r)
            .checked_mul(size)?
            .checked_add(i64::from(self.local_r))?;
        Some(Axial::new(i32::try_from(q).ok()?, i32::try_from(r).ok()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_coordinates_keep_the_zero_sum_invariant() {
        for q in -20..=20 {
            for r in -20..=20 {
                assert!(Axial::new(q, r).cube().is_valid());
            }
        }
    }

    #[test]
    fn all_neighbors_are_symmetric_and_one_step_away() {
        for q in -10..=10 {
            for r in -10..=10 {
                let origin = Axial::new(q, r);
                for direction in HexDirection::ALL {
                    let neighbor = origin.neighbor(direction);
                    assert_eq!(origin.distance(neighbor), 1);
                    assert_eq!(neighbor.neighbor(direction.opposite()), origin);
                }
            }
        }
    }

    #[test]
    fn distance_is_symmetric_and_obeys_triangle_inequality() {
        let points = [
            Axial::new(-7, 2),
            Axial::new(0, 0),
            Axial::new(4, -9),
            Axial::new(12, 8),
        ];
        for a in points {
            for b in points {
                assert_eq!(a.distance(b), b.distance(a));
                for c in points {
                    assert!(a.distance(c) <= a.distance(b) + b.distance(c));
                }
            }
        }
    }

    #[test]
    fn chunk_address_round_trips_across_negative_boundaries() {
        for chunk_size in [1, 2, 7, 16, 31] {
            for q in -65..=65 {
                for r in -65..=65 {
                    let coordinate = Axial::new(q, r);
                    let address = coordinate.chunk_address(chunk_size).unwrap();
                    assert!(address.local_q < chunk_size);
                    assert!(address.local_r < chunk_size);
                    assert_eq!(address.to_axial(chunk_size), Some(coordinate));
                }
            }
        }
    }

    #[cfg(feature = "serde")]
    #[test]
    fn hex_edges_round_trip_and_reject_non_canonical_input() {
        let edge = HexEdge::new(Axial::new(1, 0), Axial::new(0, 0)).unwrap();
        assert_eq!(edge.a, Axial::new(0, 0), "constructor canonicalizes order");
        let json = serde_json::to_string(&edge).unwrap();
        assert_eq!(serde_json::from_str::<HexEdge>(&json).unwrap(), edge);

        let swapped = r#"{"a":{"q":1,"r":0},"b":{"q":0,"r":0}}"#;
        let error = serde_json::from_str::<HexEdge>(swapped).unwrap_err();
        assert!(error.to_string().contains("canonical"), "{error}");

        let non_adjacent = r#"{"a":{"q":0,"r":0},"b":{"q":2,"r":0}}"#;
        let error = serde_json::from_str::<HexEdge>(non_adjacent).unwrap_err();
        assert!(error.to_string().contains("not adjacent"), "{error}");
    }

    #[test]
    fn invalid_chunks_are_rejected() {
        assert_eq!(Axial::ZERO.chunk_address(0), None);
        let address = ChunkAddress {
            chunk: ChunkCoord { q: 0, r: 0 },
            local_q: 16,
            local_r: 0,
        };
        assert_eq!(address.to_axial(16), None);
    }
}
