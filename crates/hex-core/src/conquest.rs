use crate::map::{HexMap, PlayerId};

pub const BASIS_POINTS: u32 = 10_000;

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConquestError {
    EmptyCapturableMap,
    InvalidThreshold,
}

/// Fixed-at-match-start Conquest denominator and threshold.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConquestRule {
    total_capturable: u64,
    threshold_bps: u32,
    required_control: u64,
}

impl ConquestRule {
    pub fn new(total_capturable: u64, threshold_bps: u32) -> Result<Self, ConquestError> {
        if total_capturable == 0 {
            return Err(ConquestError::EmptyCapturableMap);
        }
        if threshold_bps == 0 || threshold_bps > BASIS_POINTS {
            return Err(ConquestError::InvalidThreshold);
        }

        let numerator = u128::from(total_capturable) * u128::from(threshold_bps);
        let required = numerator.div_ceil(u128::from(BASIS_POINTS));
        Ok(Self {
            total_capturable,
            threshold_bps,
            required_control: required as u64,
        })
    }

    pub fn v1(total_capturable: u64) -> Result<Self, ConquestError> {
        Self::new(total_capturable, 8_000)
    }

    pub const fn total_capturable(self) -> u64 {
        self.total_capturable
    }

    pub const fn threshold_bps(self) -> u32 {
        self.threshold_bps
    }

    pub const fn required_control(self) -> u64 {
        self.required_control
    }

    pub fn progress(self, controlled: u64) -> ConquestProgress {
        let controlled = controlled.min(self.total_capturable);
        ConquestProgress {
            controlled,
            total_capturable: self.total_capturable,
            required_control: self.required_control,
            won: controlled >= self.required_control,
        }
    }

    pub fn progress_on_map(self, map: &HexMap, player: PlayerId) -> ConquestProgress {
        let controlled = map
            .cells()
            .filter(|cell| cell.capturable && cell.owner == Some(player))
            .count() as u64;
        self.progress(controlled)
    }
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConquestProgress {
    pub controlled: u64,
    pub total_capturable: u64,
    pub required_control: u64,
    pub won: bool,
}

impl ConquestProgress {
    /// Display-only integer basis points, rounded down. Victory uses the exact
    /// cross-multiplied threshold captured by `required_control`.
    pub fn controlled_bps(self) -> u32 {
        ((u128::from(self.controlled) * u128::from(BASIS_POINTS))
            / u128::from(self.total_capturable)) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{coord::Axial, map::Cell};

    #[test]
    fn v1_requires_exactly_eighty_percent_with_ceiling() {
        let hundred = ConquestRule::v1(100).unwrap();
        assert_eq!(hundred.required_control(), 80);
        assert!(!hundred.progress(79).won);
        assert!(hundred.progress(80).won);

        let three = ConquestRule::v1(3).unwrap();
        assert_eq!(three.required_control(), 3);
        assert!(!three.progress(2).won);
        assert!(three.progress(3).won);
    }

    #[test]
    fn threshold_math_is_overflow_safe_for_large_maps() {
        let rule = ConquestRule::v1(u64::MAX).unwrap();
        assert_eq!(rule.required_control(), 14_757_395_258_967_641_292);
        assert!(!rule.progress(rule.required_control() - 1).won);
        assert!(rule.progress(rule.required_control()).won);
    }

    #[test]
    fn fixed_denominator_excludes_non_capturable_cells() {
        let mut map = HexMap::new();
        for q in 0..10 {
            let mut cell = Cell::ground(Axial::new(q, 0), 0, Some(if q < 8 { 1 } else { 2 }), 100);
            cell.capturable = true;
            map.insert(cell);
        }
        map.insert(Cell::water(Axial::new(10, 0), 0));

        let progress = ConquestRule::v1(10).unwrap().progress_on_map(&map, 1);
        assert_eq!(progress.controlled, 8);
        assert_eq!(progress.controlled_bps(), 8_000);
        assert!(progress.won);
    }

    #[test]
    fn invalid_rules_are_rejected() {
        assert_eq!(ConquestRule::v1(0), Err(ConquestError::EmptyCapturableMap));
        assert_eq!(
            ConquestRule::new(10, 10_001),
            Err(ConquestError::InvalidThreshold)
        );
    }
}
