#![forbid(unsafe_code)]
//! Executable, independently observed acceptance and benchmark evidence.
pub mod cooperative;
pub mod external;
pub mod fixture;
pub mod managed;
pub mod mcp;
pub mod provenance;
pub mod runner;
pub mod suite;
pub mod workflows;

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Distribution {
    pub samples_ns: Vec<u64>,
    pub p50_ns: u64,
    pub p95_ns: u64,
}

/// Nearest-rank percentiles retain raw samples; no timing threshold is an assertion.
pub fn distribution(samples_ns: Vec<u64>) -> Option<Distribution> {
    if samples_ns.is_empty() {
        return None;
    }
    let mut sorted = samples_ns.clone();
    sorted.sort_unstable();
    let rank = |percent: usize| sorted[(sorted.len() * percent).div_ceil(100) - 1];
    Some(Distribution {
        p50_ns: rank(50),
        p95_ns: rank(95),
        samples_ns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn percentiles_keep_real_outlier_and_original_order() {
        let result = distribution(vec![100, 2, 3, 4, 5]).expect("nonempty samples");
        assert_eq!(result.p50_ns, 4);
        assert_eq!(result.p95_ns, 100);
        assert_eq!(result.samples_ns, [100, 2, 3, 4, 5]);
        assert!(distribution(vec![]).is_none());
    }
}
