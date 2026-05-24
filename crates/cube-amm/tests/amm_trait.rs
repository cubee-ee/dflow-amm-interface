//! End-to-end tests of the `Amm` trait surface against a synthetic
//! cubic-pool v4 account.
//!
//! We hand-construct a 1683-byte pool buffer with three tokens (50/50/0
//! — really just two active tokens, third slot zeroed), then drive the
//! full lifecycle:
//!   - `from_keyed_account`
//!   - `get_reserve_mints`
//!   - `get_accounts_to_update`
//!   - `update`
//!   - `quote` (both directions, plus oversized → in_amount cap)
//!   - `get_swap_and_account_metas`

use cube_amm::constants::{
    CUBIC_POOL_PROGRAM_ID, MAX_TOKENS, POOL_DISCRIMINATOR, POOL_V4_LEN, SWAP_IX_DISCRIMINATOR,
};
use cube_amm::CubeAmm;
use dflow_amm_interface::{
    AccountMap, Amm, AmmContext, ClockRef, KeyedAccount, QuoteParams, Swap, SwapMode, SwapParams,
};
use solana_sdk::{account::Account, pubkey::Pubkey};

const TOKEN_BASE: usize = 179;
const SLOT_SIZE: usize = 144;

#[derive(Clone, Copy)]
struct TokenInit {
    mint: Pubkey,
    token_program: Pubkey,
    normalized_weight: u64,
    virtual_balance: u64,
    actual_balance: u64,
    protocol_fees_owed: u64,
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
    data[40] = 0; // bump
    data[41] = tokens.len() as u8;
    data[42..50].copy_from_slice(&pool_id.to_le_bytes());
    data[50..54].copy_from_slice(&swap_fee_rate.to_le_bytes());
    data[54..56].copy_from_slice(&protocol_fee_rate.to_le_bytes());
    // created_at @ 56..64 left zero
    data[64] = pool_enabled as u8;
    data[65] = swaps_enabled as u8;
    // admin/range_manager block @ 66..179 left zero

    for (i, t) in tokens.iter().enumerate() {
        let off = TOKEN_BASE + i * SLOT_SIZE;
        data[off..off + 32].copy_from_slice(t.mint.as_ref());
        data[off + 32..off + 64].copy_from_slice(t.token_program.as_ref());
        data[off + 64..off + 72].copy_from_slice(&t.normalized_weight.to_le_bytes());
        // max_selloff (8B), max_selloff_period_length (4B), reserved (4B) → zero
        data[off + 88..off + 96].copy_from_slice(&t.virtual_balance.to_le_bytes());
        data[off + 96..off + 104].copy_from_slice(&t.actual_balance.to_le_bytes());
        data[off + 104..off + 112].copy_from_slice(&t.protocol_fees_owed.to_le_bytes());
        // remaining 32B of dynamics → zero
    }

    data
}

fn fixture() -> (CubeAmm, [Pubkey; 2]) {
    let config = Pubkey::new_unique();
    let mint_a = Pubkey::new_unique();
    let mint_b = Pubkey::new_unique();
    let token_program = spl_token::id();
    let tokens = [
        TokenInit {
            mint: mint_a,
            token_program,
            normalized_weight: 5_000,
            virtual_balance: 1_000_000_000,
            actual_balance: 1_000_000_000,
            protocol_fees_owed: 0,
        },
        TokenInit {
            mint: mint_b,
            token_program,
            normalized_weight: 5_000,
            virtual_balance: 2_000_000_000,
            actual_balance: 2_000_000_000,
            protocol_fees_owed: 0,
        },
    ];
    let data = build_pool_data(config, 1, 3_000, 2_000, true, true, &tokens);
    let pool_key = CubeAmm::derive_pool_pda(&config, 1);
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
    let amm = CubeAmm::from_keyed_account(&keyed, &ctx).unwrap();
    (amm, [mint_a, mint_b])
}

#[test]
fn lifecycle_smoke() {
    let (amm, mints) = fixture();
    assert_eq!(amm.label(), "Cube");
    assert_eq!(amm.program_id(), CUBIC_POOL_PROGRAM_ID);
    assert_eq!(amm.get_reserve_mints(), mints.to_vec());
    assert_eq!(amm.get_accounts_to_update().len(), 1);
    assert_eq!(amm.get_accounts_len(), 11);
    assert!(amm.is_active());
    assert!(amm.program_dependencies().len() == 1);
}

#[test]
fn quote_matches_worked_example() {
    let (amm, [a, b]) = fixture();
    let q = amm
        .quote(&QuoteParams {
            amount: 10_000_000,
            input_mint: a,
            output_mint: b,
            swap_mode: SwapMode::ExactIn,
        })
        .unwrap();
    assert_eq!(q.in_amount, 10_000_000);
    // Worked example expected ≈ 19_743_160 (case A from contracts research).
    assert!(
        q.out_amount.abs_diff(19_743_160) < 10_000,
        "out_amount = {}",
        q.out_amount
    );
}

#[test]
fn quote_reverse_direction() {
    let (amm, [a, b]) = fixture();
    // Token B has 2x the virtual balance; with same weights, swapping 20M of B
    // should approximate the symmetric output we'd get the other way.
    let q = amm
        .quote(&QuoteParams {
            amount: 20_000_000,
            input_mint: b,
            output_mint: a,
            swap_mode: SwapMode::ExactIn,
        })
        .unwrap();
    assert_eq!(q.in_amount, 20_000_000);
    // Symmetric to the forward case: ~9_871_580 (half of 19_743_160).
    assert!(q.out_amount > 9_700_000 && q.out_amount < 10_000_000, "out_amount = {}", q.out_amount);
}

#[test]
fn quote_oversized_input_caps_via_halving() {
    // Custom fixture where lp_actual_out is much smaller than virtual_balance_out
    // (heavy protocol_fees_owed), so the curve's natural asymptote exceeds the
    // LP-accessible cap and the connector must halve.
    let config = Pubkey::new_unique();
    let mint_a = Pubkey::new_unique();
    let mint_b = Pubkey::new_unique();
    let tp = spl_token::id();
    let tokens = [
        TokenInit {
            mint: mint_a,
            token_program: tp,
            normalized_weight: 5_000,
            virtual_balance: 1_000_000_000,
            actual_balance: 1_000_000_000,
            protocol_fees_owed: 0,
        },
        TokenInit {
            mint: mint_b,
            token_program: tp,
            normalized_weight: 5_000,
            virtual_balance: 1_000_000_000_000, // huge virtual — curve eager to pay
            actual_balance: 1_000_000_000,
            protocol_fees_owed: 999_000_000, // lp_actual = 1_000_000
        },
    ];
    let data = build_pool_data(config, 9, 3_000, 0, true, true, &tokens);
    let pool_key = CubeAmm::derive_pool_pda(&config, 9);
    let amm = CubeAmm::from_keyed_account(
        &KeyedAccount {
            key: pool_key,
            account: Account {
                lamports: 0,
                data,
                owner: CUBIC_POOL_PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            },
            params: None,
        },
        &AmmContext {
            clock_ref: ClockRef::default(),
        },
    )
    .unwrap();
    let q = amm
        .quote(&QuoteParams {
            amount: 1_000_000_000_000,
            input_mint: mint_a,
            output_mint: mint_b,
            swap_mode: SwapMode::ExactIn,
        })
        .unwrap();
    assert!(q.in_amount > 0);
    assert!(q.in_amount <= 1_000_000_000_000);
    assert!(q.out_amount > 0);
    // lp_actual_out = 1_000_000_000 - 999_000_000 = 1_000_000.
    assert!(q.out_amount <= 1_000_000, "out_amount = {} exceeds lp_actual cap", q.out_amount);
    // For the full 1e12 input the curve would saturate around vbO=1e12, far above
    // the cap → in_amount must have been clamped well below the request.
    assert!(q.in_amount < 1_000_000_000_000, "in_amount = {} not capped", q.in_amount);
}

#[test]
fn quote_rejects_unknown_mint() {
    let (amm, [a, _]) = fixture();
    let bogus = Pubkey::new_unique();
    let r = amm.quote(&QuoteParams {
        amount: 1_000_000,
        input_mint: a,
        output_mint: bogus,
        swap_mode: SwapMode::ExactIn,
    });
    assert!(r.is_err());
}

#[test]
fn quote_zero_amount_errors() {
    let (amm, [a, b]) = fixture();
    let r = amm.quote(&QuoteParams {
        amount: 0,
        input_mint: a,
        output_mint: b,
        swap_mode: SwapMode::ExactIn,
    });
    assert!(r.is_err());
}

#[test]
fn quote_dust_input_errors_zero_fee() {
    let (amm, [a, b]) = fixture();
    // Pool has swap_fee_rate=3000, so an input of 1 produces fee=0 → revert.
    let r = amm.quote(&QuoteParams {
        amount: 1,
        input_mint: a,
        output_mint: b,
        swap_mode: SwapMode::ExactIn,
    });
    assert!(r.is_err());
}

#[test]
fn disabled_pool_is_inactive_and_unquotable() {
    let config = Pubkey::new_unique();
    let m_a = Pubkey::new_unique();
    let m_b = Pubkey::new_unique();
    let tp = spl_token::id();
    let toks = [
        TokenInit {
            mint: m_a,
            token_program: tp,
            normalized_weight: 5_000,
            virtual_balance: 1_000_000_000,
            actual_balance: 1_000_000_000,
            protocol_fees_owed: 0,
        },
        TokenInit {
            mint: m_b,
            token_program: tp,
            normalized_weight: 5_000,
            virtual_balance: 1_000_000_000,
            actual_balance: 1_000_000_000,
            protocol_fees_owed: 0,
        },
    ];
    let data = build_pool_data(config, 7, 3_000, 0, /*pool_enabled*/ false, true, &toks);
    let pool_key = CubeAmm::derive_pool_pda(&config, 7);
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
    let amm = CubeAmm::from_keyed_account(
        &keyed,
        &AmmContext {
            clock_ref: ClockRef::default(),
        },
    )
    .unwrap();
    assert!(!amm.is_active());
    let r = amm.quote(&QuoteParams {
        amount: 1_000_000,
        input_mint: m_a,
        output_mint: m_b,
        swap_mode: SwapMode::ExactIn,
    });
    assert!(r.is_err());
}

#[test]
fn update_reads_fresh_balances() {
    let (mut amm, [a, b]) = fixture();
    let q_before = amm
        .quote(&QuoteParams {
            amount: 10_000_000,
            input_mint: a,
            output_mint: b,
            swap_mode: SwapMode::ExactIn,
        })
        .unwrap();

    // Build a new pool buffer where vbB and abB are 4x larger.
    let new_tokens = [
        TokenInit {
            mint: a,
            token_program: spl_token::id(),
            normalized_weight: 5_000,
            virtual_balance: 1_000_000_000,
            actual_balance: 1_000_000_000,
            protocol_fees_owed: 0,
        },
        TokenInit {
            mint: b,
            token_program: spl_token::id(),
            normalized_weight: 5_000,
            virtual_balance: 8_000_000_000,
            actual_balance: 8_000_000_000,
            protocol_fees_owed: 0,
        },
    ];
    let new_data = build_pool_data(
        Pubkey::new_unique(), // config doesn't matter for re-decode here
        1,
        3_000,
        2_000,
        true,
        true,
        &new_tokens,
    );

    let mut map: AccountMap = AccountMap::default();
    map.insert(
        amm.key(),
        Account {
            lamports: 0,
            data: new_data,
            owner: CUBIC_POOL_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    );
    amm.update(&map).unwrap();

    let q_after = amm
        .quote(&QuoteParams {
            amount: 10_000_000,
            input_mint: a,
            output_mint: b,
            swap_mode: SwapMode::ExactIn,
        })
        .unwrap();
    assert!(
        q_after.out_amount > q_before.out_amount,
        "after vbO=8e9 should yield more output than before vbO=2e9"
    );
}

#[test]
fn swap_and_account_metas_layout() {
    let (amm, [a, b]) = fixture();
    let user = Pubkey::new_unique();
    let src_ata = Pubkey::new_unique();
    let dst_ata = Pubkey::new_unique();
    let sap = amm
        .get_swap_and_account_metas(&SwapParams {
            in_amount: 1_000_000,
            source_mint: a,
            destination_mint: b,
            source_token_account: src_ata,
            destination_token_account: dst_ata,
            token_transfer_authority: user,
        })
        .unwrap();
    assert!(matches!(sap.swap, Swap::Placeholder));
    assert_eq!(sap.account_metas.len(), 11);

    // Index 0: program id, read-only.
    assert_eq!(sap.account_metas[0].pubkey, CUBIC_POOL_PROGRAM_ID);
    assert!(!sap.account_metas[0].is_writable);
    assert!(!sap.account_metas[0].is_signer);

    // Index 1: pool, writable, not signer.
    assert_eq!(sap.account_metas[1].pubkey, amm.key());
    assert!(sap.account_metas[1].is_writable);

    // Index 2: token_mint_in (read).
    assert_eq!(sap.account_metas[2].pubkey, a);
    assert!(!sap.account_metas[2].is_writable);

    // Index 3: token_mint_out (read).
    assert_eq!(sap.account_metas[3].pubkey, b);

    // Index 4: user_token_in (writable).
    assert_eq!(sap.account_metas[4].pubkey, src_ata);
    assert!(sap.account_metas[4].is_writable);

    // Index 5: user_token_out (writable).
    assert_eq!(sap.account_metas[5].pubkey, dst_ata);

    // Index 6/7: vaults = ATA(pool, mint, token_program).
    let vault_in = CubeAmm::derive_vault(&amm.key(), &a, &spl_token::id());
    let vault_out = CubeAmm::derive_vault(&amm.key(), &b, &spl_token::id());
    assert_eq!(sap.account_metas[6].pubkey, vault_in);
    assert!(sap.account_metas[6].is_writable);
    assert_eq!(sap.account_metas[7].pubkey, vault_out);
    assert!(sap.account_metas[7].is_writable);

    // Index 8: user (signer, writable).
    assert_eq!(sap.account_metas[8].pubkey, user);
    assert!(sap.account_metas[8].is_signer);
    assert!(sap.account_metas[8].is_writable);

    // Index 9/10: token programs (read).
    assert_eq!(sap.account_metas[9].pubkey, spl_token::id());
    assert_eq!(sap.account_metas[10].pubkey, spl_token::id());
}

#[test]
fn build_swap_instruction_produces_correct_data() {
    let (amm, _) = fixture();
    let user = Pubkey::new_unique();
    let ix = amm.build_swap_instruction(user, 1_000_000, 0, 0, 1).unwrap();
    assert_eq!(ix.program_id, CUBIC_POOL_PROGRAM_ID);
    assert_eq!(ix.data.len(), 26);
    assert_eq!(&ix.data[..8], &SWAP_IX_DISCRIMINATOR);
    // Ix doesn't carry the leading program-id meta — that's only in the
    // `Amm`-trait account_metas. The on-chain Instruction has 10 accounts.
    assert_eq!(ix.accounts.len(), 10);
}
