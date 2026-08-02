//! Configuration for a scan engine.
//!
//! v1 keeps the surface minimal: a deployment lists the assets to scan from, a
//! single USD size to quote every path at, and how deep to search. Traversal
//! safety limits and the freshness window are documented defaults here, promoted
//! to an `EngineConfig` field when tuning one becomes a real requirement.

use std::time::Duration;

use rust_decimal::Decimal;

use crate::primitives::asset::{AssetId, Usd};

/// The knobs a v1 deployment varies.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// Assets each scan starts (and, for cycles, ends) from.
    pub start_assets: Vec<AssetId>,
    /// Maximum path length in hops.
    pub max_hops: usize,
    /// USD value quoted into every path, sized per start asset from its price.
    pub input_usd: Usd,
}

impl EngineConfig {
    /// A config over `start_assets` at the default depth and input size.
    pub fn new(start_assets: Vec<AssetId>) -> Self {
        Self {
            start_assets,
            max_hops: DEFAULT_MAX_HOPS,
            input_usd: Usd(Decimal::from(DEFAULT_INPUT_USD)),
        }
    }
}

// ─── v1 defaults ────────────────────────────────────────────────────────────
// Each of these is a candidate `EngineConfig` field. Kept as a constant until
// tuning it is an actual requirement.

/// Experimental default search depth.
pub const DEFAULT_MAX_HOPS: usize = 16;

/// Default USD value quoted into each path.
pub const DEFAULT_INPUT_USD: u32 = 1_000;

/// Edges expanded per node at each traversal depth.
pub const BEAM_WIDTH: usize = 64;

/// Pools synced longer ago than this are rejected as stale.
pub const MAX_POOL_STALENESS: Duration = Duration::from_secs(60);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_experimental_depth_and_input() {
        let cfg = EngineConfig::new(vec![AssetId::new("ethereum:usdc").unwrap()]);
        assert_eq!(cfg.max_hops, 16);
        assert_eq!(cfg.input_usd, Usd(Decimal::from(1000)));
        assert_eq!(cfg.start_assets.len(), 1);
    }
}
