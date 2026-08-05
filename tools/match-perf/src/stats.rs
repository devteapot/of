//! Quantiles and aggregate controlled-cell statistics for timeline rows.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ControlStats {
    pub min: u64,
    pub p50: u64,
    pub p95: u64,
    pub max: u64,
    pub sum: u64,
    pub count: u16,
}

impl ControlStats {
    pub fn from_counts(mut counts: Vec<u64>) -> Self {
        if counts.is_empty() {
            return Self::default();
        }
        counts.sort_unstable();
        let count = u16::try_from(counts.len()).unwrap_or(u16::MAX);
        let sum = counts.iter().copied().sum();
        Self {
            min: counts[0],
            p50: quantile_sorted(&counts, 0.50),
            p95: quantile_sorted(&counts, 0.95),
            max: *counts.last().expect("non-empty"),
            sum,
            count,
        }
    }
}

/// Nearest-rank quantile on a non-empty sorted slice.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
pub fn quantile_sorted(sorted: &[u64], q: f64) -> u64 {
    assert!(!sorted.is_empty(), "quantile on empty sample");
    let q = q.clamp(0.0, 1.0);
    if sorted.len() == 1 {
        return sorted[0];
    }
    let rank = (q * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

pub fn quantiles_ms(samples_ms: &[f64]) -> (f64, f64, f64, f64) {
    if samples_ms.is_empty() {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let mut sorted = samples_ms.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    (
        quantile_f64_sorted(&sorted, 0.50),
        quantile_f64_sorted(&sorted, 0.95),
        quantile_f64_sorted(&sorted, 0.99),
        sorted[sorted.len() - 1],
    )
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn quantile_f64_sorted(sorted: &[f64], q: f64) -> f64 {
    let q = q.clamp(0.0, 1.0);
    if sorted.len() == 1 {
        return sorted[0];
    }
    let rank = (q * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_stats_aggregate_without_fixed_player_columns() {
        let stats = ControlStats::from_counts(vec![10, 20, 30, 40, 100]);
        assert_eq!(stats.min, 10);
        assert_eq!(stats.max, 100);
        assert_eq!(stats.sum, 200);
        assert_eq!(stats.count, 5);
        assert_eq!(stats.p50, 30);
        assert_eq!(stats.p95, 100);
    }

    #[test]
    fn empty_control_stats_are_zero() {
        assert_eq!(ControlStats::from_counts(vec![]), ControlStats::default());
    }

    #[test]
    fn ms_quantiles_cover_p50_p95_p99_max() {
        // 0..=100 inclusive -> 101 samples; nearest-rank hits exact labels.
        let samples: Vec<f64> = (0..=100).map(f64::from).collect();
        let (p50, p95, p99, max) = quantiles_ms(&samples);
        assert!((p50 - 50.0).abs() < f64::EPSILON);
        assert!((p95 - 95.0).abs() < f64::EPSILON);
        assert!((p99 - 99.0).abs() < f64::EPSILON);
        assert!((max - 100.0).abs() < f64::EPSILON);
    }
}
