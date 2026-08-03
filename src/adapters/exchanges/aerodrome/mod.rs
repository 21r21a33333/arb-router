//! Aerodrome (Base) exchange adapters.
//!
//! Aerodrome is a Solidly / Velodrome descendant. It has three pool kinds, two
//! of which reuse the Uniswap quote math:
//!
//! - **v2 volatile** — constant-product, quoted by
//!   [`super::uniswap::v2::UniswapV2Pool`] (identical `getAmountOut`).
//! - **v2 stable** — the Solidly `x³y + y³x` curve; the only kind needing its
//!   own math, in [`stable::AerodromeStablePool`].
//! - **Slipstream** — a Uniswap V3 concentrated-liquidity fork, quoted by
//!   [`super::uniswap::v3::UniswapV3Pool`].
//!
//! Both the v2 factory (volatile + stable) and the Slipstream factory expose a
//! `getPool`, so discovery is on-chain (unlike Curve/V4).

pub mod slipstream_exchange;
pub mod stable;
pub mod v2_exchange;
