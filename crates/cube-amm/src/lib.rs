//! Cube DEX (cubic-pool) connector for `dflow-amm-interface`.
//!
//! See README for usage. Public surface:
//!   - `CubeAmm` — the `Amm` impl
//!   - `constants::CUBIC_POOL_PROGRAM_ID`
//!   - `state::PoolState` for decoding pool accounts manually
//!   - `ix::{encode_swap_ix_data, build_swap_account_metas, SwapAccounts}` for
//!     building swap instructions outside the trait

pub mod amm;
pub mod constants;
pub mod ix;
pub mod math;
pub mod state;

pub use amm::CubeAmm;
pub use constants::CUBIC_POOL_PROGRAM_ID;
