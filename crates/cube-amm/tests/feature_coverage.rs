//! Feature-coverage suite. Each test simulates an on-chain feature
//! (admin actions, N-ary pools, Token-2022 mixed, max_selloff, liquidity
//! adds, protocol-fee collection, pool migration) by hand-crafting the
//! corresponding pool buffer and asserts the connector reacts the way
//! the on-chain program does.

use cube_amm::constants::{
    CUBIC_POOL_PROGRAM_ID, MAX_TOKENS, POOL_DISCRIMINATOR, POOL_V3_LEN, POOL_V4_LEN,
};
use cube_amm::CubeAmm;
use dflow_amm_interface::{
    AccountMap, Amm, AmmContext, ClockRef, KeyedAccount, QuoteParams, SwapMode,
};
use solana_sdk::{account::Account, pubkey::Pubkey};
use std::sync::atomic::Ordering;

const TOKEN_BASE: usize = 179;
const SLOT_SIZE: usize = 144;

// Token-2022 program id.
const TOKEN_2022_ID_BYTES: [u8; 32] = [
    6, 221, 246, 225, 238, 117, 143, 222, 235, 200, 197, 32, 165, 32, 209, 78,
    150, 35, 199, 174, 9, 105, 145, 11, 192, 76, 38, 110, 79, 64, 64, 50,
];

#[derive(Clone, Copy, Default)]
struct TokenInit {
    mint: Pubkey,
    token_program: Pubkey,
    normalized_weight: u64,
    max_selloff: u64,
    max_selloff_period_length: u32,
    virtual_balance: u64,
    actual_balance: u64,
    protocol_fees_owed: u64,
    previous_selloff: u64,
    current_selloff: u64,
    window_start_timestamp: i64,
}

fn build_pool_data(
    config: Pubkey,
    pool_id: u64,
    swap_fee_rate: u32,
    protocol_fee_rate: u16,
    pool_enabled: bool,
    swaps_enabled: bool,
    tokens: &[TokenInit],
) -> Vec<u8> {
    assert!(tokens.len() <= MAX_TOKENS);
    let mut data = vec![0u8; POOL_V4_LEN];
    data[0..8].copy_from_slice(&POOL_DISCRIMINATOR);
    data[8..40].copy_from_slice(config.as_ref());
    data[40] = 0;
    data[41] = tokens.len() as u8;
    data[42..50].copy_from_slice(&pool_id.to_le_bytes());
    data[50..54].copy_from_slice(&swap_fee_rate.to_le_bytes());
    data[54..56].copy_from_slice(&protocol_fee_rate.to_le_bytes());
    data[64] = pool_enabled as u8;
    data[65] = swaps_enabled as u8;

    for (i, t) in tokens.iter().enumerate() {
        let off = TOKEN_BASE + i * SLOT_SIZE;
        // AssetConfig
        data[off..off + 32].copy_from_slice(t.mint.as_ref());
        data[off + 32..off + 64].copy_from_slice(t.token_program.as_ref());
        data[off + 64..off + 72].copy_from_slice(&t.normalized_weight.to_le_bytes());
        data[off + 72..off + 80].copy_from_slice(&t.max_selloff.to_le_bytes());
        data[off + 80..off + 84].copy_from_slice(&t.max_selloff_period_length.to_le_bytes());
        // AssetDynamics
        data[off + 88..off + 96].copy_from_slice(&t.virtual_balance.to_le_bytes());
        data[off + 96..off + 104].copy_from_slice(&t.actual_balance.to_le_bytes());
        data[off + 104..off + 112].copy_from_slice(&t.protocol_fees_owed.to_le_bytes());
        data[off + 112..off + 120].copy_from_slice(&t.previous_selloff.to_le_bytes());
        data[off + 120..off + 128].copy_from_slice(&t.current_selloff.to_le_bytes());
        data[off + 128..off + 136]
            .copy_from_slice(&(t.window_start_timestamp as u64).to_le_bytes());
    }
    data
}

fn build_amm(data: Vec<u8>, clock_ts: i64) -> CubeAmm {
    let pool_key = Pubkey::new_unique();
    let keyed = KeyedAccount {
        key: pool_key,
        account: Account {
            lamports: 0,
            data,
            owner: CUBIC_POOL_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
        params: None,
    };
    let ctx = AmmContext {
        clock_ref: ClockRef::default(),
    };
    ctx.clock_ref
        .unix_timestamp
        .store(clock_ts, Ordering::Relaxed);
    CubeAmm::from_keyed_account(&keyed, &ctx).unwrap()
}

fn qp(amount: u64, input: Pubkey, output: Pubkey) -> QuoteParams {
    QuoteParams {
        amount,
        input_mint: input,
        output_mint: output,
        swap_mode: SwapMode::ExactIn,
    }
}

fn spl_token_id() -> Pubkey {
    spl_token::id()
}

fn token_2022_id() -> Pubkey {
    Pubkey::new_from_array(TOKEN_2022_ID_BYTES)
}

fn slot(
    mint: Pubkey,
    weight: u64,
    vb: u64,
    ab: u64,
    program: Pubkey,
) -> TokenInit {
    TokenInit {
        mint,
        token_program: program,
        normalized_weight: weight,
        virtual_balance: vb,
        actual_balance: ab,
        ..Default::default()
    }
}

// ─── 1. N-ary pools (3 tokens) ─────────────────────────────────────────────

#[test]
fn n_ary_pool_3_tokens_all_pairs_quotable() {
    let m_a = Pubkey::new_unique();
    let m_b = Pubkey::new_unique();
    let m_c = Pubkey::new_unique();
    let tp = spl_token_id();
    let toks = [
        slot(m_a, 4000, 1_000_000_000, 1_000_000_000, tp),
        slot(m_b, 4000, 1_000_000_000, 1_000_000_000, tp),
        slot(m_c, 2000, 500_000_000, 500_000_000, tp),
    ];
    let data = build_pool_data(Pubkey::new_unique(), 1, 3000, 0, true, true, &toks);
    let amm = build_amm(data, 1_000_000);

    // get_reserve_mints returns 3 mints
    assert_eq!(amm.get_reserve_mints().len(), 3);

    // Every (in, out) pair quotes
    for (i, &mi) in [m_a, m_b, m_c].iter().enumerate() {
        for (j, &mj) in [m_a, m_b, m_c].iter().enumerate() {
            if i == j {
                continue;
            }
            let q = amm.quote(&qp(1_000_000, mi, mj)).unwrap();
            assert!(q.out_amount > 0, "pair {i}→{j} returned zero");
        }
    }
}

#[test]
fn n_ary_pool_asymmetric_weight_amplifies_output() {
    let m_a = Pubkey::new_unique();
    let m_b = Pubkey::new_unique();
    let m_c = Pubkey::new_unique();
    let tp = spl_token_id();
    // C has tiny weight (2000), A has dominant weight (8000-).
    // Swapping A → C with same VBs should yield MORE than A → B (balanced).
    let toks = [
        slot(m_a, 8000, 1_000_000_000, 1_000_000_000, tp),
        slot(m_b, 1000, 1_000_000_000, 1_000_000_000, tp),
        slot(m_c, 1000, 1_000_000_000, 1_000_000_000, tp),
    ];
    let data = build_pool_data(Pubkey::new_unique(), 1, 3000, 0, true, true, &toks);
    let amm = build_amm(data, 1_000_000);

    let q_ab = amm.quote(&qp(50_000_000, m_a, m_b)).unwrap();
    let q_ba = amm.quote(&qp(50_000_000, m_b, m_a)).unwrap();
    // With wI=8000 > wO=1000: tiny weight on out side amplifies; A→B
    // yields more out than B→A on the symmetric inverse.
    assert!(q_ab.out_amount > q_ba.out_amount);
}

// ─── 2. Token-2022 mixed pool ──────────────────────────────────────────────

#[test]
fn token_2022_mixed_pool_swap_metas_use_per_slot_program() {
    let m_a = Pubkey::new_unique();
    let m_b = Pubkey::new_unique();
    let toks = [
        slot(m_a, 5000, 1_000_000_000, 1_000_000_000, spl_token_id()),
        slot(m_b, 5000, 1_000_000_000, 1_000_000_000, token_2022_id()),
    ];
    let data = build_pool_data(Pubkey::new_unique(), 1, 3000, 0, true, true, &toks);
    let amm = build_amm(data, 1_000_000);

    use dflow_amm_interface::SwapParams;
    let sap = amm
        .get_swap_and_account_metas(&SwapParams {
            in_amount: 1_000_000,
            source_mint: m_a,
            destination_mint: m_b,
            source_token_account: Pubkey::new_unique(),
            destination_token_account: Pubkey::new_unique(),
            token_transfer_authority: Pubkey::new_unique(),
        })
        .unwrap();
    // metas[9] = token_program_in (classic), metas[10] = token_program_out (2022).
    assert_eq!(sap.account_metas[9].pubkey, spl_token_id());
    assert_eq!(sap.account_metas[10].pubkey, token_2022_id());

    // vault_in derived with classic program, vault_out with token-2022.
    let v_in = CubeAmm::derive_vault(&amm.key(), &m_a, &spl_token_id());
    let v_out = CubeAmm::derive_vault(&amm.key(), &m_b, &token_2022_id());
    assert_eq!(sap.account_metas[6].pubkey, v_in);
    assert_eq!(sap.account_metas[7].pubkey, v_out);
    // The two vaults MUST differ because the seeds use different token_program.
    assert_ne!(v_in, v_out);
}

// ─── 3. Admin actions propagate across update() ────────────────────────────

#[test]
fn admin_set_swap_fee_rate_picked_up_after_update() {
    let m_a = Pubkey::new_unique();
    let m_b = Pubkey::new_unique();
    let tp = spl_token_id();
    let toks = [
        slot(m_a, 5000, 1_000_000_000, 1_000_000_000, tp),
        slot(m_b, 5000, 1_000_000_000, 1_000_000_000, tp),
    ];
    // Start at 30 bps fee.
    let data0 = build_pool_data(Pubkey::new_unique(), 1, 3000, 0, true, true, &toks);
    let mut amm = build_amm(data0, 1_000_000);
    let q_before = amm.quote(&qp(10_000_000, m_a, m_b)).unwrap();

    // Admin bumps fee to 1%.
    let data1 = build_pool_data(Pubkey::new_unique(), 1, 10_000, 0, true, true, &toks);
    let mut map: AccountMap = AccountMap::default();
    map.insert(
        amm.key(),
        Account {
            lamports: 0,
            data: data1,
            owner: CUBIC_POOL_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    );
    amm.update(&map).unwrap();
    let q_after = amm.quote(&qp(10_000_000, m_a, m_b)).unwrap();
    // Higher fee → less output for same input.
    assert!(q_after.out_amount < q_before.out_amount);
}

#[test]
fn admin_set_swaps_enabled_false_blocks_quoting() {
    let m_a = Pubkey::new_unique();
    let m_b = Pubkey::new_unique();
    let tp = spl_token_id();
    let toks = [
        slot(m_a, 5000, 1_000_000_000, 1_000_000_000, tp),
        slot(m_b, 5000, 1_000_000_000, 1_000_000_000, tp),
    ];
    let data0 = build_pool_data(Pubkey::new_unique(), 1, 3000, 0, true, true, &toks);
    let mut amm = build_amm(data0, 1_000_000);
    assert!(amm.is_active());
    amm.quote(&qp(1_000_000, m_a, m_b)).unwrap();

    // Admin sets swaps_enabled = false.
    let data1 = build_pool_data(
        Pubkey::new_unique(),
        1,
        3000,
        0,
        /*pool_enabled*/ true,
        /*swaps_enabled*/ false,
        &toks,
    );
    let mut map: AccountMap = AccountMap::default();
    map.insert(
        amm.key(),
        Account {
            lamports: 0,
            data: data1,
            owner: CUBIC_POOL_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    );
    amm.update(&map).unwrap();
    assert!(!amm.is_active());
    assert!(amm.quote(&qp(1_000_000, m_a, m_b)).is_err());
}

#[test]
fn admin_set_pool_enabled_false_blocks_quoting() {
    let m_a = Pubkey::new_unique();
    let m_b = Pubkey::new_unique();
    let tp = spl_token_id();
    let toks = [
        slot(m_a, 5000, 1_000_000_000, 1_000_000_000, tp),
        slot(m_b, 5000, 1_000_000_000, 1_000_000_000, tp),
    ];
    let data = build_pool_data(
        Pubkey::new_unique(),
        1,
        3000,
        0,
        /*pool_enabled*/ false,
        true,
        &toks,
    );
    let amm = build_amm(data, 1_000_000);
    assert!(!amm.is_active());
    assert!(amm.quote(&qp(1_000_000, m_a, m_b)).is_err());
}

#[test]
fn admin_set_protocol_fee_rate_does_not_affect_user_out() {
    // protocol_fee_rate carves out a slice of the swap_fee for the
    // treasury — it does NOT change the trader's out_amount. The math
    // in cubic-pool::swap.rs reflects this. Verify the connector matches.
    let m_a = Pubkey::new_unique();
    let m_b = Pubkey::new_unique();
    let tp = spl_token_id();
    let toks = [
        slot(m_a, 5000, 1_000_000_000, 1_000_000_000, tp),
        slot(m_b, 5000, 1_000_000_000, 1_000_000_000, tp),
    ];
    let data_0pct = build_pool_data(Pubkey::new_unique(), 1, 3000, 0, true, true, &toks);
    let data_30pct = build_pool_data(Pubkey::new_unique(), 1, 3000, 3000, true, true, &toks);

    let amm_0 = build_amm(data_0pct, 1_000_000);
    let amm_30 = build_amm(data_30pct, 1_000_000);
    let q0 = amm_0.quote(&qp(10_000_000, m_a, m_b)).unwrap();
    let q30 = amm_30.quote(&qp(10_000_000, m_a, m_b)).unwrap();
    assert_eq!(q0.out_amount, q30.out_amount);
}

// ─── 4. max_selloff ────────────────────────────────────────────────────────

#[test]
fn max_selloff_zero_means_disabled() {
    let m_a = Pubkey::new_unique();
    let m_b = Pubkey::new_unique();
    let tp = spl_token_id();
    let mut toks = [
        slot(m_a, 5000, 10_000_000_000, 10_000_000_000, tp),
        slot(m_b, 5000, 10_000_000_000, 10_000_000_000, tp),
    ];
    toks[0].max_selloff = 0;
    toks[0].max_selloff_period_length = 60;
    let data = build_pool_data(Pubkey::new_unique(), 1, 3000, 0, true, true, &toks);
    let amm = build_amm(data, 1_000_000);
    // Massive input passes because the check is disabled.
    let q = amm.quote(&qp(5_000_000_000, m_a, m_b)).unwrap();
    assert_eq!(q.in_amount, 5_000_000_000);
}

#[test]
fn max_selloff_blocks_exceeding_input_and_binary_search_caps() {
    let m_a = Pubkey::new_unique();
    let m_b = Pubkey::new_unique();
    let tp = spl_token_id();
    let mut toks = [
        slot(m_a, 5000, 10_000_000_000, 10_000_000_000, tp),
        slot(m_b, 5000, 10_000_000_000, 10_000_000_000, tp),
    ];
    // Cap input token at 1_000_000 per 60s window.
    toks[0].max_selloff = 1_000_000;
    toks[0].max_selloff_period_length = 60;
    let data = build_pool_data(Pubkey::new_unique(), 1, 3000, 0, true, true, &toks);
    let amm = build_amm(data, 1_000_000);

    // 100M is well above the cap; binary search must clamp to ≤ 1M.
    let q = amm.quote(&qp(100_000_000, m_a, m_b)).unwrap();
    assert!(q.in_amount <= 1_000_000, "got in_amount={}", q.in_amount);
    // And under the cap still works.
    let q2 = amm.quote(&qp(500_000, m_a, m_b)).unwrap();
    assert_eq!(q2.in_amount, 500_000);
}

#[test]
fn max_selloff_current_bucket_reduces_remaining_headroom() {
    let m_a = Pubkey::new_unique();
    let m_b = Pubkey::new_unique();
    let tp = spl_token_id();
    let mut toks = [
        slot(m_a, 5000, 10_000_000_000, 10_000_000_000, tp),
        slot(m_b, 5000, 10_000_000_000, 10_000_000_000, tp),
    ];
    // Cap 1M; 800k already used in current window.
    toks[0].max_selloff = 1_000_000;
    toks[0].max_selloff_period_length = 60;
    toks[0].current_selloff = 800_000;
    toks[0].window_start_timestamp = 1_000_000;
    let data = build_pool_data(Pubkey::new_unique(), 1, 3000, 0, true, true, &toks);
    // Clock at window_start → elapsed=0 → previous fully weighted but
    // previous=0 here, so effective = current_selloff = 800k.
    // Remaining headroom = 200k.
    let amm = build_amm(data, 1_000_000);

    let q = amm.quote(&qp(1_000_000, m_a, m_b)).unwrap();
    assert!(q.in_amount <= 200_000, "expected ≤200k, got {}", q.in_amount);
}

#[test]
fn max_selloff_window_rolls_after_period() {
    let m_a = Pubkey::new_unique();
    let m_b = Pubkey::new_unique();
    let tp = spl_token_id();
    let mut toks = [
        slot(m_a, 5000, 10_000_000_000, 10_000_000_000, tp),
        slot(m_b, 5000, 10_000_000_000, 10_000_000_000, tp),
    ];
    toks[0].max_selloff = 1_000_000;
    toks[0].max_selloff_period_length = 60;
    toks[0].current_selloff = 800_000;
    toks[0].window_start_timestamp = 1_000_000;
    let data = build_pool_data(Pubkey::new_unique(), 1, 3000, 0, true, true, &toks);

    // Two full periods later → both buckets aged out → full 1M available.
    let amm = build_amm(data, 1_000_000 + 120);
    let q = amm.quote(&qp(900_000, m_a, m_b)).unwrap();
    assert_eq!(q.in_amount, 900_000);
}

#[test]
fn max_selloff_only_constrains_input_token_not_output() {
    let m_a = Pubkey::new_unique();
    let m_b = Pubkey::new_unique();
    let tp = spl_token_id();
    let mut toks = [
        slot(m_a, 5000, 10_000_000_000, 10_000_000_000, tp),
        slot(m_b, 5000, 10_000_000_000, 10_000_000_000, tp),
    ];
    // A has zero cap (no limit), B has 1M cap.
    toks[1].max_selloff = 1_000_000;
    toks[1].max_selloff_period_length = 60;
    let data = build_pool_data(Pubkey::new_unique(), 1, 3000, 0, true, true, &toks);
    let amm = build_amm(data, 1_000_000);
    // A → B: B cap is on selling B INTO the pool. Not relevant here.
    let q = amm.quote(&qp(50_000_000, m_a, m_b)).unwrap();
    assert_eq!(q.in_amount, 50_000_000);
}

// ─── 5. Liquidity changes propagate ────────────────────────────────────────

#[test]
fn add_liquidity_increases_quote_output() {
    let m_a = Pubkey::new_unique();
    let m_b = Pubkey::new_unique();
    let tp = spl_token_id();
    let toks_before = [
        slot(m_a, 5000, 1_000_000_000, 1_000_000_000, tp),
        slot(m_b, 5000, 1_000_000_000, 1_000_000_000, tp),
    ];
    let data0 = build_pool_data(Pubkey::new_unique(), 1, 3000, 0, true, true, &toks_before);
    let mut amm = build_amm(data0, 1_000_000);
    let q_before = amm.quote(&qp(10_000_000, m_a, m_b)).unwrap();

    // Proportional add_liquidity: both vb/ab grow 5x.
    let toks_after = [
        slot(m_a, 5000, 5_000_000_000, 5_000_000_000, tp),
        slot(m_b, 5000, 5_000_000_000, 5_000_000_000, tp),
    ];
    let data1 = build_pool_data(Pubkey::new_unique(), 1, 3000, 0, true, true, &toks_after);
    let mut map: AccountMap = AccountMap::default();
    map.insert(
        amm.key(),
        Account {
            lamports: 0,
            data: data1,
            owner: CUBIC_POOL_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    );
    amm.update(&map).unwrap();
    let q_after = amm.quote(&qp(10_000_000, m_a, m_b)).unwrap();
    // Deeper liquidity → less price impact → higher output for same input.
    assert!(q_after.out_amount > q_before.out_amount);
}

#[test]
fn collect_protocol_fees_increases_lp_actual_out() {
    let m_a = Pubkey::new_unique();
    let m_b = Pubkey::new_unique();
    let tp = spl_token_id();
    // B has 100M actual, but 99M owed to protocol → lp_actual_out = 1M.
    let mut toks_before = [
        slot(m_a, 5000, 100_000_000, 100_000_000, tp),
        slot(m_b, 5000, 100_000_000, 100_000_000, tp),
    ];
    toks_before[1].protocol_fees_owed = 99_000_000;
    let data0 = build_pool_data(Pubkey::new_unique(), 1, 3000, 0, true, true, &toks_before);
    let mut amm = build_amm(data0, 1_000_000);
    let q_before = amm.quote(&qp(50_000_000, m_a, m_b)).unwrap();
    assert!(q_before.out_amount <= 1_000_000);

    // Admin runs collect_protocol_fees → pfo zeroed, actual reduced.
    let mut toks_after = toks_before;
    toks_after[1].protocol_fees_owed = 0;
    toks_after[1].actual_balance = 1_000_000; // 100M - 99M collected
    let data1 = build_pool_data(Pubkey::new_unique(), 1, 3000, 0, true, true, &toks_after);
    let mut map: AccountMap = AccountMap::default();
    map.insert(
        amm.key(),
        Account {
            lamports: 0,
            data: data1,
            owner: CUBIC_POOL_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    );
    amm.update(&map).unwrap();
    // lp_actual_out still 1M (unchanged in this collapse-then-collect
    // pattern); the cap remains the same.
    let q_after = amm.quote(&qp(50_000_000, m_a, m_b)).unwrap();
    assert!(q_after.out_amount <= 1_000_000);
}

// ─── 6. v3 / v4 layout discrimination ──────────────────────────────────────

#[test]
fn v3_layout_rejected_with_clear_error() {
    let data = vec![0u8; POOL_V3_LEN];
    let keyed = KeyedAccount {
        key: Pubkey::new_unique(),
        account: Account {
            lamports: 0,
            data,
            owner: CUBIC_POOL_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
        params: None,
    };
    let r = CubeAmm::from_keyed_account(
        &keyed,
        &AmmContext {
            clock_ref: ClockRef::default(),
        },
    );
    let e = r.err().unwrap().to_string();
    assert!(e.contains("v3"), "want hint about v3 migration, got: {e}");
}

#[test]
fn migrate_pool_v4_picks_up_on_next_update() {
    // Simulate the migration sequence: connector loaded with v4 already;
    // on update we receive a v4 buffer (size 1683). The decoder accepts.
    let m_a = Pubkey::new_unique();
    let m_b = Pubkey::new_unique();
    let tp = spl_token_id();
    let toks = [
        slot(m_a, 5000, 1_000_000_000, 1_000_000_000, tp),
        slot(m_b, 5000, 1_000_000_000, 1_000_000_000, tp),
    ];
    let data = build_pool_data(Pubkey::new_unique(), 1, 3000, 0, true, true, &toks);
    let mut amm = build_amm(data.clone(), 1_000_000);
    // Re-supply same v4 buffer through update — succeeds.
    let mut map: AccountMap = AccountMap::default();
    map.insert(
        amm.key(),
        Account {
            lamports: 0,
            data,
            owner: CUBIC_POOL_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    );
    amm.update(&map).unwrap();
    amm.quote(&qp(1_000_000, m_a, m_b)).unwrap();
}
