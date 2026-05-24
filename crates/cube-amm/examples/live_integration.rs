//! End-to-end test against a live `solana-test-validator`.
//!
//! Expects /tmp/dflow-cube-setup.json (produced by
//! contracts/tests/dflow-cube-amm-setup.ts) describing the freshly
//! deployed pool, mints, vaults, and trader keypair.
//!
//! Steps:
//!   1. Fetch pool account from local RPC.
//!   2. Construct `CubeAmm` via `from_keyed_account`.
//!   3. Run `get_accounts_to_update` → fetch accounts → `update`.
//!   4. `quote(ExactIn)` for token_in=0 → token_out=1, amount=10_000_000.
//!   5. Build the swap `Instruction` via the connector.
//!   6. Submit the swap on-chain.
//!   7. Read trader's destination ATA before/after; compare delta to quote.

use anyhow::{anyhow, Context, Result};
use cube_amm::CubeAmm;
use dflow_amm_interface::{
    AccountMap, Amm, AmmContext, ClockRef, KeyedAccount, QuoteParams, SwapMode, SwapParams,
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
    #[allow(dead_code)]
    vaults: Vec<String>,
    trader: SetupTrader,
}

fn main() -> Result<()> {
    let s: Setup = serde_json::from_str(&std::fs::read_to_string("/tmp/dflow-cube-setup.json")?)?;
    let rpc = RpcClient::new_with_commitment(s.rpc.clone(), CommitmentConfig::confirmed());

    let pool_pk = Pubkey::from_str(&s.pool)?;
    let mint_in = Pubkey::from_str(&s.mints[0])?;
    let mint_out = Pubkey::from_str(&s.mints[1])?;

    eprintln!("=== Step 1: fetch pool {pool_pk}");
    let pool_account = rpc
        .get_account(&pool_pk)
        .with_context(|| format!("rpc.get_account({pool_pk})"))?;

    eprintln!(
        "pool data.len() = {} (expected 1683 for v4)",
        pool_account.data.len()
    );

    eprintln!("=== Step 2: from_keyed_account");
    let keyed = KeyedAccount {
        key: pool_pk,
        account: pool_account.clone(),
        params: None,
    };
    let ctx = AmmContext {
        clock_ref: ClockRef::default(),
    };
    let mut amm = CubeAmm::from_keyed_account(&keyed, &ctx)?;
    eprintln!("  label = {}", amm.label());
    eprintln!("  program_id = {}", amm.program_id());
    eprintln!("  reserve_mints = {:?}", amm.get_reserve_mints());

    eprintln!("=== Step 3: refresh state via update()");
    let to_update = amm.get_accounts_to_update();
    eprintln!("  accounts_to_update = {:?}", to_update);
    let mut map: AccountMap = AccountMap::default();
    for pk in &to_update {
        let a = rpc.get_account(pk)?;
        map.insert(*pk, a);
    }
    amm.update(&map)?;

    eprintln!("=== Step 4: quote 10_000_000 of mint0 → mint1");
    let amount_in: u64 = 10_000_000;
    let quote = amm.quote(&QuoteParams {
        amount: amount_in,
        input_mint: mint_in,
        output_mint: mint_out,
        swap_mode: SwapMode::ExactIn,
    })?;
    eprintln!(
        "  Quote {{ in_amount: {}, out_amount: {} }}",
        quote.in_amount, quote.out_amount
    );

    eprintln!("=== Step 5: build swap Instruction");
    let trader_pk = Pubkey::from_str(&s.trader.public_key)?;
    let trader_secret =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &s.trader.secret_key)?;
    let trader_kp = Keypair::from_bytes(&trader_secret)
        .map_err(|e| anyhow!("Keypair::from_bytes: {e}"))?;
    assert_eq!(trader_kp.pubkey(), trader_pk);

    // Lookup token_program for each side via decoded state.
    let in_idx = 0u8;
    let out_idx = 1u8;
    // Generous slippage: use 0 as min_out so the swap can't fail on slippage —
    // the parity check below is on the actual delta vs the predicted out_amount.
    let ix = amm.build_swap_instruction(trader_pk, amount_in, 0, in_idx, out_idx)?;
    eprintln!(
        "  Ix data.len() = {}, accounts = {}",
        ix.data.len(),
        ix.accounts.len()
    );

    let user_ata_out = spl_associated_token_account::get_associated_token_address_with_program_id(
        &trader_pk,
        &mint_out,
        &spl_token::id(),
    );
    eprintln!("=== Step 6a: read trader's out-ATA balance BEFORE swap");
    let bal_before = match rpc.get_token_account_balance(&user_ata_out) {
        Ok(b) => b.amount.parse::<u64>().unwrap_or(0),
        Err(_) => {
            eprintln!("  (ATA not initialized yet → before = 0)");
            0
        }
    };
    eprintln!("  balance_before = {bal_before}");

    eprintln!("=== Step 6b: submit swap transaction");
    let blockhash = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&trader_pk),
        &[&trader_kp],
        blockhash,
    );
    let sig = rpc
        .send_and_confirm_transaction_with_spinner(&tx)
        .context("send_and_confirm swap tx")?;
    eprintln!("  signature = {sig}");

    eprintln!("=== Step 7: compare on-chain delta to predicted out_amount");
    let bal_after = rpc
        .get_token_account_balance(&user_ata_out)?
        .amount
        .parse::<u64>()
        .unwrap();
    eprintln!("  balance_after  = {bal_after}");
    let delta = bal_after.saturating_sub(bal_before);
    eprintln!("  on-chain delta = {delta}");
    eprintln!("  predicted out  = {}", quote.out_amount);

    if delta == quote.out_amount {
        println!("PARITY OK: delta == predicted ({delta})");
    } else {
        let diff = delta.abs_diff(quote.out_amount);
        let tolerance = quote.out_amount / 1_000_000; // 1 ppm
        if diff <= tolerance {
            println!(
                "PARITY OK (within 1ppm): delta={delta}, predicted={}, diff={diff}",
                quote.out_amount
            );
        } else {
            return Err(anyhow!(
                "PARITY FAIL: on-chain delta {delta} differs from predicted {} by {diff}",
                quote.out_amount
            ));
        }
    }

    let _ = SwapParams {
        in_amount: amount_in,
        source_mint: mint_in,
        destination_mint: mint_out,
        source_token_account: Pubkey::default(),
        destination_token_account: user_ata_out,
        token_transfer_authority: trader_pk,
    };

    Ok(())
}
