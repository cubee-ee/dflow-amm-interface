//! `CubeAmm` — `dflow_amm_interface::Amm` impl for cubic-pool.
//!
//! Lifecycle (from research §"Runtime lifecycle"):
//!   1. engine calls `from_keyed_account` with the pool account
//!   2. engine calls `get_accounts_to_update` → we return `[pool]`
//!   3. engine fetches the listed accounts, calls `update(map)`
//!   4. engine calls `quote(params)` many times per `update`
//!   5. when routed, engine calls `get_swap_and_account_metas(params)` once
//!
//! Quote math is the exact swap.rs pipeline:
//!   - guard: pool_enabled && swaps_enabled && amount > 0
//!   - fee:   (fee, amount_after_fee) = apply_swap_fee(amount, swap_fee_rate)
//!   - cap:   lp_actual_out = saturating_sub(actual_balance_out, protocol_fees_owed_out)
//!   - curve: out = calc_out_given_in(vbI, wI, vbO, wO, after_fee, lp_actual_out)
//!
//! On input-cap: when the curve refuses the full input (vault drained),
//! we DON'T return Err — we halve and retry until we find a viable
//! in_amount, then return that cap-respecting Quote. Mirrors the dflow
//! convention "quote.in_amount <= quote_params.amount".

use crate::constants::{BPT_MINT_SEED, CUBIC_POOL_PROGRAM_ID, CUBIC_POOL_SEED};
use crate::ix::{build_swap_account_metas, encode_swap_ix_data, SwapAccounts};
use crate::math::cubic_math::calc_out_given_in;
use crate::math::fee::apply_swap_fee;
use crate::state::PoolState;
use anyhow::{anyhow, Result};
use dflow_amm_interface::{
    AccountMap, Amm, AmmContext, KeyedAccount, Quote, QuoteParams, Swap, SwapAndAccountMetas,
    SwapParams,
};
use solana_sdk::{instruction::Instruction, pubkey::Pubkey};
use std::collections::HashSet;

/// Connector instance for one cubic_pool pool.
#[derive(Clone)]
pub struct CubeAmm {
    /// Pool account pubkey.
    key: Pubkey,
    /// Program id taken from `account.owner` at construction time.
    program_id: Pubkey,
    /// Active mints (length = pool.token_count).
    reserve_mints: Vec<Pubkey>,
    /// Decoded pool state, refreshed each `update()`.
    state: PoolState,
}

impl CubeAmm {
    /// Derive the standard `cubic_pool` PDA for `(config, pool_id)`.
    pub fn derive_pool_pda(config: &Pubkey, pool_id: u64) -> Pubkey {
        Pubkey::find_program_address(
            &[CUBIC_POOL_SEED, config.as_ref(), &pool_id.to_le_bytes()],
            &CUBIC_POOL_PROGRAM_ID,
        )
        .0
    }

    /// Derive the BPT mint PDA for a pool.
    pub fn derive_bpt_mint(pool: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[BPT_MINT_SEED, pool.as_ref()], &CUBIC_POOL_PROGRAM_ID).0
    }

    /// Derive vault pubkey (associated token account of pool authority).
    pub fn derive_vault(pool: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
        spl_associated_token_account::get_associated_token_address_with_program_id(
            pool,
            mint,
            token_program,
        )
    }

    /// Helper to build a complete on-chain swap `Instruction` outside the
    /// `Amm` trait (handy for integration tests).
    pub fn build_swap_instruction(
        &self,
        user: Pubkey,
        amount_in: u64,
        minimum_amount_out: u64,
        token_in_index: u8,
        token_out_index: u8,
    ) -> Result<Instruction> {
        let in_slot = self
            .state
            .active_tokens()
            .get(token_in_index as usize)
            .ok_or_else(|| anyhow!("token_in_index OOB"))?;
        let out_slot = self
            .state
            .active_tokens()
            .get(token_out_index as usize)
            .ok_or_else(|| anyhow!("token_out_index OOB"))?;
        let user_token_in = spl_associated_token_account::get_associated_token_address_with_program_id(
            &user,
            &in_slot.mint,
            &in_slot.token_program,
        );
        let user_token_out = spl_associated_token_account::get_associated_token_address_with_program_id(
            &user,
            &out_slot.mint,
            &out_slot.token_program,
        );
        let acc = SwapAccounts {
            program_id: self.program_id,
            pool: self.key,
            token_mint_in: in_slot.mint,
            token_mint_out: out_slot.mint,
            user_token_in,
            user_token_out,
            vault_in: Self::derive_vault(&self.key, &in_slot.mint, &in_slot.token_program),
            vault_out: Self::derive_vault(&self.key, &out_slot.mint, &out_slot.token_program),
            user,
            token_program_in: in_slot.token_program,
            token_program_out: out_slot.token_program,
        };
        let metas = build_swap_account_metas(&acc);
        // strip leading program-id meta — Instruction.program_id is set separately
        let metas = metas.into_iter().skip(1).collect::<Vec<_>>();
        Ok(Instruction {
            program_id: self.program_id,
            accounts: metas,
            data: encode_swap_ix_data(amount_in, minimum_amount_out, token_in_index, token_out_index),
        })
    }
}

impl Amm for CubeAmm {
    fn from_keyed_account(keyed_account: &KeyedAccount, _ctx: &AmmContext) -> Result<Self> {
        let state = PoolState::decode(&keyed_account.account.data)?;
        let reserve_mints = state
            .active_tokens()
            .iter()
            .map(|t| t.mint)
            .collect::<Vec<_>>();
        Ok(Self {
            key: keyed_account.key,
            program_id: keyed_account.account.owner,
            reserve_mints,
            state,
        })
    }

    fn label(&self) -> String {
        "Cube".to_string()
    }

    fn program_id(&self) -> Pubkey {
        self.program_id
    }

    fn key(&self) -> Pubkey {
        self.key
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        self.reserve_mints.clone()
    }

    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        // The pool account carries virtual + actual balances + fees + flags
        // (kept in sync by the program on every swap / add_liquidity). We
        // don't need to re-read vaults separately.
        vec![self.key]
    }

    fn update(&mut self, account_map: &AccountMap) -> Result<()> {
        let acc = account_map
            .get(&self.key)
            .ok_or_else(|| anyhow!("CubeAmm: pool account missing from update map"))?;
        self.state = PoolState::decode(&acc.data)?;
        self.reserve_mints = self
            .state
            .active_tokens()
            .iter()
            .map(|t| t.mint)
            .collect();
        Ok(())
    }

    fn quote(&self, qp: &QuoteParams) -> Result<Quote> {
        if !self.state.pool_enabled || !self.state.swaps_enabled {
            return Err(anyhow!("CubeAmm: pool disabled / swaps disabled"));
        }
        if qp.amount == 0 {
            return Err(anyhow!("CubeAmm: zero amount"));
        }
        let in_idx = self
            .state
            .index_of_mint(&qp.input_mint)
            .ok_or_else(|| anyhow!("CubeAmm: input mint not in pool"))?;
        let out_idx = self
            .state
            .index_of_mint(&qp.output_mint)
            .ok_or_else(|| anyhow!("CubeAmm: output mint not in pool"))?;
        if in_idx == out_idx {
            return Err(anyhow!("CubeAmm: input == output mint"));
        }

        let in_slot = &self.state.tokens[in_idx];
        let out_slot = &self.state.tokens[out_idx];
        let lp_actual_out = out_slot
            .actual_balance
            .saturating_sub(out_slot.protocol_fees_owed);

        // If the full input is viable, return it. Otherwise binary-search
        // for the largest in_amount the LP-actual cap allows (dflow
        // convention: quote.in_amount <= qp.amount).
        if let Ok(out_amount) = self.try_quote(in_slot, out_slot, lp_actual_out, qp.amount) {
            return Ok(Quote {
                in_amount: qp.amount,
                out_amount,
            });
        }
        let mut lo: u64 = 0;
        let mut hi: u64 = qp.amount;
        let mut best: Option<(u64, u64)> = None;
        // log2(u64::MAX) ≈ 64; cap iterations defensively.
        for _ in 0..64 {
            if hi <= lo + 1 {
                break;
            }
            let mid = lo + (hi - lo) / 2;
            match self.try_quote(in_slot, out_slot, lp_actual_out, mid) {
                Ok(out) => {
                    best = Some((mid, out));
                    lo = mid;
                }
                Err(_) => {
                    hi = mid;
                }
            }
        }
        match best {
            Some((in_amount, out_amount)) => Ok(Quote {
                in_amount,
                out_amount,
            }),
            None => Err(anyhow!("CubeAmm: no viable quote at any sub-amount")),
        }
    }

    fn get_swap_and_account_metas(&self, sp: &SwapParams) -> Result<SwapAndAccountMetas> {
        let in_idx = self
            .state
            .index_of_mint(&sp.source_mint)
            .ok_or_else(|| anyhow!("CubeAmm: source mint not in pool"))?;
        let out_idx = self
            .state
            .index_of_mint(&sp.destination_mint)
            .ok_or_else(|| anyhow!("CubeAmm: destination mint not in pool"))?;
        let in_slot = &self.state.tokens[in_idx];
        let out_slot = &self.state.tokens[out_idx];

        let acc = SwapAccounts {
            program_id: self.program_id,
            pool: self.key,
            token_mint_in: in_slot.mint,
            token_mint_out: out_slot.mint,
            user_token_in: sp.source_token_account,
            user_token_out: sp.destination_token_account,
            vault_in: Self::derive_vault(&self.key, &in_slot.mint, &in_slot.token_program),
            vault_out: Self::derive_vault(&self.key, &out_slot.mint, &out_slot.token_program),
            user: sp.token_transfer_authority,
            token_program_in: in_slot.token_program,
            token_program_out: out_slot.token_program,
        };
        Ok(SwapAndAccountMetas {
            swap: Swap::Placeholder,
            account_metas: build_swap_account_metas(&acc),
        })
    }

    fn get_accounts_len(&self) -> usize {
        // 10 swap accounts + 1 leading program id (dflow convention).
        11
    }

    fn is_active(&self) -> bool {
        if !self.state.pool_enabled || !self.state.swaps_enabled {
            return false;
        }
        // Reject pools where every active token has zero LP-accessible balance.
        self.state
            .active_tokens()
            .iter()
            .any(|t| t.actual_balance.saturating_sub(t.protocol_fees_owed) > 0)
    }

    fn underlying_liquidities(&self) -> Option<HashSet<Pubkey>> {
        None
    }

    fn program_dependencies(&self) -> Vec<(Pubkey, String)> {
        vec![(self.program_id, "cubic_pool".to_string())]
    }

    fn clone_amm(&self) -> Box<dyn Amm + Send + Sync> {
        Box::new(self.clone())
    }
}

impl CubeAmm {
    fn try_quote(
        &self,
        in_slot: &crate::state::TokenSlot,
        out_slot: &crate::state::TokenSlot,
        lp_actual_out: u64,
        amount: u64,
    ) -> Result<u64> {
        let (_fee, after) = apply_swap_fee(amount, self.state.swap_fee_rate)?;
        if after == 0 {
            return Err(anyhow!("CubeAmm: amount_in_after_fee = 0"));
        }
        calc_out_given_in(
            in_slot.virtual_balance,
            in_slot.normalized_weight,
            out_slot.virtual_balance,
            out_slot.normalized_weight,
            after,
            lp_actual_out,
        )
    }
}
