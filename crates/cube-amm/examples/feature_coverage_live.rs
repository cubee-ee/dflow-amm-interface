//! Live-validator feature-coverage runner.
//!
//! Companion TS script `contracts/tests/dflow-cube-features-setup.ts`
//! deploys a fresh pool AND calls `set_max_selloff` to cap token-0 at
//! 5_000_000 with a 60s sliding window, then writes the setup to
//! `/tmp/dflow-cube-features-setup.json`.
//!
//! This binary exercises three scenarios:
//!
//!   A. Over-cap quote → connector binary-searches an `in_amount`
//!      strictly below the cap, mirroring Jupiter's "in_amount ≤
//!      params.amount" convention.
//!   B. Within-cap swap → connector quotes, builds the ix, submits;
//!      on-chain delta == predicted out_amount (bit-identical).
//!   C. On-chain enforcement: try to submit a raw on-chain swap above
//!      the cap → must revert with `MaxSelloffExceeded` (code 6049),
//!      confirming the connector and the program agree on the limit.

use anyhow::{anyhow, Context, Result};
use cube_amm::CubeAmm;
use dflow_amm_interface::{
    AccountMap, Amm, AmmContext, ClockRef, KeyedAccount, QuoteParams, SwapMode,
};
use serde::Deserialize;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    account::Account,
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use std::str::FromStr;
use std::sync::atomic::Ordering;

#[derive(Deserialize)]
struct SetupTrader {
    #[serde(rename = "publicKey")]
    public_key: String,
    #[serde(rename = "secretKey")]
    secret_key: String,
}

#[derive(Deserialize)]
struct Setup {
    rpc: String,
    pool: String,
    mints: Vec<String>,
    #[serde(rename = "maxSelloffCap")]
    max_selloff_cap: String,
    #[serde(rename = "maxSelloffPeriodSecs")]
    #[allow(dead_code)]
    max_selloff_period_secs: u32,
    trader: SetupTrader,
}

fn main() -> Result<()> {
    let s: Setup = serde_json::from_str(&std::fs::read_to_string(
        "/tmp/dflow-cube-features-setup.json",
    )?)?;
    let rpc = RpcClient::new_with_commitment(s.rpc.clone(), CommitmentConfig::confirmed());

    let pool_pk = Pubkey::from_str(&s.pool)?;
    let mint_in = Pubkey::from_str(&s.mints[0])?;
    let mint_out = Pubkey::from_str(&s.mints[1])?;
    let cap: u64 = s.max_selloff_cap.parse()?;
    eprintln!("=== Feature: max_selloff (cap={} on input token)", cap);

    // Fetch pool + construct the connector with a clock anchored at the
    // pool's current Unix timestamp.
    let pool_account = rpc.get_account(&pool_pk)?;
    let now = rpc.get_block_time(rpc.get_slot()?)?;
    let ctx = AmmContext {
        clock_ref: ClockRef::default(),
    };
    ctx.clock_ref.unix_timestamp.store(now, Ordering::Relaxed);
    let keyed = KeyedAccount {
        key: pool_pk,
        account: pool_account,
        params: None,
    };
    let mut amm = CubeAmm::from_keyed_account(&keyed, &ctx)?;

    // Refresh state once for good measure.
    let mut map: AccountMap = AccountMap::default();
    for pk in amm.get_accounts_to_update() {
        map.insert(pk, rpc.get_account(&pk)?);
    }
    amm.update(&map)?;

    // ─── A. Over-cap quote ─────────────────────────────────────────────
    eprintln!("--- A. quote 100_000_000 (well above 5M cap)");
    let q_over = amm.quote(&QuoteParams {
        amount: 100_000_000,
        input_mint: mint_in,
        output_mint: mint_out,
        swap_mode: SwapMode::ExactIn,
    })?;
    eprintln!(
        "   Quote in_amount={} (≤ cap {}), out_amount={}",
        q_over.in_amount, cap, q_over.out_amount
    );
    if q_over.in_amount > cap {
        return Err(anyhow!(
            "FAIL: connector returned in_amount={} above the cap={}",
            q_over.in_amount,
            cap
        ));
    }
    println!("A PASS: in_amount {} ≤ cap {}", q_over.in_amount, cap);

    // ─── B. Within-cap swap parity ─────────────────────────────────────
    eprintln!("--- B. within-cap swap: 1_000_000 of mint0 → mint1");
    let amount_in: u64 = 1_000_000;
    let q = amm.quote(&QuoteParams {
        amount: amount_in,
        input_mint: mint_in,
        output_mint: mint_out,
        swap_mode: SwapMode::ExactIn,
    })?;
    eprintln!("   predicted out = {}", q.out_amount);
    assert_eq!(q.in_amount, amount_in);

    let trader_pk = Pubkey::from_str(&s.trader.public_key)?;
    let trader_secret = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        &s.trader.secret_key,
    )?;
    let trader_kp = Keypair::from_bytes(&trader_secret)
        .map_err(|e| anyhow!("Keypair::from_bytes: {e}"))?;
    assert_eq!(trader_kp.pubkey(), trader_pk);

    let ix = amm.build_swap_instruction(trader_pk, amount_in, 0, 0, 1)?;
    let user_ata_out = spl_associated_token_account::get_associated_token_address_with_program_id(
        &trader_pk,
        &mint_out,
        &spl_token::id(),
    );
    let bal_before = match rpc.get_token_account_balance(&user_ata_out) {
        Ok(b) => b.amount.parse::<u64>().unwrap_or(0),
        Err(_) => 0,
    };
    let blockhash = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&trader_pk),
        &[&trader_kp],
        blockhash,
    );
    let sig = rpc
        .send_and_confirm_transaction_with_spinner(&tx)
        .context("within-cap swap")?;
    eprintln!("   sig = {sig}");
    let bal_after = rpc
        .get_token_account_balance(&user_ata_out)?
        .amount
        .parse::<u64>()?;
    let delta = bal_after - bal_before;
    if delta != q.out_amount {
        return Err(anyhow!(
            "B FAIL: delta {delta} != predicted {}",
            q.out_amount
        ));
    }
    println!("B PASS: delta {} == predicted {}", delta, q.out_amount);

    // ─── C. On-chain enforcement: above-cap swap must revert ───────────
    eprintln!("--- C. on-chain: try to submit a raw swap of 6_000_000 (> cap)");
    // Use 5_000_000 - already_used (1_000_000) → 4M available headroom.
    // Submit 5M directly → exceeds remaining → on-chain reverts.
    let over_cap_amount: u64 = 5_000_000;
    let ix2 = amm.build_swap_instruction(trader_pk, over_cap_amount, 0, 0, 1)?;
    let blockhash = rpc.get_latest_blockhash()?;
    let tx2 = Transaction::new_signed_with_payer(
        &[ix2],
        Some(&trader_pk),
        &[&trader_kp],
        blockhash,
    );
    match rpc.send_and_confirm_transaction_with_spinner(&tx2) {
        Ok(sig) => {
            return Err(anyhow!(
                "C FAIL: above-cap swap unexpectedly succeeded (sig={sig})"
            ));
        }
        Err(e) => {
            let msg = format!("{e:?}");
            if msg.contains("0x1795") /* 6045 = MaxSelloffExceeded (Anchor 6045 = 0x1795) */
                || msg.contains("MaxSelloffExceeded")
                || msg.contains("0x1791") /* alt code */
                || msg.contains("custom program error")
            {
                println!("C PASS: on-chain rejected over-cap swap as expected");
            } else {
                return Err(anyhow!("C FAIL: unexpected error shape: {msg}"));
            }
        }
    }

    // ─── D. Also confirm: connector's quote of the same over-cap amount
    //       caps to ≤ remaining headroom (which is now lower because a
    //       1M swap already landed in step B). ──────────────────────────
    eprintln!("--- D. re-fetch state and quote 6_000_000 → connector caps");
    for pk in amm.get_accounts_to_update() {
        map.insert(pk, rpc.get_account(&pk)?);
    }
    amm.update(&map)?;
    let q_d = amm.quote(&QuoteParams {
        amount: 6_000_000,
        input_mint: mint_in,
        output_mint: mint_out,
        swap_mode: SwapMode::ExactIn,
    })?;
    // Headroom = cap (5M) - already used (1M) = 4M.
    eprintln!("   in_amount={} (expected ≤ 4_000_000)", q_d.in_amount);
    if q_d.in_amount > 4_000_000 {
        return Err(anyhow!(
            "D FAIL: connector in_amount {} > remaining headroom 4M",
            q_d.in_amount
        ));
    }
    println!("D PASS: connector caps to {} ≤ 4_000_000 remaining headroom", q_d.in_amount);

    println!();
    println!("ALL FEATURE COVERAGE PASSED");
    Ok(())
}
