// 2026-09 reward rules: static treasury×1%/day; level treasury×configurable bps/day, eight normalized
// headcount tiers with a per-person cap; promotion retains the original static-claim base.
// All three claims spend treasury, consume gross-value power and deliver net LIKE.
// All rewards share one claim with a fixed 4h cooldown.
//! donate — the co-build program.
//!
//! Users donate **SOL** or **Alpha** (i.e. $LIKE, a classic SPL token);
//! the program records the donation's USDT value and multiplies it by 3 into the user's *power*. Power plays two
//! roles: (a) as a weight in the continuous release/emission of **Beta** (the project reward token);
//! (b) as a *release budget* that every claim deducts from. See `docs/donate-design.md`.
//!
//! Both tokens use classic SPL. A registered referrer is required before donation.
//! Static rewards use a per-power accumulator and a time-value claim cap; promotion
//! and level rewards use separate pending ledgers and a shared 4h claim clock.
//! All claims consume gross-value power, charge the configured fee, and swap net
//! FLOCK from treasury into LIKE. Unpaid rewards carry while power remains.
//! Price observations and pool guards are validated before value-sensitive actions.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::invoke_signed,
    pubkey,
};
use anchor_lang::system_program::{transfer as sol_transfer, Transfer as SolTransfer};
use anchor_lang::AccountsExit;
use anchor_lang::Discriminator;
use anchor_spl::token_interface::{
    burn, close_account, sync_native, transfer_checked, Burn, CloseAccount, Mint, SyncNative,
    TokenAccount, TokenInterface, TransferChecked,
};
mod venue;
use venue::VenuePool;
mod treasury_burn;
pub use treasury_burn::*;
mod raydium_migration;
pub use raydium_migration::*;
mod allowance;
pub use allowance::*;
mod oracle;
pub use oracle::*;
mod migration;
pub use migration::*;
mod escrow_guard;
mod max_leg;
pub use max_leg::*;
use spl_token_2022::onchain::invoke_transfer_checked;

declare_id!("9NhbVKzzSKwkP7kEgNzSpacYA9UfCi2b58E1mrn2QsKT");

/// Denominator for basis points (bps) (100% = 10_000 bps).
pub const BPS_DENOM: u16 = 10_000;
/// The legacy level-reward rate (0.6%) used by existing accounts before governance sets a rate.
pub const DEFAULT_LEVEL_REWARD_BPS: u16 = 60;
/// Fixed-point scaling factor for the "accumulated reward per unit of power" accumulator.
pub const ACC_SCALE: u128 = 1_000_000_000_000; // 1e12
/// Seconds in the release/emission window — the daily release rate is expressed over exactly this window.
#[cfg(not(feature = "mainnet-test-10m"))]
pub const SECONDS_PER_DAY: i64 = 86_400;
#[cfg(feature = "mainnet-test-10m")]
pub const SECONDS_PER_DAY: i64 = 600;
/// Open-price fixed-point scaling (matching dex-pool): open_price = base*1e12/quote (Alpha per Beta).
pub const PRICE_SCALE: u128 = 1_000_000_000_000;
/// Smallest units per whole token for SOL / Alpha / Beta (all three have 9 decimals here).
pub const TOKEN_UNIT: u128 = 1_000_000_000; // 1e9
/// PR-02: dex_prices decimals assumption — base(Alpha)/quote(Beta) must be 9dp (both sides cancel).
/// initialize / set_alpha assert on this; a mismatch reverts.
pub const EXPECTED_TOKEN_DECIMALS: u8 = 9;
/// Number of (VIP) level tiers: V1..V8 (index i maps to V(i+1); 0 means below V1).
/// V7/V8 are surfaced to users as "V6 one-star / V6 two-star" — on-chain they are ordinary levels 7 and 8.
pub const LEVEL_TIERS: usize = 8;
pub const V_TIERS: usize = 8;
/// The lowest level enrolled in that pool; `v_idx` maps a level onto the per-tier arrays.
pub const V_TIER_MIN_LEVEL: u8 = 1;
/// A direct child counts toward its parent's `direct_ge6` star counter once its cached level
/// reaches this. `set_levels` forbids a star requirement below it, which is what guarantees a
/// star change can only ever move a user *within* {V6, V7, V8} and never across this boundary.
pub const STAR_COUNT_LEVEL: u8 = 6;
/// `UserStake.flags` bit 0: this user's own power has been folded into its uplines' `leg_power`
/// (and its level into the direct upline's `direct_ge6`) by `seed_leg`.
pub const FLAG_LEG_SEEDED: u8 = 1 << 0;
/// Max number of upline levels traversed when propagating a power change upward (within 10 levels below).
pub const MAX_UPLINE: usize = 10;
/// How far a `leg_power` change propagates: one hop less than `MAX_UPLINE`. That off-by-one is
/// exactly what makes `Σ(child.leg_power) ≡ parent.team_power` hold — team_power spans 10
/// generations below a node, so each child's leg must span 9 below itself.
pub const LEG_UPLINE: usize = MAX_UPLINE - 1;
pub const INCINERATOR: Pubkey = pubkey!("1nc1nerator11111111111111111111111111111111");
pub const LP_LOCKER_ID: Pubkey = pubkey!("9c18DLHnXrQC6MAVHpig9kpAE8YX7mZYhB6LtVT84RW1");
/// Successful claims share a cooldown independent of the daily reward window.
#[cfg(not(feature = "mainnet-test-10m"))]
pub const CLAIM_COOLDOWN: i64 = 14_400;
#[cfg(feature = "mainnet-test-10m")]
pub const CLAIM_COOLDOWN: i64 = 600;
/// Per-user cap on **currently held** power: 300,000. Power is denominated in USDT-e6 scaled by
/// `power_multiplier_bps`, so 300_000 * 1e6 = 3e11 base units (= 100,000 USDT donated at the 3x multiplier).
/// Enforced against `power` (the remaining release budget), which shrinks as `claim` releases it: a user
/// who has released power back down is free to donate again, up to this ceiling on the *outstanding* power.
/// `power_granted` remains a monotonic lifetime total, but it no longer gates donations.
pub const USER_POWER_CAP: u128 = 300_000_000_000;
/// `AllocateSolBuy` declares 20 fixed accounts.
/// The dApp appends the donating wallet + System Program at these positions so the
/// temporary WSOL ATA can refund its rent without changing the Anchor account ABI.
pub const RENT_REFUND_USER_ACCOUNT_INDEX: usize = 20;
pub const RENT_REFUND_SYSTEM_ACCOUNT_INDEX: usize = 21;

// ---- Raydium AMM v4 (LIKE/WSOL pool) — the external DEX for the SOL donation's SOL→Alpha buy leg. ----
// This pool is a Raydium Liquidity Pool V4 + OpenBook (not CPMM). The swap_base_in account order was verified against a real on-chain swap
// transaction as **17 accounts (excluding amm_target_orders)**; instruction data = [9u8] ++ amount_in(LE u64) ++ min_out(LE u64).
pub const WSOL_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
pub const RAYDIUM_AMM_PROGRAM: Pubkey = pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");
pub const RAYDIUM_POOL: Pubkey = pubkey!("FuemMjepntbzthvSEVmDGnfq7YWr8UebZrAXJrP46VtF");
pub const RAYDIUM_AUTHORITY: Pubkey = pubkey!("5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1");
pub const RAYDIUM_OPEN_ORDERS: Pubkey = pubkey!("5CQzLpqEKUhXGVXs3KzWNL7GxdusTtucZZzsNVMpq4UP");
pub const RAYDIUM_COIN_VAULT: Pubkey = pubkey!("GTfC2Xxfxzqpvx8sJg24VauWgskCsT6SuxgrPx3oTLcR"); // LIKE(coin/base)
pub const RAYDIUM_PC_VAULT: Pubkey = pubkey!("BnL2wav9yAL8XYi9FRRcB46UFoQNZLGUC6GRtZtSvRrQ"); // WSOL(pc/quote)
pub const OPENBOOK_PROGRAM: Pubkey = pubkey!("srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX");
pub const SERUM_MARKET: Pubkey = pubkey!("AmNde9EdnEUDVjpyPyMJ7i4TW1S56HBeehqUo9Lcj8mN");
pub const SERUM_BIDS: Pubkey = pubkey!("3n3UEkLZd6cWbHN22JLkD7352Wn71tw33jHZAs1zJeZ9");
pub const SERUM_ASKS: Pubkey = pubkey!("J4c8de2QftrgmUERFiUXqdqRstn8PhGn26fWwQuvkcX6");
pub const SERUM_EVENT_QUEUE: Pubkey = pubkey!("F3cwGcQjjuHVzcynTpPG1oYETRdAawd2UqHJAHFGEu2V");
pub const SERUM_COIN_VAULT: Pubkey = pubkey!("BtFehJQeDc4X1ur5y4i8S8ZDhinyi83PVsT5uih9ueAj");
pub const SERUM_PC_VAULT: Pubkey = pubkey!("5xd5dibdqBrLmuVC4uA8kqXTzqBKQL7YoaaSWaU9a7tG");
pub const SERUM_VAULT_SIGNER: Pubkey = pubkey!("6J31tcboLYxT51NaGFdBXcc6gkLCFbpcbBzPHMdF1oYv");
// The two vaults of the SOL/USDC deep pool (Raydium AMM v4) — `record_price_observation` reads them to derive SOL/USD (USDT-e6).
// WSOL has 9 decimals, USDC has 6. Only balances are read; validated against address constants to prevent substitution.
pub const SOLUSDC_WSOL_VAULT: Pubkey = pubkey!("DQyrAcCrDXQ7NeoqGgDCZwBvWDcYmFCjSb9JtteuvPpz"); // WSOL(9dp)
pub const SOLUSDC_USDC_VAULT: Pubkey = pubkey!("HLmqeL62xR1QoZ1HKKbXRrdN1p3phKpxRMb2VVopvBBz"); // USDC(6dp)
/// Slippage tolerance for the Raydium swap: with a deep pool + a small 3% amount, 300bps (3%) is enough to cover the 0.25% fee and price impact.
pub const RAYDIUM_SLIPPAGE_BPS: u16 = 300;

#[program]
pub mod donate {
    use super::*;


    pub fn crank_treasury_burn(ctx: Context<CrankTreasuryBurn>) -> Result<()> { treasury_burn::crank(ctx) }

    pub fn reschedule_raydium_migration(ctx:Context<RescheduleRaydiumMigration>,resume_ts:i64)->Result<()> {
        raydium_migration::reschedule(ctx,resume_ts)
    }

    pub fn begin_raydium_migration(ctx:Context<BeginRaydiumMigration>,users:u32,ordered_hash:[u8;32],resume_ts:i64)->Result<()> {raydium_migration::begin(ctx,users,ordered_hash,resume_ts)}
    pub fn checkpoint_raydium_user(ctx:Context<CheckpointRaydiumUser>,owner:Pubkey)->Result<()> {raydium_migration::checkpoint(ctx,owner)}
    pub fn bind_existing_clmm_pool(ctx:Context<BindExistingClmmPool>)->Result<()> {raydium_migration::bind_existing_clmm(ctx)}
    pub fn bind_prepared_clmm_pool(ctx:Context<BindPreparedClmmPool>)->Result<()> {raydium_migration::bind_prepared_clmm(ctx)}
    pub fn finalize_raydium_migration(ctx:Context<FinalizeRaydiumMigration>)->Result<()> {raydium_migration::finalize(ctx)}
    pub fn resume_raydium_now(ctx:Context<ResumeRaydiumNow>)->Result<()> {raydium_migration::resume_now(ctx)}

    /// Activate a sealed import after oracle warmup and verification of the fixed,
    /// immutable LP locker. Starts the reward and imported-user waiting baselines.
    pub fn activate(ctx: Context<Activate>) -> Result<()> {
        migration::activate(ctx)
    }

    pub fn admin_seed_user(
        ctx: Context<AdminSeedUser>,
        owner: Pubkey,
        referrer: Pubkey,
        power: u128,
        power_granted: u128,
    ) -> Result<()> {
        migration::seed(ctx, owner, referrer, power, power_granted)
    }

    pub fn begin_rebuild(ctx: Context<AdminConfig>) -> Result<()> {
        migration::begin_rebuild(ctx)
    }

    /// Irreversibly stop a migration before activation and prepare its
    /// UserStake accounts for bounded rent-reclaim batches.
    pub fn begin_migration_abort(ctx: Context<BeginMigrationAbort>) -> Result<()> {
        migration::begin_abort(ctx)
    }

    pub fn close_migration_users<'info>(
        ctx: Context<'_, '_, 'info, 'info, CloseMigrationUsers<'info>>,
    ) -> Result<()> {
        migration::close_abort_users(ctx)
    }

    pub fn finalize_migration_abort(ctx: Context<FinalizeMigrationAbort>) -> Result<()> {
        migration::finalize_abort(ctx)
    }

    pub fn finalize_fresh(ctx: Context<FinalizeFresh>) -> Result<()> {
        migration::finalize_fresh(ctx)
    }

    pub fn finalize_seeding(ctx: Context<AdminConfig>) -> Result<()> {
        migration::finalize(ctx)
    }

    pub fn initialize_price_observation(ctx: Context<InitializeObservation>) -> Result<()> {
        ctx.accounts.observation.config = ctx.accounts.config.key();
        Ok(())
    }

    pub fn rebuild_user(ctx: Context<RebuildUser>) -> Result<()> {
        migration::rebuild(ctx)
    }

    pub fn record_price_observation(ctx: Context<RecordPriceObservation>) -> Result<()> {
        require_keys_eq!(
            ctx.accounts.keeper.key(),
            ctx.accounts.config.keeper,
            DonateError::Unauthorized
        );
        let clock = Clock::get()?;
        let sol = if ctx.accounts.solusdc_wsol_vault.amount == 0 {
            0
        } else {
            ctx.accounts.solusdc_usdc_vault.amount as u128 * TOKEN_UNIT
                / ctx.accounts.solusdc_wsol_vault.amount as u128
        };
        let like = if ctx.accounts.amm_coin_vault.amount == 0 {
            0
        } else {
            ctx.accounts.amm_pc_vault.amount as u128 * PRICE_SCALE
                / ctx.accounts.amm_coin_vault.amount as u128
        };
        let dex = if ctx.accounts.quote_vault.amount == 0 {
            0
        } else {
            ctx.accounts.pool.reserves(ctx.accounts.base_vault.amount,ctx.accounts.quote_vault.amount)?.0 as u128 * PRICE_SCALE
                / ctx.accounts.pool.reserves(ctx.accounts.base_vault.amount,ctx.accounts.quote_vault.amount)?.1.max(1) as u128
        };
        if ctx.accounts.pool.clmm.is_some() { ctx.accounts.pool.fee(ctx.remaining_accounts)?; }
        if ctx.accounts.pool.cpmm.is_some() || ctx.accounts.pool.clmm.is_some() {raydium_migration::day_price(ctx.accounts.config.key(),ctx.accounts.pool.key(),ctx.remaining_accounts,clock.unix_timestamp,Some(dex))?;}
        let dex_integral = if ctx.accounts.pool.clmm.is_some() {
            ctx.accounts.observation.clmm_integral(clock.unix_timestamp)?
        } else {
            ctx.accounts.pool.integral(ctx.accounts.pool.key(),ctx.accounts.base_vault.amount,ctx.accounts.quote_vault.amount,ctx.remaining_accounts,clock.unix_timestamp)?
        };
        let avg = ctx.accounts.observation.record_for_venue(
            ctx.accounts.config.max_sync_deviation_bps,
            clock.unix_timestamp,
            clock.slot,
            sol,
            like,
            dex,
            dex_integral,
            ctx.accounts.pool.cpmm.is_some(),
        )?;
        let c = &mut ctx.accounts.config;
        c.price_ready = false;
        c.price_breaker = ctx.accounts.observation.breaker;
        if let Some(avg) = avg {
            c.sol_usd = u64::try_from(avg[0]).map_err(|_| error!(DonateError::MathOverflow))?;
            c.alpha_usd = u64::try_from(
                avg[0]
                    .checked_mul(avg[1])
                    .ok_or(DonateError::MathOverflow)?
                    / PRICE_SCALE,
            )
            .map_err(|_| error!(DonateError::MathOverflow))?;
            c.beta_usd = u64::try_from(
                (c.alpha_usd as u128)
                    .checked_mul(avg[2])
                    .ok_or(DonateError::MathOverflow)?
                    / PRICE_SCALE,
            )
            .map_err(|_| error!(DonateError::MathOverflow))?;
            c.beta_usd_anchor = c.beta_usd;
            c.like_sol_anchor = avg[1];
            c.pool_ratio_anchor = avg[2];
            c.last_sync_ts = clock.unix_timestamp;
            c.price_ready = c.sol_usd > 0 && c.alpha_usd > 0 && c.beta_usd > 0;
        }
        Ok(())
    }

    pub fn set_import_manifest(
        ctx: Context<AdminConfig>,
        slot: u64,
        digest: [u8; 32],
        ordered_hash: [u8; 32],
        count: u32,
        power: u128,
    ) -> Result<()> {
        migration::commitment(ctx, slot, digest, ordered_hash, count, power)
    }

    pub fn set_payout_wallets(
        ctx: Context<AdminConfig>,
        operations_wallet: Pubkey,
        lp_recipient: Pubkey,
    ) -> Result<()> {
        check_lp_recipient(&lp_recipient)?;
        let cfg = &mut ctx.accounts.config;
        cfg.operations_wallet = operations_wallet;
        cfg.lp_recipient = lp_recipient;
        Ok(())
    }

    pub fn initialize(ctx: Context<Initialize>, params: InitializeParams, startup_mode: StartupMode) -> Result<()> {
        params.validate()?;

        // PR-02: the three-price derivation (dex_prices) hardcodes the assumption that Alpha/Beta = 9dp (both 9dp sides cancel).
        // Assert here that base(Alpha)/quote(Beta) decimals equal the assumed value; a mismatch reverts, preventing a future mint swap
        // from systematically miscomputing prices.
        require!(
            ctx.accounts.alpha_mint.decimals == EXPECTED_TOKEN_DECIMALS
                && ctx.accounts.beta_mint.decimals == EXPECTED_TOKEN_DECIMALS,
            DonateError::BadDecimals
        );

        let cfg = &mut ctx.accounts.config;
        cfg.authority = ctx.accounts.authority.key();
        cfg.keeper = params.keeper;
        cfg.root_referrer = ctx.accounts.root_referrer.key();
        // config_seed stays the PDA seed forever; beta_mint is the live FLOCK (they diverge only after migrate_flock).
        cfg.config_seed = ctx.accounts.beta_mint.key();
        cfg.beta_mint = ctx.accounts.beta_mint.key();
        cfg.alpha_mint = ctx.accounts.alpha_mint.key();
        cfg.pool = ctx.accounts.pool.key();
        cfg.base_vault = ctx.accounts.base_vault.key();
        cfg.quote_vault = ctx.accounts.quote_vault.key();
        cfg.treasury = ctx.accounts.treasury.key();
        cfg.alpha_escrow = ctx.accounts.alpha_escrow.key();
        cfg.sol_escrow = ctx.accounts.sol_escrow.key();
        cfg.operations_wallet = ctx.accounts.operations_wallet.key();
        check_lp_recipient(&ctx.accounts.lp_recipient.key())?;
        cfg.lp_recipient = ctx.accounts.lp_recipient.key();
        cfg.bps_operations = params.bps_operations;

        cfg.bps_liquidity = params.bps_liquidity;
        cfg.bps_treasury = params.bps_treasury;

        cfg.power_multiplier_bps = params.power_multiplier_bps;
        cfg.donate_cap_usdt = params.donate_cap_usdt;
        cfg.daily_emission_bps = params.daily_emission_bps;
        cfg.user_daily_cap_bps = params.user_daily_cap_bps;
        cfg.day_window_secs = params.day_window_secs;
        cfg.blackhole_floor = params.blackhole_floor;
        cfg.max_slippage_bps = params.max_slippage_bps;
        cfg.sol_usd = 0;
        cfg.alpha_usd = 0;
        cfg.beta_usd = 0;
        // Price deviation protection defaults to 10%; the optional claim valuation floor is separate.
        cfg.beta_usd_anchor = 0;
        cfg.last_sync_ts = 0;
        // Preserve the legacy i64 account slot and its byte layout. Old accounts have 0 here;
        // level_reward_bps() interprets that as the historical 60 bps rate.
        cfg.min_sync_interval_secs = DEFAULT_LEVEL_REWARD_BPS as i64;
        cfg.max_sync_deviation_bps = oracle::DEFAULT_DEVIATION_BPS;
        cfg.claim_floor_tol_bps = 0;
        cfg.pool_ratio_anchor = 0;
        cfg.acc_reward_per_power = 0;
        cfg.total_power = 0;
        cfg.total_burned = 0;
        cfg.last_update_ts = Clock::get()?.unix_timestamp;
        // Level thresholds default to unreachable (u128::MAX) → everyone is V0 until the authority writes real
        // thresholds via `set_levels`. Avoids misclassifying everyone as top-level while thresholds are "undefined".
        cfg.level_min_power = [u64::MAX; LEVEL_TIERS];
        cfg.level_min_team = [u64::MAX; LEVEL_TIERS];
        // Small-area and star gates default to "no requirement": they only bite once the authority
        // writes them, and unlike the three above, a zero here is the *permissive* value — which
        // is safe only because the three unreachable thresholds above already gate every level.
        cfg.level_min_small = [0; LEVEL_TIERS];
        cfg.level_min_star = [0; LEVEL_TIERS];

        cfg.level_share_cap_bps = 2000; // maximum 20% per person within a tier
        cfg.level_weights = [10, 9, 8, 7, 6, 5, 3, 2];

        cfg.leg_seeded_count = 0;
        // Migration seeding starts unlocked; `finalize_seeding` latches it closed after the referrer/power
        // snapshot has been replayed via `admin_seed_user`.
        cfg.startup_mode = startup_mode;
        cfg.seeding_locked = startup_mode == StartupMode::Fresh;
        cfg.seeded_user_count = 0;
        cfg.v_last_update_ts = Clock::get()?.unix_timestamp;

        cfg.claim_first_delay_secs = SECONDS_PER_DAY;
        cfg.claim_fee_base_bps = 2_000;
        cfg.claim_fee_drop_bps = [500, 1000, 2000, 3000];
        cfg.claim_fee_tier_bps = [2500, 3000, 3500, 4000];
        cfg.claim_fee_operations_bps = 1500;
        cfg.claim_fee_blackhole_bps = 3500;

        cfg.v_dividend_enabled = true; // Level accrual starts enabled.
                                       // The promotion reward (by generation) is off by default: rates/thresholds all 0, valid threshold = u128::MAX (no one qualifies), until the authority
                                       // writes the real tables via `set_promotion` (same principle as level thresholds being "unreachable while unconfigured").
        cfg.promo_rate_bps = [0; MAX_UPLINE];
        cfg.promo_min_direct = [0; MAX_UPLINE];
        cfg.promo_min_power = [0; MAX_UPLINE];
        cfg.valid_invite_min_power = u128::MAX;
        cfg.sol_escrow_bump = ctx.bumps.sol_escrow;
        cfg.bump = ctx.bumps.config;

        // Root referral node (configurable root_referrer, defaults to the deployer).
        // referrer = itself (the only self-loop in the whole tree).
        let root = &mut ctx.accounts.root_user;
        root.owner = ctx.accounts.root_referrer.key();
        root.referrer = ctx.accounts.root_referrer.key();
        root.bump = ctx.bumps.root_user;
        // Born seeded: an account created here starts with zero power, so its contribution to every
        // upline's `leg_power` is zero and already correct. `seed_leg` exists only for accounts that
        // predate the small-area bookkeeping; leaving a fresh one unseeded would make `sync_level` skip its
        // leg propagation forever, pinning every ancestor's `max_leg` at 0 and over-stating the small area.
        root.flags = FLAG_LEG_SEEDED;
        Ok(())
    }

    /// authority-only: adjust the fund-split ratios + economic params (the bps values must sum to 10000).
    pub fn set_params(ctx: Context<EmissionAdmin>, params: InitializeParams) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        accrue_global(
            &mut ctx.accounts.config,
            ctx.accounts.treasury.amount,
            now,
        );
        accrue_v(
            &mut ctx.accounts.config,
            ctx.accounts.treasury.amount,
            now,
        );
        params.validate()?;
        let cfg = &mut ctx.accounts.config;
        cfg.keeper = params.keeper;
        cfg.bps_operations = params.bps_operations;

        cfg.bps_liquidity = params.bps_liquidity;
        cfg.bps_treasury = params.bps_treasury;

        cfg.power_multiplier_bps = params.power_multiplier_bps;
        cfg.donate_cap_usdt = params.donate_cap_usdt;
        cfg.daily_emission_bps = params.daily_emission_bps;
        cfg.user_daily_cap_bps = params.user_daily_cap_bps;
        // Remainders represent fractions of one base unit; changing the time denominator
        // must not turn a previous fractional reward into whole tokens.
        cfg.emission_remainder = rescale_time_remainder(
            cfg.emission_remainder,
            cfg.day_window_secs,
            params.day_window_secs,
        )?;
        cfg.day_window_secs = params.day_window_secs;
        cfg.blackhole_floor = params.blackhole_floor;
        cfg.max_slippage_bps = params.max_slippage_bps;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_levels(
        ctx: Context<LevelRewardAdmin>,
        min_power: [u64; LEVEL_TIERS],
        min_team: [u64; LEVEL_TIERS],
        min_small: [u64; LEVEL_TIERS],
        min_star: [u8; LEVEL_TIERS],
    ) -> Result<()> {
        require!(
            matches!(
                ctx.accounts.config.migration_phase,
                migration::IMPORTING | migration::ACTIVE
            ),
            DonateError::MigrationPhase
        );
        validate_levels(&min_power, &min_team, &min_small, &min_star)?;
        accrue_v(
            &mut ctx.accounts.config,
            ctx.accounts.treasury.amount,
            Clock::get()?.unix_timestamp,
        );
        let cfg = &mut ctx.accounts.config;
        cfg.level_min_power = min_power;
        cfg.level_min_team = min_team;
        cfg.level_min_small = min_small;
        cfg.level_min_star = min_star;
        Ok(())
    }

    /// authority-only: set the three per-generation tables for the "promotion reward" (generational promotion reward), plus
    /// the power threshold for a "valid direct referral". Index `g-1` maps to the `g`-th generation upline (the nearest upline is generation 1):
    ///   - `rate_bps[g-1]`: that generation's rate (bps; the accounting base = the Beta the user takes in this `claim`).
    ///   - `min_direct[g-1]`: the "valid direct referral count" required for that generation (invitee power ≥ `valid_invite_min_power`).
    ///   - `min_power[g-1]`: the "upline's own power" required for that generation (same units as `UserStake.power`).
    /// The three tables + threshold all live on-chain, are adjustable at any time, and need no redeploy. Each rate must not exceed 100% (BPS_DENOM).
    /// This instruction only writes config, it does not pay out; the promotion reward is accounted inside `claim` and claimed by the upline via `claim` at the cost of power.
    pub fn set_promotion(
        ctx: Context<AdminConfig>,
        rate_bps: [u16; MAX_UPLINE],
        min_direct: [u16; MAX_UPLINE],
        min_power: [u128; MAX_UPLINE],
        valid_invite_min_power: u128,
    ) -> Result<()> {
        require!(
            matches!(
                ctx.accounts.config.migration_phase,
                migration::IMPORTING | migration::ACTIVE
            ),
            DonateError::MigrationPhase
        );
        for i in 0..MAX_UPLINE {
            require!(rate_bps[i] <= BPS_DENOM, DonateError::BadRatios);
        }

        let cfg = &mut ctx.accounts.config;
        cfg.promo_rate_bps = rate_bps;
        cfg.promo_min_direct = min_direct;
        cfg.promo_min_power = min_power;
        cfg.valid_invite_min_power = valid_invite_min_power;
        Ok(())
    }

    /// authority-only: set the optional default cranker (scheduled trigger) identity, keeper.
    pub fn set_keeper(ctx: Context<AdminConfig>, keeper: Pubkey) -> Result<()> {
        ctx.accounts.config.keeper = keeper;
        Ok(())
    }

    /// authority-only: re-point to a new Alpha mint and its paired pool / vault /
    /// escrow. Note: under the unified design, Alpha is the classic SPL mint pinned by the swap DEX ($LIKE),
    /// and the pool is shared, so this instruction is effectively deprecated — kept only for IDL/admin-interface continuity.
    /// Switching Alpha means a *different* `base=Alpha / quote=Beta` pool, so the new pool +
    /// vaults, plus a config-owned escrow holding the new Alpha, must all already exist (the caller must
    /// pre-create this escrow ATA). The old escrow's balance is abandoned — allocate it out
    /// before switching. Beta (quote) never changes.

    /// authority-only and **irreversible**: renounce authority. Afterward, all authority-gated
    /// instructions (`set_*`, `renounce`) revert permanently — because the stored authority is set to
    /// the default (unsignable) pubkey, and no one can ever pass the has_one check again.
    pub fn renounce_authority(ctx: Context<AdminConfig>) -> Result<()> {
        require!(
            !matches!(
                ctx.accounts.config.migration_phase,
                migration::ABORTING | migration::ABORTED
            ),
            DonateError::MigrationPhase
        );
        ctx.accounts.config.authority = Pubkey::default();
        Ok(())
    }

    /// authority-only: **transfer** admin to `new_authority`. Reuses the same has_one gate as renounce.
    /// Rejects the all-zero address so a transfer can never silently become an irreversible renounce
    /// (use renounce_authority explicitly for that).
    pub fn transfer_authority(ctx: Context<AdminConfig>, new_authority: Pubkey) -> Result<()> {
        require!(
            !matches!(
                ctx.accounts.config.migration_phase,
                migration::ABORTING | migration::ABORTED
            ),
            DonateError::MigrationPhase
        );
        require_keys_neq!(
            new_authority,
            Pubkey::default(),
            DonateError::InvalidAuthority
        );
        ctx.accounts.config.authority = new_authority;
        Ok(())
    }

    /// Update every oracle/transaction deviation check. Does not alter prices,
    /// observation history, freshness or breaker state.
    pub fn set_price_deviation(ctx: Context<AdminConfig>, max_deviation_bps: u16) -> Result<()> {
        require!((1..=BPS_DENOM).contains(&max_deviation_bps), DonateError::BadPriceDeviation);
        let cfg = &mut ctx.accounts.config;
        let old_bps = oracle::effective_deviation_bps(cfg.max_sync_deviation_bps);
        cfg.max_sync_deviation_bps = max_deviation_bps;
        emit!(PriceDeviationChanged {
            config: cfg.key(),
            authority: ctx.accounts.authority.key(),
            old_bps,
            new_bps: max_deviation_bps,
            timestamp: Clock::get()?.unix_timestamp,
        });
        Ok(())
    }

    /// authority-only: configure the withdrawal fee (base/anti-dump tiers/distribution) and the time gates.
    /// The "blackhole" share is no longer transferred to any account; instead it is `burn`ed directly against the treasury's Beta inside `claim` (truly destroyed),
    /// so there is no longer an admin-specifiable blackhole destination here (removing the "fees siphoned away" path).
    #[allow(clippy::too_many_arguments)]
    pub fn set_claim_fee(
        ctx: Context<AdminConfig>,
        base_bps: u16,
        drop_bps: [u16; 4],
        tier_bps: [u16; 4],
        operations_bps: u16,
        blackhole_bps: u16,
        first_delay_secs: i64,
    ) -> Result<()> {
        require!(base_bps <= BPS_DENOM, DonateError::BadRatios);
        for t in tier_bps.iter() {
            require!(*t <= BPS_DENOM, DonateError::BadRatios);
        }
        require!(
            (operations_bps as u32 + blackhole_bps as u32) <= BPS_DENOM as u32,
            DonateError::BadRatios
        );
        require!(
            first_delay_secs >= 0,
            DonateError::BadRatios
        );
        let cfg = &mut ctx.accounts.config;
        cfg.claim_fee_base_bps = base_bps;
        cfg.claim_fee_drop_bps = drop_bps;
        cfg.claim_fee_tier_bps = tier_bps;
        cfg.claim_fee_operations_bps = operations_bps;
        cfg.claim_fee_blackhole_bps = blackhole_bps;
        cfg.claim_first_delay_secs = first_delay_secs;
        Ok(())
    }

    /// Register the signer and bind an (immutable) referrer. The referrer must be a *registered* node —
    /// i.e. the `referrer_stake` PDA must already exist; Anchor enforces this by deserializing it
    /// (if the account doesn't exist, deserialization fails and the whole tx reverts). Self-referral is rejected. Since a node can only
    /// attach under a node registered earlier than itself, the whole graph is necessarily acyclic.
    pub fn bind_referrer(ctx: Context<BindReferrer>, referrer: Pubkey) -> Result<()> {
        migration::active(&ctx.accounts.config)?;
        require_keys_neq!(referrer, ctx.accounts.user.key(), DonateError::SelfReferral);

        let us = &mut ctx.accounts.user_stake;
        us.owner = ctx.accounts.user.key();
        us.referrer = referrer;
        us.bump = ctx.bumps.user_stake;
        // See `initialize`: a fresh account has zero power, so it is already correctly reflected in
        // its uplines' legs and must be marked seeded, or `sync_level` will never propagate its leg.
        us.flags = FLAG_LEG_SEEDED;

        ctx.accounts.referrer_stake.direct_count = ctx.accounts.referrer_stake.direct_count
            .checked_add(1).ok_or(DonateError::MathOverflow)?;
        us.referral_index = ctx.accounts.referrer_stake.direct_count;

        emit!(BindEvent {
            user: us.owner,
            referrer
        });
        Ok(())
    }

    /// Donate native SOL. Records its USDT value, mints power, and moves the SOL into escrow for later
    /// processing by `allocate`. Requires a bound referrer (the `user_stake` PDA must already have been
    /// created via `bind_referrer`).
    pub fn donate_sol<'info>(ctx: Context<'_, '_, '_, 'info, DonateSol<'info>>, amount: u64) -> Result<()> {
        migration::active(&ctx.accounts.config)?;
        if ctx.accounts.config.pool == venue::EXISTING_CLMM_POOL {
            let n=ctx.remaining_accounts.len();
            require!(n==3, DonateError::BadTreasuryBurnAccount);
            treasury_burn::business(&mut ctx.accounts.config, &mut ctx.accounts.treasury,
                &ctx.remaining_accounts[0], &ctx.accounts.token_program.to_account_info(), ctx.remaining_accounts)?;
        }
        require!(amount > 0, DonateError::ZeroAmount);
        require_keys_neq!(
            ctx.accounts.user_stake.referrer,
            Pubkey::default(),
            DonateError::ReferrerRequired
        );

        ctx.accounts.config.sol_budget = ctx
            .accounts
            .config
            .sol_budget
            .checked_add(amount)
            .ok_or(DonateError::MathOverflow)?;
        // **Wrap the donated SOL directly into WSOL** and deposit it in config's WSOL ATA (standard wrap: system transfer to
        // the WSOL account, then sync_native to make the amount take effect). The buy leg `allocate_sol_buy` then spends 97% here to buy Alpha,
        // then closes out the remaining 3%, unwrapping it into native SOL sent to the operations wallet — all via CPI only, avoiding direct lamport edits on a PDA
        // that trigger UnbalancedInstruction. (The prior approach of routing through sol_escrow and having the buy leg edit lamports directly was judged by the runtime as an account
        // balance non-conservation, so we wrap here instead.)
        sol_transfer(
            CpiContext::new(
                ctx.accounts.system_program.to_account_info(),
                SolTransfer {
                    from: ctx.accounts.user.to_account_info(),
                    to: ctx.accounts.config_wsol_ata.to_account_info(),
                },
            ),
            amount,
        )?;
        sync_native(CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            SyncNative {
                account: ctx.accounts.config_wsol_ata.to_account_info(),
            },
        ))?;

        oracle::fresh(&ctx.accounts.config, Clock::get()?.unix_timestamp)?;
        let spot = dex_sol_usd(
            ctx.accounts.solusdc_usdc_vault.amount as u128,
            ctx.accounts.solusdc_wsol_vault.amount as u128,
        )?;
        require!(
            oracle::within(spot as u128, ctx.accounts.config.sol_usd as u128, ctx.accounts.config.max_sync_deviation_bps),
            DonateError::PoolPriceDeviation
        );
        // SOL has 9 decimals (unit: lamports).
        let value = usdt_value(amount, ctx.accounts.config.sol_usd, 9)?;
        let resume=raydium_migration::business_allowance_resume(ctx.accounts.config.key(), &ctx.accounts.config, ctx.remaining_accounts, true)?;
        ctx.accounts.claim_allowance.checkpoint_with_resume(
            ctx.accounts.config.key(), &ctx.accounts.config, &ctx.accounts.user_stake,
            Clock::get()?.unix_timestamp, ctx.bumps.claim_allowance, resume,
        )?;
        let forfeited_beta = apply_donation(
            &mut ctx.accounts.config,
            &mut ctx.accounts.user_stake,
            ctx.accounts.treasury.amount,
            value,
        )?;

        // ===== Enforced check: this tx must atomically bundle the SOL distribution legs covering the full amount =====
        // The three-leg distribution of a SOL donation (operations paid in SOL, remainder bought into LIKE and allocated to treasury/liquidity) cannot be folded into
        // this instruction (the two CPI account sets, Raydium + the self-built dex, exceed the limit), so it is split into `allocate_sol_buy` +
        // `allocate_sol_distribute` within the same v0 tx. Here we introspect the Instructions sysvar, asserting both legs exist, act on the same config,
        // and the buy leg's amount covers this donation — otherwise the whole donation reverts. The donation was just wrapped into config_wsol_ata, and `allocate_sol_buy`
        // additionally verifies `config_wsol_ata holds ≥ amount WSOL`, together guaranteeing "this donation is distributed atomically and only this donation is spent".
        require_sol_donation_bundled(
            &ctx.accounts.instructions.to_account_info(),
            &ctx.accounts.config.key(),
            &ctx.accounts.user.key(),
            amount,
        )?;
        require_sync_level_after(
            &ctx.accounts.instructions.to_account_info(),
            &ctx.accounts.config.key(),
            &ctx.accounts.user_stake.key(),
        )?;

        emit!(DonateEvent {
            user: ctx.accounts.user.key(),
            asset_is_sol: true,
            amount,
            usdt_value: value,
            forfeited_beta: u64::try_from(forfeited_beta).unwrap_or(u64::MAX),
        });
        Ok(())
    }

    /// Donate Alpha ($LIKE, classic SPL). First move the Alpha into escrow, then perform exactly the same accounting as `donate_sol`.
    /// (accounting identical to donate_sol).
    pub fn donate_alpha<'info>(
        ctx: Context<'_, '_, '_, 'info, DonateAlpha<'info>>,
        amount: u64,
    ) -> Result<()> {
        migration::active(&ctx.accounts.config)?;
        require!(amount > 0, DonateError::ZeroAmount);
        require_keys_neq!(
            ctx.accounts.user_stake.referrer,
            Pubkey::default(),
            DonateError::ReferrerRequired
        );
        // `vaults` / `sinks` are nested accounts structs (see PoolVaults/DistVaults) purely so their
        // deserialization gets its own SBF stack frame. A nested struct cannot reference `config`, so
        // the `address = config.*` bindings they used to carry are asserted here instead. This runs
        // before any state is touched, exactly as the declarative constraints did.
        ctx.accounts.vaults.check(&ctx.accounts.config)?;
        ctx.accounts.sinks.check(&ctx.accounts.config)?;
        treasury_burn::business(&mut ctx.accounts.config, &mut ctx.accounts.sinks.treasury,
            &ctx.accounts.beta_mint.to_account_info(), &ctx.accounts.token_program.to_account_info(), ctx.remaining_accounts)?;

        // Forward the base token's transfer-hook extra accounts (passed by the caller as remaining_accounts) —
        // for the current classic SPL Alpha ($LIKE, no hook) this is a no-op, but it is kept so that a future
        // switch to a hooked Alpha still works. anchor-spl's `transfer_checked` would silently drop these accounts.
        transfer_checked_hooked(
            &ctx.accounts.token_program.to_account_info(),
            &ctx.accounts.user_alpha.to_account_info(),
            &ctx.accounts.alpha_mint.to_account_info(),
            &ctx.accounts.alpha_escrow.to_account_info(),
            &ctx.accounts.user.to_account_info(),
            ctx.accounts.pool.donation_ancestors(ctx.remaining_accounts)?,
            amount,
            ctx.accounts.alpha_mint.decimals,
            &[],
        )?;

        oracle::pool_guard(
            &ctx.accounts.config,
            ctx.accounts.pool.reserves(ctx.accounts.vaults.base_vault.amount, ctx.accounts.vaults.quote_vault.amount)?.0,
            ctx.accounts.pool.reserves(ctx.accounts.vaults.base_vault.amount, ctx.accounts.vaults.quote_vault.amount)?.1,
            Clock::get()?.unix_timestamp,
        )?;
        ctx.accounts.prices.guard(&ctx.accounts.config)?;
        // Alpha is an external token — use its mint's real decimals rather than a hardcoded 9.
        let value = usdt_value(
            amount,
            ctx.accounts.config.alpha_usd,
            ctx.accounts.alpha_mint.decimals,
        )?;
        let resume=raydium_migration::business_allowance_resume(ctx.accounts.config.key(), &ctx.accounts.config, ctx.remaining_accounts, false)?;
        ctx.accounts.claim_allowance.checkpoint_with_resume(
            ctx.accounts.config.key(), &ctx.accounts.config, &ctx.accounts.user_stake,
            Clock::get()?.unix_timestamp, ctx.bumps.claim_allowance, resume,
        )?;
        let forfeited_beta = apply_donation(
            &mut ctx.accounts.config,
            &mut ctx.accounts.user_stake,
            ctx.accounts.sinks.treasury.amount,
            value,
        )?;

        // ================= Atomic distribution (docs/donate.md five legs, splitting this amount by config bps) =================
        // Allocate LIKE to treasury conversion, liquidity and operations.
        // The configured operations leg stays in alpha_escrow, transferred out as Alpha by `allocate_operations_direct` in the same tx.
        let a = amount as u128;
        let ops = mul_bps(a, ctx.accounts.config.bps_operations) as u64;
        let treasury_amt = mul_bps(a, ctx.accounts.config.bps_treasury) as u64;
        let liq = amount
            .checked_sub(ops)
            .and_then(|x| x.checked_sub(treasury_amt))
            .ok_or(DonateError::MathOverflow)?;
        ctx.accounts.config.ops_alpha_budget = ctx
            .accounts
            .config
            .ops_alpha_budget
            .checked_add(ops)
            .ok_or(DonateError::MathOverflow)?;
        let slippage = ctx.accounts.config.max_slippage_bps;
        let pool_fee = ctx.accounts.pool.fee_bps;
        let beta_decimals = ctx.accounts.beta_mint.decimals;
        let lp_decimals = ctx.accounts.lp_mint.decimals;
        let seed_key = ctx.accounts.config.config_seed;
        let bump = ctx.accounts.config.bump;
        let signer: &[&[&[u8]]] = &[&[b"donate-config", seed_key.as_ref(), &[bump]]];

        // --- Treasury share: LIKE→FLOCK into treasury ---
        if treasury_amt > 0 {
            ctx.accounts.vaults.base_vault.reload()?;
            ctx.accounts.vaults.quote_vault.reload()?;
            let (rb, rq) = (
                ctx.accounts.vaults.base_vault.amount,
                ctx.accounts.vaults.quote_vault.amount,
            );
            dex_sell_alpha_for_beta(
                ctx.accounts.dex_program.to_account_info(),
                ctx.accounts.config.to_account_info(),
                ctx.accounts.pool.to_account_info(),
                ctx.accounts.alpha_mint.to_account_info(),
                ctx.accounts.beta_mint.to_account_info(),
                ctx.accounts.vaults.base_vault.to_account_info(),
                ctx.accounts.vaults.quote_vault.to_account_info(),
                ctx.accounts.alpha_escrow.to_account_info(),
                ctx.accounts.sinks.treasury.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
                rb,
                rq,
                treasury_amt,
                slippage,
                pool_fee,
                signer,
                ctx.remaining_accounts,
            )?;
        }

        // Only treasury, liquidity and operations allocations remain.

        // --- 50% add liquidity → LP to the independent locker: half the Alpha buys Beta, the other half is paired with it and injected ---
        if liq > 0 {
            let s = liq / 2;
            ctx.accounts.config_beta_ata.reload()?;
            let beta_before = ctx.accounts.config_beta_ata.amount;
            if s > 0 {
                ctx.accounts.vaults.base_vault.reload()?;
                ctx.accounts.vaults.quote_vault.reload()?;
                let (rb, rq) = (
                    ctx.accounts.vaults.base_vault.amount,
                    ctx.accounts.vaults.quote_vault.amount,
                );
                dex_sell_alpha_for_beta(
                    ctx.accounts.dex_program.to_account_info(),
                    ctx.accounts.config.to_account_info(),
                    ctx.accounts.pool.to_account_info(),
                    ctx.accounts.alpha_mint.to_account_info(),
                    ctx.accounts.beta_mint.to_account_info(),
                    ctx.accounts.vaults.base_vault.to_account_info(),
                    ctx.accounts.vaults.quote_vault.to_account_info(),
                    ctx.accounts.alpha_escrow.to_account_info(),
                    ctx.accounts.config_beta_ata.to_account_info(),
                    ctx.accounts.token_program.to_account_info(),
                    rb,
                    rq,
                    s,
                    slippage,
                    pool_fee,
                    signer,
                ctx.remaining_accounts,
                )?;
            }
            ctx.accounts.config_beta_ata.reload()?;
            let beta_got = ctx
                .accounts
                .config_beta_ata
                .amount
                .saturating_sub(beta_before);
            let base_alpha = liq.saturating_sub(s);
            let mut base_added: u64 = 0; // Alpha actually paired into the pool by add_liquidity
            ctx.accounts.vaults.base_vault.reload()?;
            ctx.accounts.vaults.quote_vault.reload()?;
            let live_pool=venue::read(&ctx.accounts.pool.to_account_info())?;
            let (rb,rq)=live_pool.reserves(ctx.accounts.vaults.base_vault.amount,ctx.accounts.vaults.quote_vault.amount)?;
            let (rb,rq)=(rb as u128,rq as u128);
            if beta_got > 0 && base_alpha > 0 && rb > 0 && rq > 0 {
                // Balance the two legs at the current price ratio, avoiding the min() waste in dex add_liquidity.
                let base_by_beta = (beta_got as u128).saturating_mul(rb) / rq;
                let base_use = core::cmp::min(base_alpha as u128, base_by_beta) as u64;
                let quote_use =
                    core::cmp::min(beta_got as u128, (base_use as u128).saturating_mul(rq) / rb)
                        as u64;
                if base_use > 0 && quote_use > 0 {
                    let lp_supply = live_pool.total_lp as u128; // the pool's share ledger, not mint supply
                    let expected_lp = if lp_supply == 0 {
                        0
                    } else {
                        core::cmp::min(
                            (base_use as u128).saturating_mul(lp_supply) / rb,
                            (quote_use as u128).saturating_mul(lp_supply) / rq,
                        )
                    };
                    let min_lp_out =
                        u64::try_from(mul_bps(expected_lp, BPS_DENOM - slippage)).unwrap_or(0);
                    ctx.accounts.config_lp_ata.reload()?;
                    let lp_before = ctx.accounts.config_lp_ata.amount;
                    base_added = venue::add(ctx.accounts.dex_program.to_account_info(), dex_pool::cpi::accounts::AddLiquidity {
                                user: ctx.accounts.config.to_account_info(),
                                pool: ctx.accounts.pool.to_account_info(),
                                base_mint: ctx.accounts.alpha_mint.to_account_info(),
                                quote_mint: ctx.accounts.beta_mint.to_account_info(),
                                base_vault: ctx.accounts.vaults.base_vault.to_account_info(),
                                quote_vault: ctx.accounts.vaults.quote_vault.to_account_info(),
                                lp_mint: ctx.accounts.lp_mint.to_account_info(),
                                lp_lock: ctx.accounts.lp_lock.to_account_info(),
                                user_base: ctx.accounts.alpha_escrow.to_account_info(),
                                user_quote: ctx.accounts.config_beta_ata.to_account_info(),
                                user_lp: ctx.accounts.config_lp_ata.to_account_info(),
                                token_program: ctx.accounts.token_program.to_account_info(),
                            }, base_use, if live_pool.clmm.is_some() { beta_got } else { quote_use }, min_lp_out, signer, ctx.remaining_accounts)?; // CLMM rounds the required LIKER leg up; unused LIKER is swept below.
                    // Fixed venue destination: CPMM LP goes to the incinerator ATA;
                    // the legacy route retains its original Locker account.
                    // This transfer does not burn LP or change either supply ledger.
                    ctx.accounts.config_lp_ata.reload()?;
                    let lp_minted = ctx.accounts.config_lp_ata.amount.saturating_sub(lp_before);
                    if lp_minted > 0 && ctx.accounts.pool.clmm.is_none() {
                        transfer_checked(
                            CpiContext::new_with_signer(
                                ctx.accounts.token_program.to_account_info(),
                                TransferChecked {
                                    from: ctx.accounts.config_lp_ata.to_account_info(),
                                    mint: ctx.accounts.lp_mint.to_account_info(),
                                    to: ctx.accounts.locked_lp_vault.to_account_info(),
                                    authority: ctx.accounts.config.to_account_info(),
                                },
                                signer,
                            ),
                            lp_minted,
                            lp_decimals,
                        )?;
                    }
                }
            }

            // After adding liquidity, any leftover Beta in config_beta_ata not paired into the pool (buy leg overbought/rounding, or
            // add_liquidity skipped) is swept entirely into treasury, not left in the intermediary account.
            ctx.accounts.config_beta_ata.reload()?;
            let beta_left = ctx.accounts.config_beta_ata.amount;
            if beta_left > 0 {
                transfer_checked(
                    CpiContext::new_with_signer(
                        ctx.accounts.token_program.to_account_info(),
                        TransferChecked {
                            from: ctx.accounts.config_beta_ata.to_account_info(),
                            mint: ctx.accounts.beta_mint.to_account_info(),
                            to: ctx.accounts.sinks.treasury.to_account_info(),
                            authority: ctx.accounts.config.to_account_info(),
                        },
                        signer,
                    ),
                    beta_left,
                    beta_decimals,
                )?;
            }

            // Leftover Alpha (the part of the retained half not paired into the pool) is sent directly into the pool's base_vault, deepening the pool without minting LP.
            // Only "this leg's leftover" = base_alpha − already-added is transferred, never touching the Alpha reserved in alpha_escrow for the configured operations leg.
            let alpha_left = base_alpha.saturating_sub(base_added);
            if alpha_left > 0 {
                transfer_checked(
                    CpiContext::new_with_signer(
                        ctx.accounts.token_program.to_account_info(),
                        TransferChecked {
                            from: ctx.accounts.alpha_escrow.to_account_info(),
                            mint: ctx.accounts.alpha_mint.to_account_info(),
                            to: ctx.accounts.vaults.base_vault.to_account_info(),
                            authority: ctx.accounts.config.to_account_info(),
                        },
                        signer,
                    ),
                    alpha_left,
                    ctx.accounts.alpha_mint.decimals,
                )?;
            }
        }

        // ===== Enforced check: this tx must atomically bundle the ops leg covering this 3% =====
        // The configured operations leg cannot be folded into this instruction (this account struct is at the SBF 4 KB stack ceiling), so it is split into
        // a second instruction `allocate_operations_direct` in the same v0 tx. Here we introspect the Instructions sysvar,
        // asserting that instruction exists, acts on the same config, and its amount covers the 3% owed here — otherwise the whole donation reverts.
        // This upgrades the "ops leg" from "optional, backfilled by a later crank" to "atomically enforced in the same tx as the donation".
        require_ops_leg_bundled(
            &ctx.accounts.instructions.to_account_info(),
            &ctx.accounts.config.key(),
            mul_bps(a, ctx.accounts.config.bps_operations) as u64,
        )?;
        require_sync_level_after(
            &ctx.accounts.instructions.to_account_info(),
            &ctx.accounts.config.key(),
            &ctx.accounts.user_stake.key(),
        )?;

        emit!(DonateEvent {
            user: ctx.accounts.user.key(),
            asset_is_sol: false,
            amount,
            usdt_value: value,
            forfeited_beta: u64::try_from(forfeited_beta).unwrap_or(u64::MAX),
        });
        Ok(())
    }

    /// permissionless: advance the global reward accumulator to `now` (the current time). Every
    /// donate/claim already folds a call to it inside, so a standalone "heartbeat" call is optional.
    pub fn accrue(ctx: Context<Accrue>) -> Result<()> {
        migration::active(&ctx.accounts.config)?;
        let now = Clock::get()?.unix_timestamp;
        accrue_global(
            &mut ctx.accounts.config,
            ctx.accounts.treasury.amount,
            now,
        );
        accrue_v(
            &mut ctx.accounts.config,
            ctx.accounts.treasury.amount,
            now,
        );
        Ok(())
    }

    /// Claim accumulated Beta. Deducts from the user's power (i.e. the release budget) the USDT value corresponding to this claim,
    /// capped by "time since last claim" (n days → n×0.5%×power, baseline = last claim / first donation); any remainder carries over, not forfeited.
    /// Beta is paid out from the treasury (authorized by the config PDA's signature).
    pub fn claim<'info>(ctx: Context<'_, '_, 'info, 'info, Claim<'info>>, percent_bps: u16) -> Result<()> {
        require!(matches!(percent_bps, 2500 | 5000 | 7500 | 10000), DonateError::BadClaimPercent);
        migration::active(&ctx.accounts.config)?;
        treasury_burn::business(&mut ctx.accounts.config, &mut ctx.accounts.treasury,
            &ctx.accounts.beta_mint.to_account_info(), &ctx.accounts.token_program.to_account_info(), ctx.remaining_accounts)?;
        let ancestors=ctx.accounts.pool.ancestors(ctx.remaining_accounts)?;
        let now = Clock::get()?.unix_timestamp;
        oracle::pool_guard(
            &ctx.accounts.config,
            ctx.accounts.pool.reserves(ctx.accounts.base_vault.amount,ctx.accounts.quote_vault.amount)?.0,
            ctx.accounts.pool.reserves(ctx.accounts.base_vault.amount,ctx.accounts.quote_vault.amount)?.1,
            now,
        )?;
        ctx.accounts.prices.guard(&ctx.accounts.config)?;
        prepare_claim(&ctx.accounts.config, &mut ctx.accounts.user_stake, now)?;
        accrue_v(
            &mut ctx.accounts.config,
            ctx.accounts.treasury.amount,
            now,
        );
        settle_v(
            &mut ctx.accounts.user_stake,
            &VSnap::of(&ctx.accounts.config),
        );
        let config_key = ctx.accounts.config.key();
        let user_stake_key = ctx.accounts.user_stake.key();

        // ---- Withdrawal gates: T+delay required after the first donate; two withdrawals must be spaced by cooldown (anti-flash-loan) ----
        {
            let us = &ctx.accounts.user_stake;
            let cfg = &ctx.accounts.config;
            require!(
                now >= us
                    .first_donate_ts
                    .saturating_add(cfg.claim_first_delay_secs),
                DonateError::ClaimTooEarly
            );
            require!(
                us.last_claim_ts == 0
                    || now >= us.last_claim_ts.saturating_add(CLAIM_COOLDOWN),
                DonateError::ClaimCooldown
            );
        }

        let treasury_beta = ctx.accounts.treasury.amount;

        // First advance the global accumulator (accrue), then settle the newly accrued reward into the user's pending bucket.
        accrue_global(&mut ctx.accounts.config, treasury_beta, now);
        let acc = ctx.accounts.config.acc_reward_per_power;
        settle(&mut ctx.accounts.user_stake, acc);

        // The Beta price (USDT-e6, per whole Beta) is taken from the keeper-set fallback oracle
        // (config.beta_usd), *not* the live pool-price ratio — so a user cannot flash-manipulate this thin
        // pool to depress their claim's valuation and thereby burn (deduct) less power (this is an audit fix).
        // Mainnet's "on-chain live read + EMA protection" is future work (§2.1/§9).
        let us = &mut ctx.accounts.user_stake;
        let spot_beta_usd = ctx.accounts.config.beta_usd as u128;
        require!(spot_beta_usd > 0, DonateError::PriceUnset);
        // FL-01: clamp the pricing with the "anchor-tolerance floor" to prevent permissionlessly depressing beta_usd and then claiming pending while deducting almost no power.
        let beta_usd = beta_usd_with_floor(&ctx.accounts.config, spot_beta_usd);
        let beta_unit = 10u128.pow(ctx.accounts.beta_mint.decimals as u32);

        let resume=raydium_migration::business_allowance_resume(config_key, &ctx.accounts.config, ctx.remaining_accounts, false)?;
        ctx.accounts.claim_allowance.checkpoint_with_resume(
            config_key, &ctx.accounts.config, us, now, ctx.bumps.claim_allowance, resume,
        )?;
        // Only gross static payments spend the carried time-value allowance.
        let static_candidate = us.pending.min(ctx.accounts.claim_allowance.credit / beta_usd);
        let candidates = [static_candidate, us.promo_pending, us.v_pending];
        let (total_take_beta, takes) = combined_claim_amounts(
            candidates, us.power.saturating_mul(beta_unit) / beta_usd,
            treasury_beta as u128, percent_bps,
        )?;
        let [static_take_beta, promo_take_beta, level_take_beta] = takes;
        let total_take_v = total_take_beta.saturating_mul(beta_usd) / beta_unit;
        let power_left_v = us.power.saturating_sub(total_take_v);
        require!(total_take_beta > 0, DonateError::NothingToClaim);
        us.power = power_left_v;
        ctx.accounts.config.total_power =
            ctx.accounts.config.total_power.saturating_sub(total_take_v);
        ctx.accounts.claim_allowance.spend(static_take_beta, beta_usd)?;
        us.pending = us.pending.saturating_sub(static_take_beta);
        us.promo_pending -= promo_take_beta;
        us.v_pending -= level_take_beta;
        let forfeited_beta = clear_exhausted(us, beta_usd, beta_unit);
        if us.power.saturating_mul(beta_unit) / beta_usd == 0 {
            ctx.accounts.claim_allowance.clear();
        }
        us.reward_debt = us.power.saturating_mul(acc) / ACC_SCALE;
        us.last_claim_ts = now; // cooldown only; unused static allowance is carried separately

        let total_take_beta_u64 =
            u64::try_from(total_take_beta).map_err(|_| DonateError::MathOverflow)?;

        // ---- Withdrawal fee (anti-dump tiers) + auto-sell the net Beta into Alpha to the user (shared settlement kernel) ----
        let (base_reserve,quote_reserve)=ctx.accounts.pool.reserves(ctx.accounts.base_vault.amount,ctx.accounts.quote_vault.amount)?;
        let (base_reserve,quote_reserve)=(base_reserve as u128,quote_reserve as u128);
        require!(
            base_reserve > 0 && quote_reserve > 0,
            DonateError::EmptyPool
        );
        let open_price = if ctx.accounts.pool.cpmm.is_some() || ctx.accounts.pool.clmm.is_some() {raydium_migration::day_price(ctx.accounts.config.key(),ctx.accounts.pool.key(),ctx.remaining_accounts,now,None)?}else{effective_open_price(&ctx.accounts.pool,now)};
        let pool_fee_bps = ctx.accounts.pool.fee_bps;
        let base_decimals = ctx.accounts.base_mint.decimals;
        let beta_decimals = ctx.accounts.beta_mint.decimals;
        // First clone each leg's AccountInfo (owned), then take the mutable borrows of config/alpha_escrow, avoiding borrow conflicts.
        let token_program_ai = ctx.accounts.token_program.to_account_info();
        let config_ai = ctx.accounts.config.to_account_info();
        let treasury_ai = ctx.accounts.treasury.to_account_info();
        let beta_mint_ai = ctx.accounts.beta_mint.to_account_info();
        let operations_beta_ai = ctx.accounts.operations_beta.to_account_info();
        let base_mint_ai = ctx.accounts.base_mint.to_account_info();
        let base_vault_ai = ctx.accounts.base_vault.to_account_info();
        let quote_vault_ai = ctx.accounts.quote_vault.to_account_info();
        let pool_ai = ctx.accounts.pool.to_account_info();
        let user_alpha_ai = ctx.accounts.user_alpha.to_account_info();
        let dex_program_ai = ctx.accounts.dex_program.to_account_info();
        let (alpha_out, fee_beta) = settle_claim_payout(
            &mut ctx.accounts.config,
            &mut ctx.accounts.alpha_escrow,
            &token_program_ai,
            &config_ai,
            &treasury_ai,
            &beta_mint_ai,
            &operations_beta_ai,
            &base_mint_ai,
            &base_vault_ai,
            &quote_vault_ai,
            &pool_ai,
            &user_alpha_ai,
            &dex_program_ai,
            total_take_beta,
            base_reserve,
            quote_reserve,
            open_price,
            pool_fee_bps,
            base_decimals,
            beta_decimals,
            ctx.remaining_accounts,
        )?;
        ctx.accounts.pool.reload()?; // the CPI mutated pool; reload to avoid reading stale state

        let caller_is_root = us.referrer == us.owner;
        if caller_is_root { require!(ancestors.is_empty(), DonateError::BadChain); }
        if !caller_is_root {
            let promo_rate = ctx.accounts.config.promo_rate_bps;
            let promo_min_direct = ctx.accounts.config.promo_min_direct;
            let promo_min_power = ctx.accounts.config.promo_min_power;
            let config_key = ctx.accounts.config.key();
            let mut expected_parent = us.referrer;
            let mut seen: Vec<Pubkey> = Vec::with_capacity(MAX_UPLINE + 1);
            seen.push(us.key());
            let mut g: usize = 0; // generations already counted for the promotion reward ("valid generations", 0-based index)
            let mut promo_done = false;
            let n = ancestors.len();
            require!(n <= MAX_UPLINE, DonateError::BadChain);
            let mut reached_root = false;
            for (i, ai) in ancestors.iter().enumerate() {
                let mut anc = load_checked_ancestor(ai, expected_parent, config_key, &mut seen)?;
                let anc_is_root = anc.referrer == anc.owner;

                if !promo_done && anc.power > 0 {
                    let rate = promo_rate[g];
                    if rate == 0 {
                        promo_done = true; // the rate is 0 from this generation on → no later generation pays
                    } else {
                        if anc.direct_valid_count as u128 >= promo_min_direct[g] as u128
                            && anc.power >= promo_min_power[g]
                        {
                            let credit =
                                static_take_beta.saturating_mul(rate as u128) / BPS_DENOM as u128;
                            anc.promo_pending = anc.promo_pending.saturating_add(credit);
                        }
                        g += 1; // only a "valid (has power)" upline consumes a generation
                        if g >= MAX_UPLINE {
                            promo_done = true;
                        }
                    }
                }

                expected_parent = anc.referrer;
                anc.exit(&crate::ID)?;
                if anc_is_root {
                    reached_root = true;
                    // The root must be the last account supplied (no trailing accounts past it).
                    require!(i == n - 1, DonateError::BadChain);
                    break;
                }
            }
            // Whole-chain enforcement (M-02): either reach the root or traverse exactly MAX_UPLINE
            // levels — a claimant deep in the tree passes exactly 10 uplines; a shallow one reaches
            // the root. Omitting uplines to dodge reward accounting now reverts the claim.
            require!(reached_root || n == MAX_UPLINE, DonateError::BadChain);
        }
        require_sync_level_after(
            &ctx.accounts.instructions.to_account_info(),
            &config_key,
            &user_stake_key,
        )?;

        emit!(ClaimEvent {
            user: ctx.accounts.user.key(),
            percent_bps,
            static_beta: static_take_beta as u64,
            promotion_beta: promo_take_beta as u64,
            level_beta: level_take_beta as u64,
            beta: total_take_beta_u64,
            value: total_take_v,
            power_left: us.power,
            fee: u64::try_from(fee_beta).unwrap_or(u64::MAX),
            alpha_out,

            forfeited_beta: u64::try_from(forfeited_beta).unwrap_or(u64::MAX),
        });
        Ok(())
    }

    /// permissionless: propagate a user's power change **upward** along the invite chain, refreshing in place
    /// the levels of each upline along the way (and of the user). The upline chain A1..Ak of `user_stake` (k = min(depth, 10), nearest
    /// upline first) is passed as `remaining_accounts`, all of which must be **writable** UserStake PDAs.
    ///
    /// It uses `synced_power` (the already-propagated baseline of the user's own power) for delta (Δ) tracking, making it **idempotent and self-healing**: the dApp
    /// places it immediately after every donate/claim, while permissionless reruns can reconcile stale caches after administrative threshold changes. Δ affects exactly
    /// the team power of the user's levels 1..10 uplines (level 1 also includes direct-referral power), so at most 10 levels are traversed.
    ///
    /// Chain safety: each hop requires `anc.owner == the expected parent` (bound to the immutable `referrer`) and re-checks the canonical PDA,
    /// so the caller can only pass real uplines and cannot inflate team power. It enforces "reach the root **or** exactly 10 levels", preventing silent under-updates
    /// caused by omitting uplines.
    pub fn sync_level<'info>(ctx: Context<'_, '_, 'info, 'info, SyncLevel<'info>>) -> Result<()> {
        migration::active(&ctx.accounts.config)?;
        // 0) Top (once per ix): advance the V-pool accumulators to now, then snapshot the
        //    accumulators + thresholds into stack locals, releasing the immutable borrow of config
        //    — so later, while holding `&mut config` (updating the tier denominators), levels can
        //    still be determined. `snap` is constant within this ix and is the sole basis for
        //    settlement / re-anchoring.
        let now = Clock::get()?.unix_timestamp;
        accrue_v(
            &mut ctx.accounts.config,
            ctx.accounts.treasury.amount,
            now,
        );
        let snap = VSnap::of(&ctx.accounts.config);
        let th = LevelThresholds::of(&ctx.accounts.config);
        // "Valid direct referral" threshold snapshot: when the user's power crosses it, increment/decrement the direct_valid_count of their direct upline (level 1).
        let valid_min_power = ctx.accounts.config.valid_invite_min_power;

        // 1) The user (caller): team_power is unchanged, but a change in own power may move level into/out of a V tier. Uniformly go through
        //    "unbucket (settle by old tier + subtract old weight) → determine level → rebucket (count into new tier + re-anchor)", then compute Δ and advance the baseline.
        let delta: i128;
        // `leg_power` propagation is gated on this user having been seeded: until `seed_leg` has
        // folded their own power into their uplines' legs, those legs simply do not contain them,
        // and adding a Δ on top would double-count once the seed lands.
        let leg_delta: i128;
        let valid_flip: i64; // change in the user's "valid" status: +1 crossing in / -1 crossing out / 0 unchanged (applied to the level-1 upline).
        let mut star_carry: i32; // ±1 to apply to the next upline's `direct_ge6`
        let mut prev_leg: u64; // leg_power of the node one hop below the ancestor being processed
        let mut expected_parent: Pubkey;
        let is_root: bool;
        {
            v_unbucket(
                &mut ctx.accounts.config,
                &mut ctx.accounts.user_stake,
                &snap,
            );
            let (old_level, seeded) = {
                let us = &ctx.accounts.user_stake;
                (us.level, us.flags & FLAG_LEG_SEEDED != 0)
            };
            {
                let us = &mut ctx.accounts.user_stake;
                delta = (us.power as i128) - (us.synced_power as i128);
                leg_delta = if seeded { delta } else { 0 };
                us.leg_power = add_delta_u64(us.leg_power, leg_delta);
            }
            let lvl = level_of(&ctx.accounts.user_stake, &th);
            v_rebucket(
                &mut ctx.accounts.config,
                &mut ctx.accounts.user_stake,
                &snap,
                lvl,
            );
            let us = &mut ctx.accounts.user_stake;
            star_carry = if seeded { star_flip(old_level, lvl) } else { 0 };
            prev_leg = us.leg_power;
            // Valid-direct-referral crossing detection: whether the user is currently "valid" (power ≥ threshold), compared with the previously recorded is_valid.
            let valid_now = us.power >= valid_min_power;
            valid_flip = match (us.is_valid, valid_now) {
                (false, true) => 1,
                (true, false) => -1,
                _ => 0,
            };
            us.is_valid = valid_now;
            us.synced_power = us.power;
            expected_parent = us.referrer;
            is_root = us.referrer == us.owner;
        }

        // 2) The root node has no upline.
        if is_root {
            require!(ctx.remaining_accounts.is_empty(), DonateError::BadChain);
            return Ok(());
        }

        let n = ctx.remaining_accounts.len();
        // One slot past MAX_UPLINE: Δ still stops at 10 generations, but the 11th account exists to
        // absorb the 10th's star-count change (see the closing require!).
        require!(n <= MAX_UPLINE + 1, DonateError::BadChain);

        // Dedup set (including the caller itself): prevent aliased accounts appearing twice from causing lost "read-modify-write" updates.
        let mut seen: Vec<Pubkey> = Vec::with_capacity(MAX_UPLINE + 2);
        seen.push(ctx.accounts.user_stake.key());

        let config_key = ctx.accounts.config.key();
        let mut reached_root = false;
        for (i, ai) in ctx.remaining_accounts.iter().enumerate() {
            // Chain-safety checks (writable / owned by this program / deduped / owner==expected parent / canonical PDA) — shared with claim.
            let mut anc = load_checked_ancestor(ai, expected_parent, config_key, &mut seen)?;
            let in_delta_range = i < MAX_UPLINE;
            let in_leg_range = i < LEG_UPLINE;

            // V-pool order: unbucket (settle old tier + subtract old weight) → apply Δ → determine level → rebucket (count into new tier + re-anchor).
            // Every write to `team_power` / `max_leg` below sits inside this pair by construction.
            v_unbucket(&mut ctx.accounts.config, &mut anc, &snap);
            // The level-1 upline (direct upline): direct-referral power changes by Δ, and the user's "valid" status flip also affects only the direct upline.
            if i == 0 {
                anc.direct_power = saturating_add_i128(anc.direct_power, delta);
                if valid_flip != 0 {
                    anc.direct_valid_count =
                        (anc.direct_valid_count as i64 + valid_flip).max(0) as u32;
                }
            }
            if in_delta_range {
                anc.team_power = saturating_add_i128(anc.team_power, delta);
                if leg_delta != 0 {
                    max_leg::invalidate(&mut anc)?;
                }
            }
            if in_leg_range {
                anc.leg_power = add_delta_u64(anc.leg_power, leg_delta);
            }
            // The leg directly below this ancestor just moved. Raise-only here (this ix sees a
            // single leg); `refresh_max_leg` is what lowers it, from the whole child set.
            if anc.max_leg < prev_leg {
                anc.max_leg = prev_leg;
            }
            if star_carry != 0 {
                anc.direct_ge6 =
                    (anc.direct_ge6 as i64 + star_carry as i64).clamp(0, u32::MAX as i64) as u32;
            }
            let old_level = anc.level;
            let lvl = level_of(&anc, &th);
            v_rebucket(&mut ctx.accounts.config, &mut anc, &snap, lvl);
            star_carry = if anc.flags & FLAG_LEG_SEEDED != 0 {
                star_flip(old_level, lvl)
            } else {
                0
            };
            prev_leg = anc.leg_power;

            let anc_is_root = anc.referrer == anc.owner;
            expected_parent = anc.referrer;
            // Write back (these uplines are not in the Accounts struct, so Anchor won't auto-serialize; manual exit needed).
            anc.exit(&crate::ID)?;

            if anc_is_root {
                reached_root = true;
                // The root has no upline of its own, so its star change has nowhere to go — and needs nowhere.
                star_carry = 0;
                // Reached the root; it must be the last account passed in.
                require!(i == n - 1, DonateError::BadChain);
                break;
            }
        }
        // Whole-chain enforcement: reach the root or traverse the full chain — preventing a client
        // from silently under-updating by omitting uplines.
        //
        // `star_carry == 0` is the other half, and it is a security check, not tidiness: without it
        // anyone could pay 5000 lamports to run `sync_level` with a chain one hop too short and
        // drop a pending V6-boundary change on the floor, permanently denying (or gifting) a
        // stranger their V7/V8 star. It is always satisfiable because monotone thresholds keep a
        // star-only level move inside {V6, V7, V8} — so the carry dies at the first hop that Δ did
        // not also touch, i.e. by slot MAX_UPLINE + 1.
        require!(
            (reached_root || n >= MAX_UPLINE) && star_carry == 0,
            DonateError::BadChain
        );
        Ok(())
    }

    pub fn set_level_rewards(
        ctx: Context<LevelRewardAdmin>,
        weights: [u16; V_TIERS],
        share_cap_bps: u16,
    ) -> Result<()> {
        require!(
            weights.iter().map(|x| *x as u32).sum::<u32>() > 0 && share_cap_bps <= BPS_DENOM,
            DonateError::BadRatios
        );
        accrue_v(
            &mut ctx.accounts.config,
            ctx.accounts.treasury.amount,
            Clock::get()?.unix_timestamp,
        );
        ctx.accounts.config.level_weights = weights;
        ctx.accounts.config.level_share_cap_bps = share_cap_bps;

        Ok(())
    }

    /// Change the total V1–V8 budget rate; previously accrued time uses the old rate.
    pub fn set_level_reward_rate(ctx: Context<LevelRewardAdmin>, rate_bps: u16) -> Result<()> {
        require!(rate_bps <= BPS_DENOM, DonateError::BadRatios);
        accrue_v(
            &mut ctx.accounts.config,
            ctx.accounts.treasury.amount,
            Clock::get()?.unix_timestamp,
        );
        // Zero is a valid explicit rate; -1 distinguishes it from legacy zero (= 60 bps).
        ctx.accounts.config.min_sync_interval_secs = if rate_bps == 0 { -1 } else { rate_bps as i64 };
        Ok(())
    }

    /// permissionless, **one-shot per user**: backfill the small-area bookkeeping for an
    /// account that predates it.
    ///
    /// `leg_power` and the uplines' `direct_ge6` are subtree aggregates — they cannot be derived
    /// from any single account's stored state, so each user contributes its own figures exactly
    /// once: `synced_power` is added to its own `leg_power` and to that of every upline within
    /// `LEG_UPLINE` generations, and the direct upline's star counter picks this user up if its
    /// cached level is already >= `STAR_COUNT_LEVEL`.
    ///
    /// Seeded from `synced_power` rather than `power` deliberately: that is the figure already
    /// reflected in every upline's `team_power`, and keeping the two in lockstep is what preserves
    /// `Σ(child.leg_power) ≡ parent.team_power`.
    ///
    /// Idempotent via `FLAG_LEG_SEEDED`, and `sync_level` skips leg/star propagation for an
    /// unseeded user, so the seed and the Δ stream can never double-count each other — in either
    /// order. Accounts created *after* this upgrade are born seeded (see `bind_referrer`), so this
    /// instruction rejects them: their zero starting power is already reflected everywhere.
    pub fn seed_leg<'info>(ctx: Context<'_, '_, 'info, 'info, SeedLeg<'info>>) -> Result<()> {
        migration::active(&ctx.accounts.config)?;
        let (contribution, counts_star, is_root, referrer) = {
            let us = &mut ctx.accounts.user_stake;
            require!(us.flags & FLAG_LEG_SEEDED == 0, DonateError::AlreadySeeded);
            let c =
                u64::try_from(us.synced_power).map_err(|_| error!(DonateError::MathOverflow))?;
            us.leg_power = us.leg_power.saturating_add(c);
            us.flags |= FLAG_LEG_SEEDED;
            (
                c,
                us.level >= STAR_COUNT_LEVEL,
                us.referrer == us.owner,
                us.referrer,
            )
        };
        ctx.accounts.config.leg_seeded_count =
            ctx.accounts.config.leg_seeded_count.saturating_add(1);
        if is_root {
            require!(ctx.remaining_accounts.is_empty(), DonateError::BadChain);
            return Ok(());
        }
        let n = ctx.remaining_accounts.len();
        require!(n <= MAX_UPLINE, DonateError::BadChain);
        let config_key = ctx.accounts.config.key();
        let mut seen: Vec<Pubkey> = Vec::with_capacity(LEG_UPLINE + 1);
        seen.push(ctx.accounts.user_stake.key());
        let mut expected_parent = referrer;
        let mut reached_root = false;
        for (i, ai) in ctx.remaining_accounts.iter().enumerate() {
            let mut anc = load_checked_ancestor(ai, expected_parent, config_key, &mut seen)?;
            if contribution != 0 {
                max_leg::invalidate(&mut anc)?;
            }
            if i < LEG_UPLINE {
                anc.leg_power = anc.leg_power.saturating_add(contribution);
            }
            if i == 0 && counts_star {
                anc.direct_ge6 = anc.direct_ge6.saturating_add(1);
            }
            let anc_is_root = anc.referrer == anc.owner;
            expected_parent = anc.referrer;
            anc.exit(&crate::ID)?;
            if anc_is_root {
                reached_root = true;
                require!(i == n - 1, DonateError::BadChain);
                break;
            }
        }
        // Same whole-chain rule as sync_level: a short chain would leave this user's power missing
        // from an upline's leg, which under-states `max_leg` and therefore over-states the small area.
        require!(reached_root || n == MAX_UPLINE, DonateError::BadChain);
        Ok(())
    }

    /// Start/resume a permissionless bounded refresh. An unchanged live scan cannot be reset.
    pub fn begin_max_leg_refresh(ctx: Context<BeginMaxLegRefresh>) -> Result<()> {
        max_leg::begin(ctx)
    }

    /// Process at most 16 consecutive referral indices; commit only a complete, stable scan.
    pub fn refresh_max_leg_page<'info>(
        ctx: Context<'_, '_, 'info, 'info, RefreshMaxLegPage<'info>>,
        revision: u64,
        cursor: u32,
    ) -> Result<()> {
        max_leg::page(ctx, revision, cursor)
    }

    /// Legacy atomic path for small parents. Large parents use refresh_max_leg_page.
    /// permissionless: recompute `max_leg` exactly, from **every** direct child.
    ///
    /// `sync_level` only ever raises `max_leg` because it sees one leg at a time; lowering it needs
    /// the whole set. Supplying all `direct_count` children is enforced, so the answer cannot be
    /// biased by omitting the largest one.
    ///
    /// The level is deliberately left alone here. A small-area move could cross the `STAR_COUNT_LEVEL`
    /// boundary, and this context carries no upline account to record that against — so this
    /// instruction refreshes the V-pool weight only, and `sync_level` picks up the level.
    pub fn refresh_max_leg<'info>(
        ctx: Context<'_, '_, 'info, 'info, SyncLevel<'info>>,
    ) -> Result<()> {
        migration::active(&ctx.accounts.config)?;
        let now = Clock::get()?.unix_timestamp;
        accrue_v(
            &mut ctx.accounts.config,
            ctx.accounts.treasury.amount,
            now,
        );
        let snap = VSnap::of(&ctx.accounts.config);
        let (owner, direct_count, level) = {
            let us = &ctx.accounts.user_stake;
            (us.owner, us.direct_count, us.level)
        };
        let n = ctx.remaining_accounts.len();
        require!(n as u64 == direct_count as u64, DonateError::BadChain);
        let config_key = ctx.accounts.config.key();
        let mut seen: Vec<Pubkey> = Vec::with_capacity(n + 1);
        seen.push(ctx.accounts.user_stake.key());
        let mut max_leg: u64 = 0;
        for ai in ctx.remaining_accounts.iter() {
            // Read-only: children are never written here, so they need not be writable.
            require_keys_eq!(*ai.owner, crate::ID, DonateError::BadChain);
            require!(!seen.contains(&ai.key()), DonateError::BadChain);
            seen.push(ai.key());
            let child = Account::<UserStake>::try_from(ai)?;
            require_keys_eq!(child.referrer, owner, DonateError::BadChain);
            // `create_program_address` with the stored bump rather than `find_program_address`:
            // same guarantee (a wrong bump yields a different address and fails the compare) at a
            // fraction of the compute, which matters at up to `direct_count` children per call.
            let pda = Pubkey::create_program_address(
                &[
                    b"donate-user",
                    config_key.as_ref(),
                    child.owner.as_ref(),
                    &[child.bump],
                ],
                &crate::ID,
            )
            .map_err(|_| error!(DonateError::BadChain))?;
            require_keys_eq!(pda, ai.key(), DonateError::BadChain);
            if child.leg_power > max_leg {
                max_leg = child.leg_power;
            }
        }
        // `max_leg` is a weight input, so the write sits inside an unbucket/rebucket pair.
        v_unbucket(
            &mut ctx.accounts.config,
            &mut ctx.accounts.user_stake,
            &snap,
        );
        ctx.accounts.user_stake.max_leg = max_leg;
        v_rebucket(
            &mut ctx.accounts.config,
            &mut ctx.accounts.user_stake,
            &snap,
            level,
        );
        Ok(())
    }

    /// Toggle level accrual after settling the old enabled state.
    pub fn set_v_dividend_switch(ctx: Context<LevelRewardAdmin>, on: bool) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let vault_amt = ctx.accounts.treasury.amount;
        accrue_v(&mut ctx.accounts.config, vault_amt, now);
        let cfg = &mut ctx.accounts.config;
        cfg.v_dividend_enabled = on;
        cfg.v_last_update_ts = now;
        Ok(())
    }

    /// permissionless: transfer `amount_in` Alpha from `alpha_escrow` straight to the operations wallet's
    /// Alpha ATA. This is the docs/donate.md "configured operations" leg; called alongside within the same tx by a donate
    /// transaction, to consume the operations share left in escrow by `donate_alpha` (`amount_in` is the configured operations share of this
    /// donation, passed by the dApp).
    ///
    /// The ops leg used to sell this Alpha into native SOL via Raydium AMM v4. Paying operations in Alpha
    /// directly removes the swap: no Raydium/OpenBook account set (20 accounts → 5), no slippage loss, no
    /// dependence on that pool's depth, and no temporary WSOL ATA to fund + close. The destination stays
    /// pinned to `config.operations_wallet`'s ATA, so this leg is still unredirectable.
    ///
    /// Kept as a separate instruction rather than folded into `donate_alpha`: that account struct is at the
    /// SBF 4 KB stack ceiling (see docs) and cannot take more accounts.
    pub fn allocate_operations_direct(
        ctx: Context<AllocateOperationsDirect>,
        amount_in: u64,
    ) -> Result<()> {
        migration::active(&ctx.accounts.config)?;
        ctx.accounts.config.ops_alpha_budget = ctx
            .accounts
            .config
            .ops_alpha_budget
            .checked_sub(amount_in)
            .ok_or(DonateError::InsufficientEscrow)?;
        require!(amount_in > 0, DonateError::ZeroAmount);
        require!(
            amount_in <= ctx.accounts.alpha_escrow.amount,
            DonateError::InsufficientEscrow
        );

        let seed_key = ctx.accounts.config.config_seed;
        let bump = ctx.accounts.config.bump;
        let signer: &[&[&[u8]]] = &[&[b"donate-config", seed_key.as_ref(), &[bump]]];

        transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.alpha_escrow.to_account_info(),
                    mint: ctx.accounts.alpha_mint.to_account_info(),
                    to: ctx.accounts.operations_alpha_ata.to_account_info(),
                    authority: ctx.accounts.config.to_account_info(),
                },
                signer,
            ),
            amount_in,
            ctx.accounts.alpha_mint.decimals,
        )?;

        // min_out is meaningless without a swap; report the transferred amount on both fields so the
        // event stays shape-compatible with the other allocation legs.
        emit!(AllocateEvent {
            amount_in,
            min_out: amount_in,
        });
        Ok(())
    }

    /// SOL donation **buy leg** (allocate_sol_buy): mandatorily bundled by `donate_sol` in the same v0 tx (see
    /// `require_sol_donation_bundled`). `amount` = the full donation amount (lamports), which `donate_sol` already wrapped into
    /// WSOL in `config_wsol_ata`. Spend the non-operations allocation on Alpha ($LIKE) via Raydium,
    /// validate the exact debit, minimum credit and unchanged token permissions, then pay the
    /// configured operations share in WSOL. Unregistered escrow balances remain untouched.
    /// `allocate_sol_distribute` distributes the bought Alpha by 45/50 treasury/liquidity after operations. Accounts ≈ the Raydium swap account set
    /// (splitting into buy/distribute instructions avoids exceeding the 4 KB SBF stack frame when packing both the Raydium and self-built-dex account sets into one instruction).
    pub fn allocate_sol_buy<'info>(
        ctx: Context<'_, '_, 'info, 'info, AllocateSolBuy<'info>>,
        amount: u64,
    ) -> Result<()> {
        migration::active(&ctx.accounts.config)?;
        ctx.accounts.config.sol_budget = ctx
            .accounts
            .config
            .sol_budget
            .checked_sub(amount)
            .ok_or(DonateError::InsufficientEscrow)?;
        require!(amount > 0, DonateError::ZeroAmount);

        // Only registered input is spendable. Check the external CPI's actual token debit below;
        // passing an amount argument alone does not constrain the callee's signer authority.
        require!(
            ctx.accounts.config_wsol_ata.amount >= amount,
            DonateError::InsufficientEscrow
        );

        let bps_operations = ctx.accounts.config.bps_operations;
        let ops = mul_bps(amount as u128, bps_operations) as u64;
        let convert = amount
            .checked_sub(ops)
            .ok_or(error!(DonateError::MathOverflow))?;

        let seed_key = ctx.accounts.config.config_seed;
        let bump = ctx.accounts.config.bump;
        let signer: &[&[&[u8]]] = &[&[b"donate-config", seed_key.as_ref(), &[bump]]];

        // ---- Raydium AMM v4: WSOL to Alpha(LIKE), fixed input ----
        // Compute min_out on-chain from pool reserves: coin_out = coin_reserve × convert / (pc_reserve + convert).
        let coin_reserve = ctx.accounts.market.amm_coin_vault.amount as u128; // LIKE
        let pc_reserve = ctx.accounts.market.amm_pc_vault.amount as u128; // WSOL
        require!(coin_reserve > 0 && pc_reserve > 0, DonateError::EmptyPool);
        oracle::fresh(&ctx.accounts.config, Clock::get()?.unix_timestamp)?;
        require!(
            oracle::within(
                pc_reserve * PRICE_SCALE / coin_reserve,
                ctx.accounts.config.like_sol_anchor,
                ctx.accounts.config.max_sync_deviation_bps,
            ),
            DonateError::PoolPriceDeviation
        );
        let alpha_out = coin_reserve.saturating_mul(convert as u128)
            / pc_reserve.saturating_add(convert as u128);
        let min_out = u64::try_from(mul_bps(alpha_out, BPS_DENOM - RAYDIUM_SLIPPAGE_BPS))
            .map_err(|_| error!(DonateError::MathOverflow))?;

        require!(min_out > 0, DonateError::ExternalSwapOutput);
        let wsol_before = escrow_guard::Snapshot::capture(
            &ctx.accounts.config_wsol_ata.to_account_info(),
            &WSOL_MINT,
            &ctx.accounts.config.key(),
            true,
        )?;
        let alpha_before = escrow_guard::Snapshot::capture(
            &ctx.accounts.alpha_escrow.to_account_info(),
            &ctx.accounts.config.alpha_mint,
            &ctx.accounts.config.key(),
            false,
        )?;
        {
            // 17 accounts, Raydium's swap_base_in layout, source=WSOL → buy direction: spend WSOL, receive Alpha.
            let accounts: [AccountInfo; 17] = [
                ctx.accounts.token_program.to_account_info(),
                ctx.accounts.market.amm.to_account_info(),
                ctx.accounts.market.amm_authority.to_account_info(),
                ctx.accounts.market.amm_open_orders.to_account_info(),
                ctx.accounts.market.amm_coin_vault.to_account_info(),
                ctx.accounts.market.amm_pc_vault.to_account_info(),
                ctx.accounts.market.serum_program.to_account_info(),
                ctx.accounts.market.serum_market.to_account_info(),
                ctx.accounts.market.serum_bids.to_account_info(),
                ctx.accounts.market.serum_asks.to_account_info(),
                ctx.accounts.market.serum_event_queue.to_account_info(),
                ctx.accounts.market.serum_coin_vault.to_account_info(),
                ctx.accounts.market.serum_pc_vault.to_account_info(),
                ctx.accounts.market.serum_vault_signer.to_account_info(),
                ctx.accounts.config_wsol_ata.to_account_info(), // user source (WSOL)
                ctx.accounts.alpha_escrow.to_account_info(),    // user dest (LIKE)
                ctx.accounts.config.to_account_info(),          // user owner (config PDA signs)
            ];
            raydium_swap_base_in(
                ctx.accounts.market.raydium_program.to_account_info(),
                accounts,
                convert,
                min_out,
                signer,
            )?;
        }

        // Read raw post-CPI state: Anchor constraints only ran before the external invocation.
        let received = escrow_guard::verify_swap(
            &wsol_before,
            &alpha_before,
            &ctx.accounts.config_wsol_ata.to_account_info(),
            &ctx.accounts.alpha_escrow.to_account_info(),
            convert,
            min_out,
        )?;
        ctx.accounts.config_wsol_ata.reload()?;
        ctx.accounts.alpha_escrow.reload()?;
        ctx.accounts.config.sol_alpha_budget = ctx
            .accounts
            .config
            .sol_alpha_budget
            .checked_add(received)
            .ok_or(DonateError::MathOverflow)?;
        // Pay only the registered operations allocation. Unsolicited WSOL stays unassigned.
        // Operations receives redeemable WSOL; never close/sweep someone else's unassigned balance.
        if ops > 0 {
            transfer_checked(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    TransferChecked {
                        from: ctx.accounts.config_wsol_ata.to_account_info(),
                        mint: ctx.accounts.wsol_mint.to_account_info(),
                        to: ctx.accounts.operations_wsol_ata.to_account_info(),
                        authority: ctx.accounts.config.to_account_info(),
                    },
                    signer,
                ),
                ops,
                9,
            )?;
        }

        emit!(AllocateEvent {
            amount_in: convert,
            min_out,
        });
        Ok(())
    }

    /// SOL donation **distribution leg** (allocate_sol_distribute): distribute the Alpha bought into `alpha_escrow` by `allocate_sol_buy`
    /// by 45/50 treasury/liquidity after operations (**normalized over the non-ops weight 9500**): treasury /
    /// add-liquidity→LP to blackhole — same source as `donate_alpha`'s self-built-dex distribution leg (kept consistent). Mandatorily
    /// bundled by `donate_sol`; this leg additionally introspects to assert an earlier `allocate_sol_buy` acting on the same config exists in the tx (ensuring the buy leg already ran and
    /// escrow already holds Alpha). The distribution accounts ≈ the self-built-dex distribution portion of `donate_alpha`.
    pub fn allocate_sol_distribute<'info>(ctx: Context<'_, '_, 'info, 'info, AllocateSolDistribute<'info>>) -> Result<()> {
        // See PoolVaults/DistVaults: nested purely for the SBF stack frame, so the
        // `address = config.*` bindings are asserted here instead.
        ctx.accounts.vaults.check(&ctx.accounts.config)?;
        oracle::pool_guard(
            &ctx.accounts.config,
            ctx.accounts.pool.reserves(ctx.accounts.vaults.base_vault.amount, ctx.accounts.vaults.quote_vault.amount)?.0,
            ctx.accounts.pool.reserves(ctx.accounts.vaults.base_vault.amount, ctx.accounts.vaults.quote_vault.amount)?.1,
            Clock::get()?.unix_timestamp,
        )?;
        ctx.accounts.sinks.check(&ctx.accounts.config)?;
        treasury_burn::business(&mut ctx.accounts.config, &mut ctx.accounts.sinks.treasury,
            &ctx.accounts.beta_mint.to_account_info(), &ctx.accounts.token_program.to_account_info(), ctx.remaining_accounts)?;
        require_sol_buy_before(
            &ctx.accounts.instructions.to_account_info(),
            &ctx.accounts.config.key(),
        )?;

        ctx.accounts.alpha_escrow.reload()?;
        let pot = ctx.accounts.config.sol_alpha_budget;
        require!(
            pot <= ctx.accounts.alpha_escrow.amount,
            DonateError::InsufficientEscrow
        );
        ctx.accounts.config.sol_alpha_budget = 0;
        require!(pot > 0, DonateError::InsufficientEscrow);

        let bps_operations = ctx.accounts.config.bps_operations;
        let bps_treasury = ctx.accounts.config.bps_treasury;
        let seed_key = ctx.accounts.config.config_seed;
        let bump = ctx.accounts.config.bump;
        let signer: &[&[&[u8]]] = &[&[b"donate-config", seed_key.as_ref(), &[bump]]];

        // ---- Distribute pot after normalizing by the non-ops weight (denom = 10000 − bps_operations = 9500) ----
        // (2026-09 redesign) Only two legs remain on the buy side: treasury is normalized by its own bps;
        // liquidity takes the remainder, absorbing integer rounding.
        let denom = (BPS_DENOM - bps_operations) as u128;
        require!(denom > 0, DonateError::MathOverflow);
        let treasury_amt =
            u64::try_from((pot as u128).saturating_mul(bps_treasury as u128) / denom)
                .map_err(|_| error!(DonateError::MathOverflow))?;
        let liq = pot.saturating_sub(treasury_amt);

        let slippage = ctx.accounts.config.max_slippage_bps;
        let pool_fee = ctx.accounts.pool.fee_bps;
        let beta_decimals = ctx.accounts.beta_mint.decimals;
        let lp_decimals = ctx.accounts.lp_mint.decimals;

        // --- Treasury share: LIKE→FLOCK into treasury ---
        if treasury_amt > 0 {
            ctx.accounts.vaults.base_vault.reload()?;
            ctx.accounts.vaults.quote_vault.reload()?;
            let (rb, rq) = (
                ctx.accounts.vaults.base_vault.amount,
                ctx.accounts.vaults.quote_vault.amount,
            );
            dex_sell_alpha_for_beta(
                ctx.accounts.dex_program.to_account_info(),
                ctx.accounts.config.to_account_info(),
                ctx.accounts.pool.to_account_info(),
                ctx.accounts.alpha_mint.to_account_info(),
                ctx.accounts.beta_mint.to_account_info(),
                ctx.accounts.vaults.base_vault.to_account_info(),
                ctx.accounts.vaults.quote_vault.to_account_info(),
                ctx.accounts.alpha_escrow.to_account_info(),
                ctx.accounts.sinks.treasury.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
                rb,
                rq,
                treasury_amt,
                slippage,
                pool_fee,
                signer,
                ctx.remaining_accounts,
            )?;
        }

        // Only treasury, liquidity and operations allocations remain.

        // --- 50% add liquidity → LP to the independent locker: half the Alpha buys Beta, the other half is paired with it and injected ---
        if liq > 0 {
            let s = liq / 2;
            ctx.accounts.config_beta_ata.reload()?;
            let beta_before = ctx.accounts.config_beta_ata.amount;
            if s > 0 {
                ctx.accounts.vaults.base_vault.reload()?;
                ctx.accounts.vaults.quote_vault.reload()?;
                let (rb, rq) = (
                    ctx.accounts.vaults.base_vault.amount,
                    ctx.accounts.vaults.quote_vault.amount,
                );
                dex_sell_alpha_for_beta(
                    ctx.accounts.dex_program.to_account_info(),
                    ctx.accounts.config.to_account_info(),
                    ctx.accounts.pool.to_account_info(),
                    ctx.accounts.alpha_mint.to_account_info(),
                    ctx.accounts.beta_mint.to_account_info(),
                    ctx.accounts.vaults.base_vault.to_account_info(),
                    ctx.accounts.vaults.quote_vault.to_account_info(),
                    ctx.accounts.alpha_escrow.to_account_info(),
                    ctx.accounts.config_beta_ata.to_account_info(),
                    ctx.accounts.token_program.to_account_info(),
                    rb,
                    rq,
                    s,
                    slippage,
                    pool_fee,
                    signer,
                ctx.remaining_accounts,
                )?;
            }
            ctx.accounts.config_beta_ata.reload()?;
            let beta_got = ctx
                .accounts
                .config_beta_ata
                .amount
                .saturating_sub(beta_before);
            let base_alpha = liq.saturating_sub(s);
            let mut base_added: u64 = 0; // Alpha actually paired into the pool by add_liquidity
            ctx.accounts.vaults.base_vault.reload()?;
            ctx.accounts.vaults.quote_vault.reload()?;
            let live_pool=venue::read(&ctx.accounts.pool.to_account_info())?;
            let (rb,rq)=live_pool.reserves(ctx.accounts.vaults.base_vault.amount,ctx.accounts.vaults.quote_vault.amount)?;
            let (rb,rq)=(rb as u128,rq as u128);
            if beta_got > 0 && base_alpha > 0 && rb > 0 && rq > 0 {
                // Balance the two legs at the current price ratio, avoiding the min() waste in dex add_liquidity.
                let base_by_beta = (beta_got as u128).saturating_mul(rb) / rq;
                let base_use = core::cmp::min(base_alpha as u128, base_by_beta) as u64;
                let quote_use =
                    core::cmp::min(beta_got as u128, (base_use as u128).saturating_mul(rq) / rb)
                        as u64;
                if base_use > 0 && quote_use > 0 {
                    let lp_supply = live_pool.total_lp as u128; // the pool's share ledger, not mint supply
                    let expected_lp = if lp_supply == 0 {
                        0
                    } else {
                        core::cmp::min(
                            (base_use as u128).saturating_mul(lp_supply) / rb,
                            (quote_use as u128).saturating_mul(lp_supply) / rq,
                        )
                    };
                    let min_lp_out =
                        u64::try_from(mul_bps(expected_lp, BPS_DENOM - slippage)).unwrap_or(0);
                    ctx.accounts.config_lp_ata.reload()?;
                    let lp_before = ctx.accounts.config_lp_ata.amount;
                    base_added = venue::add(ctx.accounts.dex_program.to_account_info(), dex_pool::cpi::accounts::AddLiquidity {
                                user: ctx.accounts.config.to_account_info(),
                                pool: ctx.accounts.pool.to_account_info(),
                                base_mint: ctx.accounts.alpha_mint.to_account_info(),
                                quote_mint: ctx.accounts.beta_mint.to_account_info(),
                                base_vault: ctx.accounts.vaults.base_vault.to_account_info(),
                                quote_vault: ctx.accounts.vaults.quote_vault.to_account_info(),
                                lp_mint: ctx.accounts.lp_mint.to_account_info(),
                                lp_lock: ctx.accounts.lp_lock.to_account_info(),
                                user_base: ctx.accounts.alpha_escrow.to_account_info(),
                                user_quote: ctx.accounts.config_beta_ata.to_account_info(),
                                user_lp: ctx.accounts.config_lp_ata.to_account_info(),
                                token_program: ctx.accounts.token_program.to_account_info(),
                            }, base_use, if live_pool.clmm.is_some() { beta_got } else { quote_use }, min_lp_out, signer, ctx.remaining_accounts)?; // CLMM rounds the required LIKER leg up; unused LIKER is swept below.
                    // Fixed venue destination: CPMM LP goes to the incinerator ATA;
                    // the legacy route retains its original Locker account.
                    // This transfer does not burn LP or change either supply ledger.
                    ctx.accounts.config_lp_ata.reload()?;
                    let lp_minted = ctx.accounts.config_lp_ata.amount.saturating_sub(lp_before);
                    if lp_minted > 0 && ctx.accounts.pool.clmm.is_none() {
                        transfer_checked(
                            CpiContext::new_with_signer(
                                ctx.accounts.token_program.to_account_info(),
                                TransferChecked {
                                    from: ctx.accounts.config_lp_ata.to_account_info(),
                                    mint: ctx.accounts.lp_mint.to_account_info(),
                                    to: ctx.accounts.locked_lp_vault.to_account_info(),
                                    authority: ctx.accounts.config.to_account_info(),
                                },
                                signer,
                            ),
                            lp_minted,
                            lp_decimals,
                        )?;
                    }
                }
            }

            // After adding liquidity, any leftover Beta in config_beta_ata not paired into the pool (buy leg overbought/rounding, or
            // add_liquidity skipped) is swept entirely into treasury, not left in the intermediary account.
            ctx.accounts.config_beta_ata.reload()?;
            let beta_left = ctx.accounts.config_beta_ata.amount;
            if beta_left > 0 {
                transfer_checked(
                    CpiContext::new_with_signer(
                        ctx.accounts.token_program.to_account_info(),
                        TransferChecked {
                            from: ctx.accounts.config_beta_ata.to_account_info(),
                            mint: ctx.accounts.beta_mint.to_account_info(),
                            to: ctx.accounts.sinks.treasury.to_account_info(),
                            authority: ctx.accounts.config.to_account_info(),
                        },
                        signer,
                    ),
                    beta_left,
                    beta_decimals,
                )?;
            }

            // Leftover Alpha (the part of the retained half not paired into the pool) is sent directly into the pool's base_vault, deepening the pool without minting LP.
            // Only "this leg's leftover" = base_alpha − already-added is transferred, never touching the Alpha reserved in alpha_escrow for the configured operations leg.
            let alpha_left = base_alpha.saturating_sub(base_added);
            if alpha_left > 0 {
                transfer_checked(
                    CpiContext::new_with_signer(
                        ctx.accounts.token_program.to_account_info(),
                        TransferChecked {
                            from: ctx.accounts.alpha_escrow.to_account_info(),
                            mint: ctx.accounts.alpha_mint.to_account_info(),
                            to: ctx.accounts.vaults.base_vault.to_account_info(),
                            authority: ctx.accounts.config.to_account_info(),
                        },
                        signer,
                    ),
                    alpha_left,
                    ctx.accounts.alpha_mint.decimals,
                )?;
            }
        }

        emit!(AllocateEvent {
            amount_in: pot,
            min_out: 0,
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Shared math utilities
// ---------------------------------------------------------------------------

/// Compute the USDT-e6 value of `amount` (smallest units) at `price` (USDT-e6, per whole token),
/// using the token's real `decimals` (not a hardcoded 9).
fn usdt_value(amount: u64, price: u64, decimals: u8) -> Result<u64> {
    require!(price > 0, DonateError::PriceUnset);
    let unit = 10u128.pow(decimals as u32);
    let v = (amount as u128).saturating_mul(price as u128) / unit;
    u64::try_from(v).map_err(|_| error!(DonateError::MathOverflow))
}

/// Derive the three prices (USDT-e6, per whole token) from raw on-chain DEX vault balances. This is the
/// "live DEX quote" required by docs/donate.md — not relying on the `set_price` oracle. Decimals assumptions: WSOL/LIKE/Beta=9dp, USDC=6dp.
///   sol_usd   = usdc_raw * 1e9 / wsol_usdc_raw            (USDC 6dp / WSOL 9dp)
///   alpha_usd = wsol_like_raw * sol_usd / like_raw        (both 9dp sides cancel)
///   beta_usd  = base_raw * alpha_usd / quote_raw          (both 9dp sides cancel)
fn dex_prices(
    usdc_raw: u128,
    wsol_usdc_raw: u128,
    wsol_like_raw: u128,
    like_raw: u128,
    base_raw: u128,
    quote_raw: u128,
) -> Result<(u64, u64, u64)> {
    require!(
        wsol_usdc_raw > 0 && like_raw > 0 && quote_raw > 0,
        DonateError::EmptyPool
    );
    let sol_usd = usdc_raw
        .checked_mul(1_000_000_000u128)
        .ok_or(error!(DonateError::MathOverflow))?
        / wsol_usdc_raw;
    let alpha_usd = wsol_like_raw
        .checked_mul(sol_usd)
        .ok_or(error!(DonateError::MathOverflow))?
        / like_raw;
    let beta_usd = base_raw
        .checked_mul(alpha_usd)
        .ok_or(error!(DonateError::MathOverflow))?
        / quote_raw;
    require!(
        sol_usd > 0 && alpha_usd > 0 && beta_usd > 0,
        DonateError::PriceUnset
    );
    Ok((
        u64::try_from(sol_usd).map_err(|_| error!(DonateError::MathOverflow))?,
        u64::try_from(alpha_usd).map_err(|_| error!(DonateError::MathOverflow))?,
        u64::try_from(beta_usd).map_err(|_| error!(DonateError::MathOverflow))?,
    ))
}

/// Derive SOL/USD only (USDT-e6) — used by `donate_sol` (alpha/beta not needed, so the other pool vaults are unnecessary).
fn dex_sol_usd(usdc_raw: u128, wsol_usdc_raw: u128) -> Result<u64> {
    require!(wsol_usdc_raw > 0, DonateError::EmptyPool);
    let sol_usd = usdc_raw
        .checked_mul(1_000_000_000u128)
        .ok_or(error!(DonateError::MathOverflow))?
        / wsol_usdc_raw;
    require!(sol_usd > 0, DonateError::PriceUnset);
    u64::try_from(sol_usd).map_err(|_| error!(DonateError::MathOverflow))
}

/// Compute `x × bps / 10000` in u128 (take a bps-based fraction).
fn mul_bps(x: u128, bps: u16) -> u128 {
    x.saturating_mul(bps as u128) / BPS_DENOM as u128
}

/// The shared pool's swap output for `amount_in` — mirrors dex-pool's constant-product-with-input-fee
/// math (`dex-pool/src/lib.rs` `swap`) including `pool.fee_bps`, so our on-chain fair quote matches what
/// the pool will actually pay. Quoting this fee-free would silently spend the `max_slippage_bps` budget on
/// the pool fee instead of on real price movement (at fee=50bps that is half of a 100bps budget), leaving
/// the swap to revert on any genuine move. Callers must ensure `reserve_in > 0`.
fn dex_quote_out(amount_in: u128, reserve_in: u128, reserve_out: u128, fee_bps: u16) -> u128 {
    let in_after_fee =
        amount_in.saturating_mul((BPS_DENOM.saturating_sub(fee_bps)) as u128) / BPS_DENOM as u128;
    reserve_out.saturating_mul(in_after_fee) / reserve_in.saturating_add(in_after_fee)
}

/// FL-01 claim pricing floor: when `beta_usd_anchor > 0 && claim_floor_tol_bps > 0`,
/// return `max(spot_beta_usd, anchor × (1 − tol))`; otherwise return `spot` unchanged (off by default → behavior unchanged).
/// Effect: even if a permissionless path depresses `config.beta_usd`, the claim's pricing won't fall below the anchor's tolerance floor —
/// blocking both "depress the valuation → deduct almost no power" (numerator) and "depress beta_usd → more claimable promotion/level FLOCK" (denominator).
fn beta_usd_with_floor(cfg: &DonateConfig, spot: u128) -> u128 {
    if cfg.beta_usd_anchor > 0 && cfg.claim_floor_tol_bps > 0 {
        let floor = (cfg.beta_usd_anchor as u128)
            .saturating_mul(BPS_DENOM.saturating_sub(cfg.claim_floor_tol_bps) as u128)
            / BPS_DENOM as u128;
        spot.max(floor)
    } else {
        spot
    }
}

/// Internal payout swaps use the same mandatory configured deviation check as
/// sampling and donation entry. Invalid/unset anchors cannot disable protection.
fn check_pool_deviation(cfg: &DonateConfig, base_reserve: u128, quote_reserve: u128) -> Result<()> {
    require!(quote_reserve > 0, DonateError::EmptyPool);
    let cur = base_reserve.checked_mul(PRICE_SCALE).ok_or(DonateError::MathOverflow)? / quote_reserve;
    require!(oracle::within(cur, cfg.pool_ratio_anchor, cfg.max_sync_deviation_bps), DonateError::PoolPriceDeviation);
    Ok(())
}

/// Parse the optional rolling-upgrade accounts appended by the dApp:
/// `[donating wallet (signer+writable), System Program]`.
///
/// No accounts means a cached/legacy frontend and preserves the previous close-to-ops
/// behavior. Any partial or malformed set is rejected rather than silently redirecting rent.
fn optional_rent_refund_accounts<'info>(
    remaining_accounts: &[AccountInfo<'info>],
) -> Result<Option<(AccountInfo<'info>, AccountInfo<'info>)>> {
    if remaining_accounts.is_empty() {
        return Ok(None);
    }
    require!(
        remaining_accounts.len() == 2,
        DonateError::InvalidRentRefundAccounts
    );
    let rent_recipient = remaining_accounts[0].clone();
    let system_program = remaining_accounts[1].clone();
    require!(
        rent_recipient.is_signer
            && rent_recipient.is_writable
            && rent_recipient.owner == &anchor_lang::system_program::ID,
        DonateError::InvalidRentRefundAccounts
    );
    require!(
        system_program.key() == anchor_lang::system_program::ID && system_program.executable,
        DonateError::InvalidRentRefundAccounts
    );
    Ok(Some((rent_recipient, system_program)))
}

/// A bundled allocation instruction may be the 20-account legacy form, or the
/// refund-aware form with the donor and System Program appended at indices 20/21.
/// If refund accounts are present, bind the recipient to the user of this donation.
fn refund_accounts_match_or_legacy(ix: &Instruction, donating_user: &Pubkey) -> bool {
    if ix.accounts.len() == RENT_REFUND_USER_ACCOUNT_INDEX {
        return true;
    }
    if ix.accounts.len() != RENT_REFUND_SYSTEM_ACCOUNT_INDEX + 1 {
        return false;
    }
    let Some(user) = ix.accounts.get(RENT_REFUND_USER_ACCOUNT_INDEX) else {
        return false;
    };
    let Some(system_program) = ix.accounts.get(RENT_REFUND_SYSTEM_ACCOUNT_INDEX) else {
        return false;
    };
    user.pubkey == *donating_user
        && user.is_signer
        && user.is_writable
        && system_program.pubkey == anchor_lang::system_program::ID
        && !system_program.is_signer
        && !system_program.is_writable
}

#[cfg(test)]
mod rent_refund_tests {
    use super::{
        refund_accounts_match_or_legacy, RENT_REFUND_SYSTEM_ACCOUNT_INDEX,
        RENT_REFUND_USER_ACCOUNT_INDEX,
    };
    use anchor_lang::prelude::Pubkey;
    use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};

    fn allocation_ix() -> Instruction {
        Instruction {
            program_id: Pubkey::new_unique(),
            accounts: (0..RENT_REFUND_USER_ACCOUNT_INDEX)
                .map(|_| AccountMeta::new_readonly(Pubkey::new_unique(), false))
                .collect(),
            data: vec![],
        }
    }

    #[test]
    fn legacy_twenty_account_form_remains_compatible() {
        let ix = allocation_ix();
        assert!(refund_accounts_match_or_legacy(&ix, &Pubkey::new_unique()));
    }

    #[test]
    fn refund_form_binds_writable_signing_donor_and_system_program() {
        let donor = Pubkey::new_unique();
        let mut ix = allocation_ix();
        ix.accounts.push(AccountMeta::new(donor, true));
        ix.accounts.push(AccountMeta::new_readonly(
            anchor_lang::system_program::ID,
            false,
        ));
        assert_eq!(ix.accounts.len(), RENT_REFUND_SYSTEM_ACCOUNT_INDEX + 1);
        assert!(refund_accounts_match_or_legacy(&ix, &donor));
        assert!(!refund_accounts_match_or_legacy(&ix, &Pubkey::new_unique()));
    }

    #[test]
    fn partial_or_malformed_refund_accounts_are_rejected() {
        let donor = Pubkey::new_unique();
        let mut partial = allocation_ix();
        partial.accounts.push(AccountMeta::new(donor, true));
        assert_eq!(partial.accounts.len(), RENT_REFUND_USER_ACCOUNT_INDEX + 1);
        assert!(!refund_accounts_match_or_legacy(&partial, &donor));

        let mut wrong_system = allocation_ix();
        wrong_system.accounts.push(AccountMeta::new(donor, true));
        wrong_system
            .accounts
            .push(AccountMeta::new_readonly(Pubkey::new_unique(), false));
        assert!(!refund_accounts_match_or_legacy(&wrong_system, &donor));
    }
}

/// Introspect the Instructions sysvar and assert this tx **atomically bundles** an
/// `allocate_operations_direct` instruction covering this configured operations leg: the program must be this program, `accounts[0]` must equal the same
/// `config` (config is fixed at index 0 in that instruction's account struct), and `amount_in >= required_ops`.
/// If satisfied, allow; otherwise `OpsLegMissing` reverts the whole donation.
///
/// Security: even if an attacker stuffs this config's pubkey into another slot to spoof `accounts[0]`, that
/// `allocate_operations_direct` would fail validation when it runs because the real config slot doesn't match → atomic revert;
/// and the destination (the operations wallet's Alpha ATA) is derived from `config.operations_wallet` inside that instruction, with the amount bounded by the escrow balance,
/// so it cannot be redirected or overspent. When `required_ops == 0` (ops bps=0) no ops leg is needed, so allow directly.
#[allow(deprecated)]
fn require_ops_leg_bundled(
    ix_sysvar: &AccountInfo,
    config_key: &Pubkey,
    required_ops: u64,
) -> Result<()> {
    use anchor_lang::solana_program::sysvar::instructions::load_current_index_checked;
    if required_ops == 0 {
        return Ok(());
    }
    // L-01: bind the ops leg to THIS donation's window — after this `donate_alpha` and before the
    // next `donate_alpha` acting on the same config — so two donations in one tx cannot both count
    // the same leg (rank-1 donation would then be left with no leg in its own window and revert).
    let cur = load_current_index_checked(ix_sysvar)? as usize;
    let hi = next_donate_index(
        ix_sysvar,
        cur,
        crate::instruction::DonateAlpha::DISCRIMINATOR,
        config_key,
    );
    let disc = crate::instruction::AllocateOperationsDirect::DISCRIMINATOR;
    // No donor binding: the direct leg funds no temporary ATA, so it has no rent to refund to anyone.
    match scan_bundled_ix_window(ix_sysvar, disc, config_key, required_ops, None, cur, hi) {
        Some(_) => Ok(()),
        None => err!(DonateError::OpsLegMissing),
    }
}

/// L-01 helper: index (exclusive upper bound) of the next same-kind donate (`donate_disc`) acting on
/// this `config_key` strictly after `cur`, or `usize::MAX` if none. Bounds a donation's allocation
/// window so each donation binds its own leg(s); a shared leg falls into only one donation's window,
/// leaving the earlier donation(s) with no leg → revert.
#[allow(deprecated)]
fn next_donate_index(
    ix_sysvar: &AccountInfo,
    cur: usize,
    donate_disc: &[u8],
    config_key: &Pubkey,
) -> usize {
    use anchor_lang::solana_program::sysvar::instructions::load_instruction_at_checked;
    let mut i = cur.saturating_add(1);
    while let Ok(ix) = load_instruction_at_checked(i, ix_sysvar) {
        if ix.program_id == crate::ID
            && ix.data.len() >= 8
            && &ix.data[0..8] == donate_disc
            && ix
                .accounts
                .get(1)
                .map(|m| m.pubkey == *config_key)
                .unwrap_or(false)
        {
            return i;
        }
        i += 1;
    }
    usize::MAX
}

/// Like `scan_bundled_ix` but restricted to the open index window `(lo, hi)`: returns the first match
/// at index `i` with `lo < i < hi`. Used for L-01 per-donation leg binding.
#[allow(deprecated)]
fn scan_bundled_ix_window(
    ix_sysvar: &AccountInfo,
    disc: &[u8],
    config_key: &Pubkey,
    min_amount: u64,
    donating_user: Option<&Pubkey>,
    lo: usize,
    hi: usize,
) -> Option<usize> {
    use anchor_lang::solana_program::sysvar::instructions::load_instruction_at_checked;
    let mut i = lo.saturating_add(1);
    while i < hi {
        let ix = match load_instruction_at_checked(i, ix_sysvar) {
            Ok(ix) => ix,
            Err(_) => break,
        };
        let disc_ok = ix.data.len() >= 8 && &ix.data[0..8] == disc;
        let config_ok = ix
            .accounts
            .first()
            .map(|m| m.pubkey == *config_key)
            .unwrap_or(false);
        let amount_ok = min_amount == 0
            || (ix.data.len() >= 16
                && u64::from_le_bytes(ix.data[8..16].try_into().unwrap()) >= min_amount);
        let refund_ok = donating_user
            .map(|user| refund_accounts_match_or_legacy(&ix, user))
            .unwrap_or(true);
        if ix.program_id == crate::ID && disc_ok && config_ok && amount_ok && refund_ok {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Scan this tx's instructions and return the index of the **first** match: program = this program, discriminator = `disc`, `accounts[0]`
/// = `config_key` (config is fixed at index 0 in that instruction group's account struct), and (when `min_amount > 0`) its
/// argument `amount >= min_amount` (LE u64, immediately after the 8-byte discriminator). Returns `None` if no match.
#[allow(deprecated)]
fn scan_bundled_ix(
    ix_sysvar: &AccountInfo,
    disc: &[u8],
    config_key: &Pubkey,
    min_amount: u64,
) -> Option<usize> {
    use anchor_lang::solana_program::sysvar::instructions::load_instruction_at_checked;
    let mut i: usize = 0;
    while let Ok(ix) = load_instruction_at_checked(i, ix_sysvar) {
        let disc_ok = ix.data.len() >= 8 && &ix.data[0..8] == disc;
        let config_ok = ix
            .accounts
            .first()
            .map(|m| m.pubkey == *config_key)
            .unwrap_or(false);
        let amount_ok = min_amount == 0
            || (ix.data.len() >= 16
                && u64::from_le_bytes(ix.data[8..16].try_into().unwrap()) >= min_amount);
        if ix.program_id == crate::ID && disc_ok && config_ok && amount_ok {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Introspect the Instructions sysvar and assert this SOL donation tx **atomically bundles** two legs: ① an `allocate_sol_buy`
/// with `amount >= required` (the donation amount, ensuring 97% is fully bought into Alpha + 3% native ops), ② an
/// `allocate_sol_distribute` (distributing the bought Alpha). Both act on the same `config` (config is fixed at
/// index 0 in each account struct). Missing either one triggers `SolDonationLegMissing`, reverting the whole donation.
///
/// Security is the same as `require_ops_leg_bundled`: even if an attacker stuffs this config's pubkey elsewhere to spoof `accounts[0]`,
/// the real buy/distribute legs would revert when run because the config slot doesn't match; the buy leg's spend is bounded by the available sol_escrow balance (see
/// `amount ≤ available` in `allocate_sol_buy`), so it cannot overspend or touch others' escrow.
#[allow(deprecated)]
fn require_sol_donation_bundled(
    ix_sysvar: &AccountInfo,
    config_key: &Pubkey,
    _donating_user: &Pubkey,
    required: u64,
) -> Result<()> {
    use anchor_lang::solana_program::sysvar::instructions::load_current_index_checked;
    // L-01: bind both legs to THIS donation's window (after this `donate_sol`, before the next
    // `donate_sol` on the same config) so multiple SOL donations cannot share one buy/distribute
    // leg (which would let some donations grant power while their SOL is swept to the ops wallet).
    let cur = load_current_index_checked(ix_sysvar)? as usize;
    let hi = next_donate_index(
        ix_sysvar,
        cur,
        crate::instruction::DonateSol::DISCRIMINATOR,
        config_key,
    );
    let buy_disc = crate::instruction::AllocateSolBuy::DISCRIMINATOR;
    let dist_disc = crate::instruction::AllocateSolDistribute::DISCRIMINATOR;
    require!(
        scan_bundled_ix_window(ix_sysvar, buy_disc, config_key, required, None, cur, hi,).is_some(),
        DonateError::SolDonationLegMissing
    );
    require!(
        scan_bundled_ix_window(ix_sysvar, dist_disc, config_key, 0, None, cur, hi).is_some(),
        DonateError::SolDonationLegMissing
    );
    Ok(())
}

/// For `allocate_sol_distribute`: assert there is an **earlier** `allocate_sol_buy` acting on the same `config`
/// in this tx — ensuring that when the distribution leg runs, `alpha_escrow` has already been funded with Alpha by the buy leg (otherwise it would distribute 0 and leave
/// the Alpha bought by a later buy leg stranded in escrow). The first buy leg's index must be < this instruction's own index.
#[allow(deprecated)]
fn require_sol_buy_before(ix_sysvar: &AccountInfo, config_key: &Pubkey) -> Result<()> {
    use anchor_lang::solana_program::sysvar::instructions::load_current_index_checked;
    let cur = load_current_index_checked(ix_sysvar)? as usize;
    let buy_disc = crate::instruction::AllocateSolBuy::DISCRIMINATOR;
    match scan_bundled_ix(ix_sysvar, buy_disc, config_key, 0) {
        Some(idx) if idx < cur => Ok(()),
        _ => err!(DonateError::SolDonationLegMissing),
    }
}

/// Introspect the Instructions sysvar and assert that a power-changing `UserStake.power`
/// instruction is top-level and is followed immediately by `sync_level` for the same
/// `(config, user_stake)`. Immediate adjacency prevents a second claim/donation from
/// consuming stale referral eligibility before the first change has propagated.
#[allow(deprecated)]
fn require_sync_level_after(
    ix_sysvar: &AccountInfo,
    config_key: &Pubkey,
    user_stake_key: &Pubkey,
) -> Result<()> {
    use anchor_lang::solana_program::instruction::{
        get_stack_height, TRANSACTION_LEVEL_STACK_HEIGHT,
    };
    use anchor_lang::solana_program::sysvar::instructions::{
        load_current_index_checked, load_instruction_at_checked,
    };
    require!(
        get_stack_height() == TRANSACTION_LEVEL_STACK_HEIGHT,
        DonateError::PowerChangeCpiForbidden
    );
    let cur = load_current_index_checked(ix_sysvar)? as usize;
    let disc = crate::instruction::SyncLevel::DISCRIMINATOR;
    let ix = load_instruction_at_checked(cur.saturating_add(1), ix_sysvar)
        .map_err(|_| error!(DonateError::SyncLevelMissing))?;
    require!(
        ix.program_id == crate::ID
            && ix.data.len() >= 8
            && &ix.data[0..8] == disc
            && ix.accounts.get(0).map(|m| m.pubkey == *config_key).unwrap_or(false)
            && ix.accounts.get(1).map(|m| m.pubkey == *user_stake_key).unwrap_or(false),
        DonateError::SyncLevelMissing
    );
    Ok(())
}

/// Raw CPI for Raydium AMM v4 `swap_base_in` (tag=9). `accounts` is 17 accounts, ordered per the file-top constant
/// comments (**excluding amm_target_orders**, verified against a real on-chain swap); `accounts[16]` = user_owner
/// (= the config PDA, signed via seeds provided by `signer`). The direction is inferred from whether user_source is coin/pc.
fn raydium_swap_base_in<'info>(
    raydium_program: AccountInfo<'info>,
    accounts: [AccountInfo<'info>; 17],
    amount_in: u64,
    min_out: u64,
    signer: &[&[&[u8]]],
) -> Result<()> {
    let metas = vec![
        AccountMeta::new_readonly(*accounts[0].key, false), // spl token program
        AccountMeta::new(*accounts[1].key, false),          // amm
        AccountMeta::new_readonly(*accounts[2].key, false), // amm authority
        AccountMeta::new(*accounts[3].key, false),          // amm open_orders
        AccountMeta::new(*accounts[4].key, false),          // amm coin vault
        AccountMeta::new(*accounts[5].key, false),          // amm pc vault
        AccountMeta::new_readonly(*accounts[6].key, false), // serum program
        AccountMeta::new(*accounts[7].key, false),          // serum market
        AccountMeta::new(*accounts[8].key, false),          // serum bids
        AccountMeta::new(*accounts[9].key, false),          // serum asks
        AccountMeta::new(*accounts[10].key, false),         // serum event queue
        AccountMeta::new(*accounts[11].key, false),         // serum coin vault
        AccountMeta::new(*accounts[12].key, false),         // serum pc vault
        AccountMeta::new_readonly(*accounts[13].key, false), // serum vault signer
        AccountMeta::new(*accounts[14].key, false),         // user source
        AccountMeta::new(*accounts[15].key, false),         // user dest
        AccountMeta::new_readonly(*accounts[16].key, true), // user owner (signer)
    ];
    let mut data = Vec::with_capacity(17);
    data.push(9u8); // swap_base_in
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_out.to_le_bytes());
    let ix = Instruction {
        program_id: *raydium_program.key,
        accounts: metas,
        data,
    };
    let mut infos = accounts.to_vec();
    infos.push(raydium_program);
    invoke_signed(&ix, &infos, signer).map_err(Into::into)
}

/// Sell `amount_in` Alpha (base) for Beta (quote) via the shared dex pool into `user_quote` (side=1).
/// `min_out` is computed on-chain from the passed-in **fresh reserves** at the fair price ×(1−slippage) — the caller has no slippage freedom.
/// `user` (= the config PDA) signs for the dex (it is already the pool operator, so the buy/sell switch does not block it). The caller must, before each call,
/// reload the base/quote vaults to get the latest reserves.
#[allow(clippy::too_many_arguments)]
fn dex_sell_alpha_for_beta<'info>(
    dex_program: AccountInfo<'info>,
    config_ai: AccountInfo<'info>,
    pool: AccountInfo<'info>,
    base_mint: AccountInfo<'info>,
    quote_mint: AccountInfo<'info>,
    base_vault: AccountInfo<'info>,
    quote_vault: AccountInfo<'info>,
    user_base: AccountInfo<'info>,
    user_quote: AccountInfo<'info>,
    token_program: AccountInfo<'info>,
    reserve_base: u64,
    reserve_quote: u64,
    amount_in: u64,
    slippage_bps: u16,
    pool_fee_bps: u16,
    signer: &[&[&[u8]]],
    remaining: &[AccountInfo<'info>],
) -> Result<()> {
    require!(amount_in > 0, DonateError::ZeroAmount);
    let view=venue::read(&pool)?;
    let (ri,ro)=view.reserves(reserve_base,reserve_quote)?;
    let quote_out=if view.cpmm.is_some() || view.clmm.is_some(){venue::quote(amount_in,ri,ro,view.fee(remaining)?)? as u128}else{dex_quote_out(amount_in as u128,ri as u128,dex_quote_reserve_after_burn(&pool,ro)? as u128,pool_fee_bps)};
    let min_out = u64::try_from(mul_bps(quote_out, BPS_DENOM - slippage_bps))
        .map_err(|_| error!(DonateError::MathOverflow))?;
    venue::swap(dex_program,config_ai,pool,base_mint,quote_mint,base_vault,quote_vault,user_base,user_quote,token_program,amount_in,min_out,1,signer,remaining)?;
    Ok(())
}

/// Saturating implementation of `(base + delta).max(0)`: used when team/direct-referral power changes by Δ (positive for donate, negative for claim).
fn saturating_add_i128(base: u128, delta: i128) -> u128 {
    let r = (base as i128).saturating_add(delta);
    if r < 0 {
        0
    } else {
        r as u128
    }
}

/// Validate and deserialize an "upline" `remaining_account`: it must be **writable**, owned by this program, not duplicated in `seen`,
/// with owner exactly equal to `expected_parent` (bound to the immutable `referrer`), and at the **canonical PDA**. Returns the deserialized
/// `UserStake`; after modifying it the caller must `anc.exit(&crate::ID)` to write it back manually (these uplines are not in the Accounts struct,
/// so Anchor won't auto-serialize). `sync_level` (propagating the power Δ upward) and `claim` (promotion-reward accounting) share this function, ensuring
/// a single source of truth for chain-safety checks — the caller can only pass real uplines and cannot inflate.
fn load_checked_ancestor<'info>(
    ai: &'info AccountInfo<'info>,
    expected_parent: Pubkey,
    config_key: Pubkey,
    seen: &mut Vec<Pubkey>,
) -> Result<Account<'info, UserStake>> {
    require!(ai.is_writable, DonateError::BadChain);
    require_keys_eq!(*ai.owner, crate::ID, DonateError::BadChain);
    require!(!seen.contains(&ai.key()), DonateError::BadChain);
    seen.push(ai.key());
    let anc = Account::<UserStake>::try_from(ai)?;
    require_keys_eq!(anc.owner, expected_parent, DonateError::BadChain);
    let (pda, _b) = Pubkey::find_program_address(
        &[b"donate-user", config_key.as_ref(), anc.owner.as_ref()],
        &crate::ID,
    );
    require_keys_eq!(pda, ai.key(), DonateError::BadChain);
    Ok(anc)
}

fn validate_levels(
    min_power: &[u64; LEVEL_TIERS],
    min_team: &[u64; LEVEL_TIERS],
    min_small: &[u64; LEVEL_TIERS],
    min_star: &[u8; LEVEL_TIERS],
) -> Result<()> {
    for i in 0..LEVEL_TIERS {
        if (i as u8) + 1 <= STAR_COUNT_LEVEL {
            require!(min_star[i] == 0, DonateError::BadRatios);
        }
        if i > 0 {
            require!(
                min_power[i] >= min_power[i - 1]
                    && min_team[i] >= min_team[i - 1]
                    && min_small[i] >= min_small[i - 1]
                    && min_star[i] >= min_star[i - 1],
                DonateError::BadRatios
            );
        }
    }
    Ok(())
}

/// Stack-local copy of the level threshold tables. Snapshotting them once per instruction
/// releases the immutable borrow of `DonateConfig`, so levels can still be recomputed while
/// `&mut DonateConfig` is held for V-pool bucket maintenance.
#[derive(Clone, Copy)]
pub struct LevelThresholds {
    pub min_power: [u64; LEVEL_TIERS],
    pub min_direct: [u64; LEVEL_TIERS],
    pub min_team: [u64; LEVEL_TIERS],
    pub min_small: [u64; LEVEL_TIERS],
    pub min_star: [u8; LEVEL_TIERS],
}

impl LevelThresholds {
    fn of(cfg: &DonateConfig) -> Self {
        Self {
            min_power: cfg.level_min_power,
            min_direct: [0; LEVEL_TIERS],
            min_team: cfg.level_min_team,
            min_small: cfg.level_min_small,
            min_star: cfg.level_min_star,
        }
    }
}

/// Compute the level V1..V8 from the four power figures (personal / direct / team / small area) plus the
/// star count: thresholds are monotonically non-decreasing (enforced by `set_levels`), scanned high
/// to low, and the first tier that satisfies "all five met (AND)" is the level; none ⇒ 0 (below V1).
fn compute_level_from(
    power: u128,
    direct: u128,
    team: u128,
    small: u128,
    direct_ge6: u32,
    th: &LevelThresholds,
) -> u8 {
    // 2026-09 redesign: the direct-referral-performance (`min_direct`) requirement for leveling up is
    // removed (级别升级取消直推业绩要求). The `direct` argument + `min_direct` threshold field are kept in
    // place (as always-satisfied) to avoid a config-layout / signature churn, but no longer gate a level.
    let _ = direct;
    for i in (0..LEVEL_TIERS).rev() {
        if power >= th.min_power[i] as u128
            && team >= th.min_team[i] as u128
            && small >= th.min_small[i] as u128
            && direct_ge6 >= th.min_star[i] as u32
        {
            return (i as u8) + 1;
        }
    }
    0
}

/// Small area = team performance excluding the single largest direct leg.
/// Saturating on purpose: `max_leg` is maintained raise-only between `refresh_max_leg` runs, so it
/// can legitimately exceed a shrunken `team_power`; that must read as 0, never wrap.
fn small_area_of(us: &UserStake) -> u128 {
    us.team_power.saturating_sub(us.max_leg as u128)
}

/// The level this account currently qualifies for, from its own stored figures.
fn level_of(us: &UserStake, th: &LevelThresholds) -> u8 {
    compute_level_from(
        us.power,
        us.direct_power,
        us.team_power,
        small_area_of(us),
        us.direct_ge6,
        th,
    )
}

/// Does a move from `old_level` to `new_level` cross the `STAR_COUNT_LEVEL` boundary, and in which
/// direction? The result is what the direct upline's `direct_ge6` counter must absorb.
fn star_flip(old_level: u8, new_level: u8) -> i32 {
    match (old_level >= STAR_COUNT_LEVEL, new_level >= STAR_COUNT_LEVEL) {
        (false, true) => 1,
        (true, false) => -1,
        _ => 0,
    }
}

/// Saturating `(base + delta).max(0)` in u64 — the `leg_power` counterpart of `saturating_add_i128`.
fn add_delta_u64(base: u64, delta: i128) -> u64 {
    let r = (base as i128).saturating_add(delta);
    if r < 0 {
        0
    } else {
        u64::try_from(r).unwrap_or(u64::MAX)
    }
}

/// level → index into the per-tier V-pool arrays; `None` for anyone not enrolled.
fn v_idx(tier: u8) -> Option<usize> {
    if tier >= V_TIER_MIN_LEVEL && ((tier - V_TIER_MIN_LEVEL) as usize) < V_TIERS {
        Some((tier - V_TIER_MIN_LEVEL) as usize)
    } else {
        None
    }
}

fn v_tier_of(level: u8) -> u8 {
    if v_idx(level).is_some() {
        level
    } else {
        0
    }
}

/// Immutable per-instruction snapshot of the V-pool accumulators, taken once right after
/// `accrue_v`. Same reason as `LevelThresholds`: it frees the borrow of config.
#[derive(Clone, Copy)]
struct VSnap {
    acc: [u128; V_TIERS],
}

impl VSnap {
    fn of(cfg: &DonateConfig) -> Self {
        Self { acc: cfg.acc_per_v }
    }
}

fn level_reward_bps(cfg: &DonateConfig) -> u16 {
    // The old oracle interval slot was always zero and is no longer read by the oracle.
    // Retain the exact DonateConfig layout for already initialized mainnet accounts.
    match cfg.min_sync_interval_secs {
        -1 => 0,
        1..=10_000 => cfg.min_sync_interval_secs as u16,
        _ => DEFAULT_LEVEL_REWARD_BPS,
    }
}

fn accrue_v(cfg: &mut DonateConfig, treasury_beta: u64, now: i64) {
    if now <= cfg.v_last_update_ts {
        return;
    }
    if cfg.migration_phase != migration::ACTIVE || !cfg.v_dividend_enabled {
        cfg.v_last_update_ts = now;
        return;
    }
    let elapsed = (now - cfg.v_last_update_ts) as u128;
    let total_weight: u128 = cfg.level_weights.iter().map(|v| *v as u128).sum();
    if total_weight > 0 {
        // Keep sub-token precision until the per-head accumulator. Unallocated shares never carry.
        let denominator = BPS_DENOM as u128 * SECONDS_PER_DAY as u128;
        let numerator = (treasury_beta as u128)
            .saturating_mul(level_reward_bps(cfg) as u128)
            .saturating_mul(elapsed);
        let budget_scaled = (numerator / denominator)
            .saturating_mul(ACC_SCALE)
            .saturating_add((numerator % denominator) * ACC_SCALE / denominator);
        for i in 0..V_TIERS {
            let heads = cfg.total_v_weight[i];
            if heads == 0 {
                continue;
            }
            let tier_scaled =
                budget_scaled.saturating_mul(cfg.level_weights[i] as u128) / total_weight;
            let per_head = (tier_scaled / heads).min(mul_bps(tier_scaled, cfg.level_share_cap_bps));
            cfg.acc_per_v[i] = cfg.acc_per_v[i].saturating_add(per_head);
        }
    }
    cfg.v_last_update_ts = now;
}

fn settle_v(us: &mut UserStake, snap: &VSnap) {
    if let Some(i) = v_idx(us.v_tier) {
        let delta = snap.acc[i].saturating_sub(us.v_reward_debt);
        let scaled = delta.saturating_add(us.level_reward_remainder);
        us.v_pending = us.v_pending.saturating_add(scaled / ACC_SCALE);
        us.level_reward_remainder = scaled % ACC_SCALE;
        us.v_reward_debt = snap.acc[i];
    }
}

fn v_unbucket(cfg: &mut DonateConfig, us: &mut UserStake, snap: &VSnap) {
    settle_v(us, snap);
    if let Some(i) = v_idx(us.v_tier) {
        cfg.total_v_weight[i] = cfg.total_v_weight[i].saturating_sub(1);
    }
    us.v_tier = 0;
}

fn v_rebucket(cfg: &mut DonateConfig, us: &mut UserStake, snap: &VSnap, level: u8) {
    us.level = level;
    us.v_tier = v_tier_of(level);
    if let Some(i) = v_idx(us.v_tier) {
        cfg.total_v_weight[i] = cfg.total_v_weight[i].saturating_add(1);
        us.v_reward_debt = snap.acc[i];
    } else {
        us.v_reward_debt = 0;
    }
}

fn accrue_global(cfg: &mut DonateConfig, treasury_beta: u64, now: i64) {
    if now <= cfg.last_update_ts {
        return;
    }
    if cfg.migration_phase == migration::ACTIVE && cfg.total_power > 0 {
        let elapsed = (now - cfg.last_update_ts) as u128;
        let denominator = BPS_DENOM as u128 * cfg.day_window_secs.max(1) as u128;
        let numerator = (treasury_beta as u128)
            .saturating_mul(cfg.daily_emission_bps as u128)
            .saturating_mul(elapsed)
            .saturating_add(cfg.emission_remainder);
        let reward = numerator / denominator;
        cfg.emission_remainder = numerator % denominator;
        let scaled = reward
            .saturating_mul(ACC_SCALE)
            .saturating_add(cfg.reward_remainder);
        cfg.acc_reward_per_power = cfg
            .acc_reward_per_power
            .saturating_add(scaled / cfg.total_power);
        cfg.reward_remainder = scaled % cfg.total_power;
    }
    cfg.last_update_ts = now;
}

/// Record the newly accrued Beta into the user's `pending` and re-anchor their `reward_debt` (the settled checkpoint).
/// This is the core of the MasterChef accumulator: the user's "total entitlement" = power × acc / ACC_SCALE,
/// minus the reward_debt recorded at the last settlement; the difference is the new accrual, added to pending.
fn settle(us: &mut UserStake, acc: u128) {
    let accrued = us.power.saturating_mul(acc) / ACC_SCALE;
    let credit = accrued.saturating_sub(us.reward_debt);
    us.pending = us.pending.saturating_add(credit);
    us.reward_debt = accrued;
}

/// Before new power is granted, retain old pending in static → promotion → level order.
fn forfeit_pending_over_power(us: &mut UserStake, beta_usd: u128, beta_unit: u128) -> u128 {
    // Price unset → pending cannot be valued. Leave state untouched; claim reverts on this anyway
    // (`DonateError::PriceUnset`), so nothing is claimable in the meantime.
    if beta_usd == 0 {
        return 0;
    }
    // Remaining budget expressed in Beta, matching how claim converts power→beta (`power_as_beta`).
    let budget_beta = us.power.saturating_mul(beta_unit) / beta_usd;
    let before = us
        .pending
        .saturating_add(us.promo_pending)
        .saturating_add(us.v_pending);
    if before <= budget_beta {
        return 0; // not saturated — the ordinary path, nothing to drop
    }
    let mut left = budget_beta;
    us.pending = us.pending.min(left);
    left = left.saturating_sub(us.pending);
    us.promo_pending = us.promo_pending.min(left);
    left = left.saturating_sub(us.promo_pending);
    us.v_pending = us.v_pending.min(left);
    before.saturating_sub(budget_beta)
}

#[cfg(test)]
mod donate_forfeit_tests {
    //! `forfeit_pending_over_power`: a donation must not resurrect accrual that had already overflowed the
    //! pre-donation release budget. See the fn's doc for why claim-time forfeit alone leaves the hole open.
    use super::{forfeit_pending_over_power, UserStake, TOKEN_UNIT};

    /// UserStake is all scalars / Pubkeys / fixed-length arrays (no heap fields), so a zeroed value is valid.
    fn stake(power: u128, pending: u128, promo: u128, level: u128) -> UserStake {
        let mut us: UserStake = unsafe { std::mem::zeroed() };
        us.power = power;
        us.pending = pending;
        us.promo_pending = promo;
        us.v_pending = level;
        us
    }

    const BETA_USD: u128 = 60_900; // $0.0609 per whole Beta (USDT-e6), mainnet oracle on 2026-07-17

    #[test]
    fn not_saturated_is_untouched() {
        // The ordinary path: pending well inside the budget → nothing dropped, no state change.
        let mut us = stake(1_000_000_000, 5_000_000_000, 0, 0); // power $1000, pending 5 Beta ≈ $0.30
        assert_eq!(forfeit_pending_over_power(&mut us, BETA_USD, TOKEN_UNIT), 0);
        assert_eq!(us.pending, 5_000_000_000);
    }

    #[test]
    fn axykln_real_scenario_drops_the_overflow() {
        // Real mainnet state of AXYkLNKp… at 2026-07-17T06:43 (donation #3), the case that exposed this:
        //   power = $307.143538 (from ClaimEvent.power_left of the 07-16 claim)
        //   pending = 21_078.15 Beta ≈ $1283.7 → overflows the budget ~4.2×
        // Budget in Beta = 307.143538e6 * 1e9 / 60_900 ≈ 5043.4 Beta; the rest could never be claimed.
        let mut us = stake(307_143_538, 21_078_154_200_000, 0, 0);
        let dropped = forfeit_pending_over_power(&mut us, BETA_USD, TOKEN_UNIT);
        let budget = 307_143_538u128 * TOKEN_UNIT / BETA_USD;
        assert_eq!(us.pending, budget); // clamped to exactly the remaining budget
        assert_eq!(dropped, 21_078_154_200_000 - budget); // ≈ 16_034.8 Beta ≈ $977 zeroed
                                                          // Without this, donation #3 (+$975 power) would have made the whole $1284 claimable.
        assert!(dropped > 16_000_000_000_000 && dropped < 16_100_000_000_000);
    }

    #[test]
    fn fills_buckets_in_claim_order_static_promo_level() {
        // Budget covers static + part of promo → level is fully dropped, promo partially, static intact.
        let budget_beta = 1_000_000_000_000u128; // 1000 Beta
        let power = budget_beta * BETA_USD / TOKEN_UNIT; // value that yields exactly this budget
        let mut us = stake(power, 600_000_000_000, 900_000_000_000, 500_000_000_000);
        forfeit_pending_over_power(&mut us, BETA_USD, TOKEN_UNIT);
        assert_eq!(us.pending, 600_000_000_000); // static untouched (consumed first)
        assert_eq!(us.promo_pending, 400_000_000_000); // promo gets the remaining 400
        assert_eq!(us.v_pending, 0); // level dropped entirely (consumed last)
        assert_eq!(us.pending + us.promo_pending + us.v_pending, budget_beta);
    }

    #[test]
    fn zero_power_drops_everything() {
        // power exhausted → nothing is claimable; a donation must start from a clean slate.
        let mut us = stake(0, 9_000_000_000, 8_000_000_000, 7_000_000_000);
        assert_eq!(
            forfeit_pending_over_power(&mut us, BETA_USD, TOKEN_UNIT),
            24_000_000_000
        );
        assert_eq!((us.pending, us.promo_pending, us.v_pending), (0, 0, 0));
    }

    #[test]
    fn price_unset_leaves_state_untouched() {
        // beta_usd == 0 → pending cannot be valued. Must not zero anything (claim reverts on PriceUnset anyway).
        let mut us = stake(0, 9_000_000_000, 0, 0);
        assert_eq!(forfeit_pending_over_power(&mut us, 0, TOKEN_UNIT), 0);
        assert_eq!(us.pending, 9_000_000_000);
    }
}

#[cfg(test)]
mod emission_sizing_tests {
    //! Static emission uses the live treasury FLOCK balance at settlement time.
    use super::{accrue_global, DonateConfig, ACC_SCALE};

    /// DonateConfig contains only scalars / Pubkeys / fixed-length arrays (no heap fields), so a zeroed construction is valid;
    /// only fill the fields accrue_global actually reads.
    fn cfg(total_power: u128) -> DonateConfig {
        let mut c: DonateConfig = unsafe { core::mem::zeroed() };
        c.total_power = total_power;
        c.migration_phase = crate::migration::ACTIVE;
        c.daily_emission_bps = 100; // 1%/day
        c.day_window_secs = 86_400;
        c.last_update_ts = 0;
        c
    }

    #[test]
    fn treasury_balance_is_the_emission_base() {
        let mut c = cfg(1);
        accrue_global(&mut c, 4_000_000_000, 86_400);
        assert_eq!(c.acc_reward_per_power, 40_000_000 * ACC_SCALE);
        assert_eq!(c.last_update_ts, 86_400);
    }

    #[test]
    fn lower_treasury_reduces_emission() {
        let mut c = cfg(1);
        accrue_global(&mut c, 1_000_000_000, 86_400);
        assert_eq!(c.acc_reward_per_power, 10_000_000 * ACC_SCALE);
    }

    #[test]
    fn emission_is_time_prorated() {
        // half a day → base × 1% × 1/2.
        let mut c = cfg(1);
        accrue_global(&mut c, 4_000_000_000, 43_200);
        assert_eq!(c.acc_reward_per_power, 20_000_000 * ACC_SCALE);
    }

    #[test]
    fn empty_treasury_advances_time_without_backpay() {
        let mut c = cfg(1);
        accrue_global(&mut c, 0, 86_400);
        assert_eq!(c.acc_reward_per_power, 0);
        assert_eq!(c.last_update_ts, 86_400);
        accrue_global(&mut c, 1_000_000_000, 172_800);
        assert_eq!(c.acc_reward_per_power, 10_000_000 * ACC_SCALE);
    }

    #[test]
    fn zero_power_advances_ts_without_emitting() {
        // no emission when there is no power, but the timestamp still advances (no arrears accrue during idle periods).
        let mut c = cfg(0);
        accrue_global(&mut c, 1_000_000_000, 86_400);
        assert_eq!(c.acc_reward_per_power, 0);
        assert_eq!(c.last_update_ts, 86_400);
    }
}

/// The dex pool's "effective open price for the day": valid only when `open_day` belongs to the current UTC day; if a day rolls over with no trade
/// (`open_day` lags today, i.e. no swap has refreshed the open price since UTC midnight), it is treated as "not recorded for the day"
/// and returns 0, so `settle_claim_payout`'s `if open_price > 0` sees empty → `drop_bps = 0` → uses the base fee.
/// Avoids a claim before the first swap after UTC midnight carrying over yesterday's drop and inflating the fee tier (consistent with dex-pool swap's daily-rollover semantics).
fn effective_open_price(pool: &dex_pool::Pool, now: i64) -> u128 {
    let day = (now / pool.day_window_secs.max(1)) as u32;
    if pool.open_day == day {
        pool.open_price
    } else {
        0
    }
}

#[allow(clippy::too_many_arguments)]
fn settle_claim_payout<'info>(
    cfg: &mut DonateConfig,
    alpha_escrow: &mut InterfaceAccount<'info, TokenAccount>,
    token_program: &AccountInfo<'info>,
    config_ai: &AccountInfo<'info>,
    treasury: &AccountInfo<'info>,
    beta_mint: &AccountInfo<'info>,
    operations_beta: &AccountInfo<'info>,
    base_mint: &AccountInfo<'info>,
    base_vault: &AccountInfo<'info>,
    quote_vault: &AccountInfo<'info>,
    pool: &AccountInfo<'info>,
    user_alpha: &AccountInfo<'info>,
    dex_program: &AccountInfo<'info>,
    take_beta: u128,
    base_reserve: u128,
    quote_reserve: u128,
    open_price: u128,
    pool_fee_bps: u16,
    base_decimals: u8,
    beta_decimals: u8,
    remaining: &[AccountInfo<'info>],
) -> Result<(u64, u128)> {
    // FL-03: at entry, verify the self-built pool price remains within the configured deviation from its observation anchor. If the program's own-fund auto-sell leg fills at a manipulated
    // price, value leaks — exceeding the threshold reverts the whole tx (an instantaneous DoS is preferable to being arbitraged).
    check_pool_deviation(cfg, base_reserve, quote_reserve)?;
    // Fee tier: the dex pool's "open price for the day" vs the current price (base/quote = Alpha per Beta); the more it drops, the higher the fee.
    let cur_price = base_reserve.saturating_mul(PRICE_SCALE) / quote_reserve;
    let drop_bps: u128 = if open_price > 0 && cur_price < open_price {
        (open_price - cur_price).saturating_mul(BPS_DENOM as u128) / open_price
    } else {
        0
    };
    let mut fee_bps = cfg.claim_fee_base_bps as u128;
    for i in 0..4 {
        if drop_bps >= cfg.claim_fee_drop_bps[i] as u128 {
            fee_bps = cfg.claim_fee_tier_bps[i] as u128;
        }
    }
    let fee_beta = take_beta.saturating_mul(fee_bps) / BPS_DENOM as u128;
    let net_beta = take_beta.saturating_sub(fee_beta);
    let operations_beta_amount =
        fee_beta.saturating_mul(cfg.claim_fee_operations_bps as u128) / BPS_DENOM as u128;
    let mut bh_beta =
        fee_beta.saturating_mul(cfg.claim_fee_blackhole_bps as u128) / BPS_DENOM as u128;
    // When the pool's Beta ≤ blackhole_floor (whole Beta tokens, converted via ×TOKEN_UNIT), stop burning to the blackhole (that portion stays in treasury).
    if quote_reserve <= (cfg.blackhole_floor as u128).saturating_mul(TOKEN_UNIT) {
        bh_beta = 0;
    }
    let operations_beta_amount_u64 =
        u64::try_from(operations_beta_amount).map_err(|_| DonateError::MathOverflow)?;
    let bh_beta_u64 = u64::try_from(bh_beta).map_err(|_| DonateError::MathOverflow)?;
    let net_beta_u64 = u64::try_from(net_beta).map_err(|_| DonateError::MathOverflow)?;

    // The config PDA is the authority of treasury / alpha_escrow.
    let seed_key = cfg.config_seed;
    let bump = cfg.bump;
    let signer: &[&[&[u8]]] = &[&[b"donate-config", seed_key.as_ref(), &[bump]]];

    // Operations fee is paid in FLOCK; the burn share is destroyed from treasury; the remainder stays in treasury.
    if operations_beta_amount_u64 > 0 {
        transfer_checked(
            CpiContext::new_with_signer(
                token_program.clone(),
                TransferChecked {
                    from: treasury.clone(),
                    mint: beta_mint.clone(),
                    to: operations_beta.clone(),
                    authority: config_ai.clone(),
                },
                signer,
            ),
            operations_beta_amount_u64,
            beta_decimals,
        )?;
    }
    if bh_beta_u64 > 0 {
        burn(
            CpiContext::new_with_signer(
                token_program.clone(),
                Burn {
                    mint: beta_mint.clone(),
                    from: treasury.clone(),
                    authority: config_ai.clone(),
                },
                signer,
            ),
            bh_beta_u64,
        )?;
        cfg.total_burned = cfg.total_burned.saturating_add(bh_beta);
    }

    // Auto-sell: net Beta (treasury) → Alpha (first into alpha_escrow as an intermediary), side=0 buy (quote→base);
    // compute min_out on-chain (reserve_in=Beta, reserve_out=Alpha).
    let mut alpha_out: u64 = 0;
    if net_beta_u64 > 0 {
        let swap_quote_reserve = dex_quote_reserve_after_burn(
            pool,
            u64::try_from(quote_reserve).map_err(|_| DonateError::MathOverflow)?,
        )? as u128;
        let view=venue::read(pool)?;
        let quote_out = if view.cpmm.is_some() || view.clmm.is_some(){venue::quote(net_beta_u64,quote_reserve as u64,base_reserve as u64,view.fee(remaining)?)? as u128}else{dex_quote_out(net_beta,swap_quote_reserve,base_reserve,pool_fee_bps)};
        let min_out = u64::try_from(
            quote_out.saturating_mul((BPS_DENOM - cfg.max_slippage_bps) as u128)
                / BPS_DENOM as u128,
        )
        .map_err(|_| DonateError::MathOverflow)?;
        let before = alpha_escrow.amount;
        venue::swap(dex_program.clone(),config_ai.clone(),pool.clone(),base_mint.clone(),beta_mint.clone(),base_vault.clone(),quote_vault.clone(),alpha_escrow.to_account_info(),treasury.clone(),token_program.clone(),net_beta_u64,min_out,0,signer,remaining)?;
        alpha_escrow.reload()?;
        alpha_out = alpha_escrow.amount.saturating_sub(before);
        // Transfer the received Alpha from escrow to the user (config signs; Alpha is classic SPL → remaining is empty).
        transfer_checked_hooked(
            token_program,
            &alpha_escrow.to_account_info(),
            base_mint,
            user_alpha,
            config_ai,
            &[],
            alpha_out,
            base_decimals,
            signer,
        )?;
    }
    Ok((alpha_out, fee_beta))
}

/// Apply a donation's value to config + the user (advance the accumulator accrue, settle, check the cap,
/// add power).
fn apply_donation(
    cfg: &mut DonateConfig,
    us: &mut UserStake,
    treasury_beta: u64,
    value: u64,
) -> Result<u128> {
    let now = Clock::get()?.unix_timestamp;
    if us.imported && us.first_donate_ts == 0 {
        us.first_donate_ts = cfg.activation_ts;
    }
    accrue_global(cfg, treasury_beta, now);
    let acc = cfg.acc_reward_per_power;
    settle(us, acc);
    accrue_v(cfg, treasury_beta, Clock::get()?.unix_timestamp);
    settle_v(us, &VSnap::of(cfg));

    // Per-donation USDT-denominated cap.
    require!(
        value as u128 <= cfg.donate_cap_usdt as u128,
        DonateError::DonateCapExceeded
    );
    let add_power = mul_bps(value as u128, cfg.power_multiplier_bps);
    // Cap on the power a user may hold at once. Checked against `power` (the remaining release budget),
    // which `claim` draws down — so releasing power *does* free up room and a fully-released user can
    // donate again. `power_granted` stays a monotonic lifetime total for display/`cap_v`, but no longer
    // gates donations. Both checks run before any mutation, so a rejected donation leaves no half-applied
    // state.
    require!(
        us.power.saturating_add(add_power) <= USER_POWER_CAP,
        DonateError::PowerCapExceeded
    );

    // Accrual that already overflowed the *old* budget is dropped before the new power lands — otherwise
    // this donation would resurrect pending that `claim` could never have paid out. Must run after the
    // gates above (so a rejected donation forfeits nothing) and before `us.power` grows (that ordering is
    // the entire point).
    //
    // Priced through the exact same expression `claim` uses, so "what the clamp calls unclaimable" and
    // "what claim can actually pay" can never disagree — including if the FL-01 floor is switched on later
    // (both move together then). NOTE: on mainnet today `claim_floor_tol_bps == 0`, so the floor is a
    // no-op and this is simply the live-DEX `beta_usd` that `donate_alpha` just wrote — the clamp inherits
    // whatever price-manipulation exposure `claim` already carries, no more and no less. Dodging the clamp
    // means depressing `beta_usd` far enough that `budget_beta` covers the whole overflow, which is a large
    // pool move (the same lever FL-01 exists to shut, tracked separately).
    let beta_usd = beta_usd_with_floor(cfg, cfg.beta_usd as u128);
    // Beta is asserted 9dp at `initialize` (EXPECTED_TOKEN_DECIMALS), so TOKEN_UNIT is its whole-token unit.
    let forfeited_beta = forfeit_pending_over_power(us, beta_usd, TOKEN_UNIT);

    let new_total = (us.donated_usdt as u128).saturating_add(value as u128);
    us.donated_usdt = new_total as u64;

    us.power = us.power.saturating_add(add_power);
    us.power_granted = us.power_granted.saturating_add(add_power);
    cfg.total_power = cfg.total_power.saturating_add(add_power);
    // Re-anchor reward_debt with the new power, so the just-added power starts earning rewards only from "now"
    // (otherwise past-period accrual would be credited to the new power, causing over-payment).
    us.reward_debt = us.power.saturating_mul(acc) / ACC_SCALE;
    // Record the first-donate time (for the "first-claim T+delay" gate).
    if us.first_donate_ts == 0 {
        us.first_donate_ts = now;
    }
    Ok(forfeited_beta)
}

/// A `transfer_checked` that forwards the Token-2022 transfer hook's extra accounts — identical to the
/// same-named helper in dex-pool. `invoke_transfer_checked` checks whether the mint carries a hook, and if so, appends the resolved
/// hook accounts (passed by the caller as `remaining`) into the CPI itself; whereas anchor-spl's
/// `transfer_checked` cannot do this (it hardcodes only the 4 base accounts). For a hookless mint, pass
/// `remaining = &[]`. `seeds` is the PDA signing seeds needed by legs like escrow/payout vaults (empty when the account's
/// owner signs directly — including a PDA already marked as signed by the outer CPI).
#[allow(clippy::too_many_arguments)]
fn transfer_checked_hooked<'info>(
    token_program: &AccountInfo<'info>,
    from: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    remaining: &[AccountInfo<'info>],
    amount: u64,
    decimals: u8,
    seeds: &[&[&[u8]]],
) -> Result<()> {
    invoke_transfer_checked(
        token_program.key,
        from.clone(),
        mint.clone(),
        to.clone(),
        authority.clone(),
        remaining,
        amount,
        decimals,
        seeds,
    )
    .map_err(Into::into)
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[account]
#[derive(InitSpace)]
pub struct DonateConfig {
    pub startup_mode: StartupMode,
    pub ops_alpha_budget: u64,
    pub sol_budget: u64,
    pub sol_alpha_budget: u64,
    pub price_ready: bool,
    pub price_breaker: bool,
    pub like_sol_anchor: u128,
    pub emission_remainder: u128,
    pub reward_remainder: u128,
    pub migration_phase: u8,
    pub manifest_set: bool,
    pub snapshot_slot: u64,
    pub snapshot_digest: [u8; 32],
    pub expected_import_hash: [u8; 32],
    pub import_hash: [u8; 32],
    pub expected_user_count: u32,
    pub expected_power: u128,
    pub rebuilt_user_count: u32,
    pub activation_ts: i64,
    pub authority: Pubkey,
    pub keeper: Pubkey,
    pub root_referrer: Pubkey,
    /// Immutable config PDA seed, initialized from the new FLOCK mint.
    pub config_seed: Pubkey,
    pub alpha_mint: Pubkey,
    pub pool: Pubkey,
    pub base_vault: Pubkey,  // the pool's Alpha reserve (base)
    pub quote_vault: Pubkey, // the pool's Beta reserve (quote)
    pub treasury: Pubkey,    // the Beta reward reservoir (treasury)
    pub alpha_escrow: Pubkey,
    pub sol_escrow: Pubkey,
    pub operations_wallet: Pubkey,
    /// FLOCK mint bound at initialization.
    pub beta_mint: Pubkey,
    /// Informational LP recipient. Raydium migration sets the official incinerator;
    /// the actual destination is independently pinned by the venue account constraints.
    pub lp_recipient: Pubkey,
    // fund-split ratios (bps, summing to 10000)
    pub bps_operations: u16,

    pub bps_liquidity: u16,
    pub bps_treasury: u16,

    // economic parameters
    pub power_multiplier_bps: u16,
    pub donate_cap_usdt: u64, // USDT-e6 (USDT priced at 1e6 precision)
    pub daily_emission_bps: u16,
    pub user_daily_cap_bps: u16,
    pub blackhole_floor: u64,
    pub max_slippage_bps: u16,
    // fallback prices (USDT-e6, per whole token)
    pub sol_usd: u64,
    pub alpha_usd: u64,
    pub beta_usd: u64,
    // ---- Price deviation protection and the separate optional valuation floor ----
    // Audit 2026-07-09 (FL-01/FL-03/FL-04): add a trusted anchor + claim floor over the "permissionless instant pool price".
    pub beta_usd_anchor: u64, // trusted anchor for beta_usd (reference for the FL-01 claim pricing floor); 0=unset/off
    pub last_sync_ts: i64,    // timestamp of the latest ready observation
    pub min_sync_interval_secs: i64, // legacy 8-byte slot now stores level reward bps (0 on old accounts means 60)
    pub max_sync_deviation_bps: u16, // Shared oracle/transaction deviation: 1..=10000 bps; invalid legacy values use 1000.
    pub claim_floor_tol_bps: u16, // claim pricing-floor tolerance (floor = anchor×(1−tol)); 0=floor disabled
    pub pool_ratio_anchor: u128, // observed pool base×PRICE_SCALE/quote; zero is invalid for guarded operations
    // reward accumulator
    pub acc_reward_per_power: u128,
    pub total_power: u128,
    pub last_update_ts: i64,
    // Level thresholds (V1..V8; index i = V(i+1)). The minimum thresholds for AND level
    // determination, in the same units as `UserStake.power` (u64: the per-user power cap is 3e11
    // and the whole protocol's total_power is ~1e13, so a u64 threshold has four orders of spare).
    pub level_min_power: [u64; LEVEL_TIERS],
    pub level_min_team: [u64; LEVEL_TIERS],
    /// Small-area performance threshold per level = `team_power − max_leg`, i.e. team
    /// performance excluding the single largest direct leg. 0 = no requirement.
    pub level_min_small: [u64; LEVEL_TIERS],
    /// Star gate per level: minimum number of direct children whose cached level is at least
    /// `STAR_COUNT_LEVEL`. Must be 0 for every level below `STAR_COUNT_LEVEL` (enforced in
    /// `set_levels`); V7/V8 use it for the "2 direct referrals at V6" requirement.
    pub level_min_star: [u8; LEVEL_TIERS],

    pub level_share_cap_bps: u16,

    /// Normalized reward weights, index 0 = V1 through 7 = V8.
    pub level_weights: [u16; V_TIERS],
    /// Per-tier FLOCK accumulator per enrolled member, scaled by ACC_SCALE.
    pub acc_per_v: [u128; V_TIERS],
    /// Number of enrolled members per tier.
    pub total_v_weight: [u128; V_TIERS],
    /// Number of accounts `seed_leg` has processed — lets the rollout tell when the one-shot
    /// small-area backfill is complete without scanning every account.
    pub leg_seeded_count: u32,
    /// (2026-09 redesign) Migration lock for `admin_seed_user`. False while the fresh deploy is being
    /// seeded with the migrated referrer/power snapshot; `finalize_seeding` flips it to true forever,
    /// after which `admin_seed_user` is permanently rejected. A one-way latch — no un-lock instruction.
    pub seeding_locked: bool,
    /// Number of accounts `admin_seed_user` has created/seeded (rollout progress; parallels `leg_seeded_count`).
    pub seeded_user_count: u32,
    pub v_last_update_ts: i64, // last time the V-pool accumulators were advanced

    pub v_dividend_enabled: bool, // Level accrual switch; recorded rewards remain claimable while disabled
    // ---- Promotion reward (generational promotion reward) config (by generation; index g-1 maps to the g-th generation upline) ----
    // all default to 0 / valid threshold = u128::MAX (off), until the authority writes the real tables via `set_promotion`.
    pub promo_rate_bps: [u16; MAX_UPLINE], // per-generation rate bps (base = the Beta claimed this time)
    pub promo_min_direct: [u16; MAX_UPLINE], // the "valid direct referral count" required per generation
    pub promo_min_power: [u128; MAX_UPLINE], // the "upline's own power" required per generation (same units as UserStake.power)
    pub valid_invite_min_power: u128, // "valid direct referral" threshold: an invitee counts toward the upline's valid direct count only if their power ≥ this value
    // ---- Withdrawal fee (docs/donate.md claim mechanism; configurable, written by set_claim_fee) ----
    // Note: the blackhole share is now burned directly against treasury inside claim (truly destroyed); there is no longer an admin-specifiable destination account.
    pub total_burned: u128, // cumulative Beta truly destroyed via the blackhole fee (for the frontend's "blackhole burn pool" display)
    pub claim_fee_base_bps: u16, // base withdrawal fee (default 2000 = 20%)
    pub claim_fee_drop_bps: [u16; 4], // anti-dump drop-tier thresholds (bps; default [500,1000,2000,3000])
    pub claim_fee_tier_bps: [u16; 4], // per-tier fees (default [2500,3000,3500,4000])
    pub claim_fee_operations_bps: u16, // Operations FLOCK share of the fee (1500=15%).
    pub claim_fee_blackhole_bps: u16, // the "blackhole" share of the fee (default 3500=35%); the rest stays in treasury
    pub claim_first_delay_secs: i64, // wait seconds after the first donate before withdrawal is allowed (default 86400)
    pub day_window_secs: i64, // seconds in a "day" for emission/daily cap (default 86400; can be shortened locally for testing)
    pub sol_escrow_bump: u8,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct UserStake {
    pub owner: Pubkey,
    pub referrer: Pubkey, // bound only once; default value => not yet registered
    pub direct_count: u32,
    pub power_granted: u128, // Σ cumulative power granted (the denominator of the UI release progress bar)
    pub power: u128,         // remaining power = both the weight and the release budget
    pub donated_usdt: u64, // USDT-e6 (lifetime cumulative, for display/stats; does not participate in the per-donation limit check)
    pub reward_debt: u128, // MasterChef accumulator checkpoint: the settled "total entitlement" baseline
    pub pending: u128,     // settled but not-yet-claimed Beta (smallest units)
    // ---- small-area bookkeeping ----
    /// Σ power of this user plus every descendant within `LEG_UPLINE` levels below — the "leg"
    /// this user contributes to its direct upline. Σ over a parent's children ≡ parent.team_power.
    pub leg_power: u64,
    /// The largest `leg_power` among this user's direct children. `sync_level` only ever raises it
    /// (it sees one leg at a time); `refresh_max_leg` recomputes it exactly from the whole set.
    /// A stale-high value under-reports the small area, which is the safe direction.
    pub max_leg: u64,
    /// Number of direct children whose cached level is >= `STAR_COUNT_LEVEL` (the V7/V8 star gate).
    pub direct_ge6: u32,
    /// Bit flags; see `FLAG_LEG_SEEDED`.
    pub flags: u8,

    pub _reserved: u8,
    // ---- Level (VIP) related ----
    pub direct_power: u128, // Σ direct-referral (level-1 downline) power — direct-referral performance
    pub team_power: u128, // Σ power of all downlines within 10 levels below — team performance (excluding oneself)
    pub synced_power: u128, // the already-upward-propagated "own power" baseline (Δ tracking, self-healing)
    pub level: u8,          // cached level 0..=8 (0 = below V1)
    pub v_reward_debt: u128, // checkpoint against the "current tier's accumulator"
    pub level_reward_remainder: u128,
    pub v_pending: u128, // settled, unclaimed level reward FLOCK (smallest units)
    pub v_tier: u8, // 0 or 1..=8 — the bucket currently counted in total_v_weight (separate from level, the source of truth for bucket accounting)
    // ---- Promotion reward (generational promotion reward) ----
    pub direct_valid_count: u32, // valid direct referral count (number of direct downlines with power ≥ valid_invite_min_power)
    pub is_valid: bool, // whether the user is currently counted in "the upline's valid direct referrals" (source of truth for crossing detection, self-healing)
    pub promo_pending: u128, // recorded-but-unclaimed promotion-reward Beta (smallest units); released by claim at the cost of power
    pub first_donate_ts: i64, // first-donate timestamp (0=never donated); used by the "first-claim T+delay" gate
    pub last_claim_ts: i64, // last-claim timestamp (0=never claimed); shared cooldown only

    pub bump: u8,
    pub imported: bool,
    pub rebuilt: bool,
    pub rebuilt_children: u32,
    pub rebuild_cursor: Pubkey,
    pub descendant_power: [u128; MAX_UPLINE],
    /// Monotonic parent-assigned position, 1..=parent.direct_count (root is 0).
    /// New zero-power referrals can be excluded from a scan's frozen prefix safely.
    pub referral_index: u32,
    /// Incremented whenever any immediate child's leg can change, including 10th-hop deltas.
    pub child_leg_revision: u64,
}

/// Program-owned account holding donated SOL.
#[account]
#[derive(InitSpace)]
pub struct SolEscrow {
    pub bump: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace)]
pub enum StartupMode {
    Migration,
    Fresh,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct InitializeParams {
    pub bps_operations: u16,

    pub bps_liquidity: u16,
    pub bps_treasury: u16,

    pub power_multiplier_bps: u16,
    pub donate_cap_usdt: u64,
    pub daily_emission_bps: u16,
    pub user_daily_cap_bps: u16,
    /// The seconds in a "day" that emission/daily-cap are based on (configurable for easy testing: 86400 on mainnet, 600/60 locally).
    pub day_window_secs: i64,
    pub blackhole_floor: u64,
    pub max_slippage_bps: u16,
    pub keeper: Pubkey,
}

impl InitializeParams {
    /// Validate the split ratios and the various params: the five bps values must sum to exactly 10000, the daily-emission/daily-cap bps must not exceed
    /// 10000, the max-slippage bps must be strictly less than 10000 (otherwise min_out could compute to 0, losing protection),
    /// and the power multiplier must be positive (otherwise a donation produces no power).
    fn validate(&self) -> Result<()> {
        let sum = self.bps_operations as u32 + self.bps_liquidity as u32 + self.bps_treasury as u32;
        require!(sum == BPS_DENOM as u32, DonateError::BadRatios);
        require!(self.daily_emission_bps <= BPS_DENOM, DonateError::BadRatios);
        require!(self.user_daily_cap_bps <= BPS_DENOM, DonateError::BadRatios);
        require!(self.max_slippage_bps < BPS_DENOM, DonateError::BadRatios);
        require!(self.power_multiplier_bps > 0, DonateError::BadRatios);
        require!(self.day_window_secs >= 1, DonateError::BadTimeWindow);
        #[cfg(feature = "mainnet-test-10m")]
        require!(self.day_window_secs == SECONDS_PER_DAY, DonateError::BadTimeWindow);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Account contexts (Contexts)
// ---------------------------------------------------------------------------

// Note: these account fields are all wrapped in `Box` so that Anchor's generated `try_accounts` stays within
// the 4 KB SBF stack frame limit (the same constraint the dex-pool program hit).
#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(constraint = program.programdata_address()? == Some(program_data.key()) @ DonateError::Unauthorized)]
    pub program: Program<'info, crate::program::Donate>,
    #[account(constraint = program_data.upgrade_authority_address == Some(authority.key()) @ DonateError::Unauthorized)]
    pub program_data: Box<Account<'info, ProgramData>>,
    pub beta_mint: Box<InterfaceAccount<'info, Mint>>,
    pub alpha_mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(constraint=pool.base_mint==alpha_mint.key() && pool.quote_mint==beta_mint.key() && pool.base_vault==base_vault.key() && pool.quote_vault==quote_vault.key() @ DonateError::BadVaultAccount)]
    pub pool: Box<InterfaceAccount<'info, VenuePool>>,
    #[account(token::mint = alpha_mint, token::token_program = token_program)]
    pub base_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(token::mint = beta_mint, token::token_program = token_program)]
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    /// CHECK: a fund wallet; only its key is stored here.
    pub operations_wallet: UncheckedAccount<'info>,
    /// CHECK: recipient of the donation LP leg; only its key is stored here.
    pub lp_recipient: UncheckedAccount<'info>,
    /// CHECK: the referral-tree root (top self-loop node). Configurable — its key is
    /// stored as `config.root_referrer` and used as the owner/referrer + PDA seed of
    /// `root_user`. Defaults to the deployer if the bootstrap passes `authority`.
    pub root_referrer: UncheckedAccount<'info>,

    // Note: config must be initialized before treasury/alpha_escrow, because the latter two use config as their
    // `token::authority` (Anchor initializes fields one by one in declaration order, so the referenced one must exist first).
    #[account(
        init,
        payer = authority,
        space = 8 + DonateConfig::INIT_SPACE,
        seeds = [b"donate-config", beta_mint.key().as_ref()],
        bump
    )]
    pub config: Box<Account<'info, DonateConfig>>,

    #[account(
        init,
        payer = authority,
        seeds = [b"treasury", config.key().as_ref()],
        bump,
        token::mint = beta_mint,
        token::authority = config,
        token::token_program = token_program,
    )]
    pub treasury: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(
        init,
        payer = authority,
        seeds = [b"alpha-escrow", config.key().as_ref()],
        bump,
        token::mint = alpha_mint,
        token::authority = config,
        token::token_program = token_program,
    )]
    pub alpha_escrow: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(
        init,
        payer = authority,
        space = 8 + SolEscrow::INIT_SPACE,
        seeds = [b"sol-escrow", config.key().as_ref()],
        bump
    )]
    pub sol_escrow: Box<Account<'info, SolEscrow>>,

    #[account(
        init,
        payer = authority,
        space = 8 + UserStake::INIT_SPACE,
        seeds = [b"donate-user", config.key().as_ref(), root_referrer.key().as_ref()],
        bump
    )]
    pub root_user: Box<Account<'info, UserStake>>,

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct AdminConfig<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"donate-config", config.config_seed.as_ref()],
        bump = config.bump,
        has_one = authority @ DonateError::Unauthorized,
    )]
    pub config: Box<Account<'info, DonateConfig>>,
}

#[derive(Accounts)]
pub struct SetAlpha<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"donate-config", config.config_seed.as_ref()],
        bump = config.bump,
        has_one = authority @ DonateError::Unauthorized,
    )]
    pub config: Box<Account<'info, DonateConfig>>,
    pub alpha_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CHECK: the new dex pool PDA; only its key is stored here, to be re-validated by the dex program's own
    /// seeds/has_one when `allocate` uses it.
    pub pool: UncheckedAccount<'info>,
    /// the new pool's Alpha reserve (base).
    #[account(token::mint = alpha_mint)]
    pub base_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    /// Beta reserve (quote); its mint is checked against config.beta_mint in the handler.
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(token::mint = alpha_mint, token::authority = config)]
    pub alpha_escrow: Box<InterfaceAccount<'info, TokenAccount>>,
}

#[derive(Accounts)]
pub struct SetPrice<'info> {
    pub signer: Signer<'info>,
    #[account(mut, seeds = [b"donate-config", config.config_seed.as_ref()], bump = config.bump)]
    pub config: Box<Account<'info, DonateConfig>>,
}

// Configure the price anti-manipulation gates: keeper/authority signs; `pool_ratio_anchor` is computed on-chain from this pool's two vault balances.
#[derive(Accounts)]
pub struct SetPriceGuards<'info> {
    pub signer: Signer<'info>,
    #[account(mut, seeds = [b"donate-config", config.config_seed.as_ref()], bump = config.bump)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(address = config.base_vault)]
    pub base_vault: Box<InterfaceAccount<'info, TokenAccount>>, // Alpha(base)
    #[account(address = config.quote_vault)]
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>, // Beta(quote)
}

// Permissionless price feed: no Signer gate (the fee payer's signature suffices), reads DEX vault balances only + writes the three config prices.
// All vaults are bound by address constants / config fields to prevent account substitution. No token_program needed (only deserialize to read balances).

#[derive(Accounts)]
#[instruction(referrer: Pubkey)]
pub struct BindReferrer<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(seeds = [b"donate-config", config.config_seed.as_ref()], bump = config.bump)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(
        init,
        payer = user,
        space = 8 + UserStake::INIT_SPACE,
        seeds = [b"donate-user", config.key().as_ref(), user.key().as_ref()],
        bump
    )]
    pub user_stake: Box<Account<'info, UserStake>>,
    // Must already exist → this enforces "the referrer is registered" (except the root, which was created at init).
    // If this PDA is not yet initialized, Anchor reverts directly.
    #[account(
        mut,
        seeds = [b"donate-user", config.key().as_ref(), referrer.as_ref()],
        bump = referrer_stake.bump
    )]
    pub referrer_stake: Box<Account<'info, UserStake>>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct DonateSol<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(mut, seeds = [b"donate-config", config.config_seed.as_ref()], bump = config.bump)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(
        mut,
        seeds = [b"donate-user", config.key().as_ref(), user.key().as_ref()],
        bump = user_stake.bump,
    )]
    pub user_stake: Box<Account<'info, UserStake>>,
    #[account(address=config.pool)]
    pub pool: Box<InterfaceAccount<'info, VenuePool>>,
    #[account(address = config.quote_vault)]
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>, // Beta reserve (quote)
    #[account(mut, address = config.treasury)]
    pub treasury: Box<InterfaceAccount<'info, TokenAccount>>,
    // Live DEX quote (read-only balances): SOL/USDC deep pool → derive SOL/USD.
    #[account(address = SOLUSDC_WSOL_VAULT)]
    pub solusdc_wsol_vault: Box<InterfaceAccount<'info, TokenAccount>>, // WSOL(9dp)
    #[account(address = SOLUSDC_USDC_VAULT)]
    pub solusdc_usdc_vault: Box<InterfaceAccount<'info, TokenAccount>>, // USDC(6dp)
    // The donated SOL is wrapped directly into WSOL and deposited in config's WSOL ATA (system transfer + sync_native), to be spent by the buy leg.
    // The ATA is idempotently created by the dApp within the same tx; the buy leg closes it at the end to unwrap the configured operations SOL.
    #[account(address = WSOL_MINT)]
    pub wsol_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, associated_token::mint = wsol_mint, associated_token::authority = config)]
    pub config_wsol_ata: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(address = anchor_spl::token::ID)]
    // WSOL = classic SPL (Tokenkeg), which sync_native goes through
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
    /// CHECK: Instructions sysvar — introspects whether this tx atomically bundles `allocate_sol_buy` +
    /// `allocate_sol_distribute` covering this donation amount (enforced check, see the end of `donate_sol`). Bound by address constant.
    #[account(address = anchor_lang::solana_program::sysvar::instructions::ID)]
    pub instructions: UncheckedAccount<'info>,
    #[account(init_if_needed, payer = user, space = 8 + ClaimAllowance::INIT_SPACE,
        seeds = [b"claim-allowance", config.key().as_ref(), user.key().as_ref()], bump)]
    pub claim_allowance: Box<Account<'info, ClaimAllowance>>,

}

// Alpha donation — escrow + **atomic distribution** (treasury, liquidity and operations legs; the configured operations leg is consumed by the same tx's
// `allocate_operations_direct` instruction, which consumes the operations share left in escrow). All accounts are Box'd to avoid the SBF
// 4 KB stack frame limit (same reason as dex-pool AddLiquidity). The distribution legs run dex `swap`/`add_liquidity` CPIs,
// so there are many accounts — the dApp carries it with a v0 tx + Address Lookup Table.
//
// This struct sits AT the 4 KB ceiling: the generated `try_accounts` measured 4104 bytes (8 over) when the
// blackhole address was carried as its own `UncheckedAccount`, which made every `donate_alpha` abort with
// "Access violation in stack frame 5" before the body ran. It is now read from `config.blackhole` instead.
// Adding any account here can re-cross the limit — after changing this struct, check `cargo build-sbf` for
// "Stack offset of N exceeded max offset of 4096". The overflow does NOT show up in `anchor test` on a
// host build; only a real SBF build/run catches it.
/// Read-only price sources: the Raydium LIKE/WSOL pool and the deep SOL/USDC pool.
///
/// These live in their own `#[derive(Accounts)]` on purpose. Anchor generates one
/// `try_accounts` per accounts-struct, and every generated function gets its own 4 KB SBF
/// stack frame — so pulling this group out of `DonateAlpha` moves its deserialization
/// temporaries into a separate frame instead of piling them onto the parent's.
///
/// This is load-bearing, not cosmetic: `DonateAlpha::try_accounts` was 4992 bytes under the
/// rustc the verifiable-build image ships, which bricked every mainnet LIKE donate on
/// 2026-07-20. Do not inline these fields back into the parent struct.
///
/// Every constraint here is self-contained (address constants only) — a nested accounts
/// struct cannot reference the parent's fields, which is why only this group can be split out.
/// The dex pool's two reserve vaults. Split out for the same 4 KB stack-frame reason as
/// [`PriceVaults`]. A nested accounts struct cannot see the parent's `config`, so the
/// `address = config.base_vault` / `config.quote_vault` bindings move into the handler as
/// explicit `require_keys_eq!` checks — identical validation, just expressed imperatively.
/// The two fields stay adjacent and in their original positions, so the on-chain account
/// order of every instruction using this group is byte-for-byte unchanged.
#[derive(Accounts)]
pub struct PoolVaults<'info> {
    #[account(mut)] // checked in the handler: == config.base_vault
    pub base_vault: Box<InterfaceAccount<'info, TokenAccount>>, // Alpha reserve (base)
    #[account(mut)] // checked in the handler: == config.quote_vault
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>, // Beta reserve (quote)
}

/// The three protocol-owned Beta sinks. Same rationale as [`PoolVaults`]: split out for the
/// stack frame, `address = config.*` re-checked in the handler, original adjacency preserved.
#[derive(Accounts)]
pub struct DistVaults<'info> {
    #[account(mut)] // checked in the handler: == config.treasury
    pub treasury: Box<InterfaceAccount<'info, TokenAccount>>, // 45% Beta goes here
}

impl<'info> PoolVaults<'info> {
    /// Re-assert the `address = config.*` bindings that the nested struct cannot express.
    fn check(&self, cfg: &DonateConfig) -> Result<()> {
        require_keys_eq!(
            self.base_vault.key(),
            cfg.base_vault,
            DonateError::BadVaultAccount
        );
        require_keys_eq!(
            self.quote_vault.key(),
            cfg.quote_vault,
            DonateError::BadVaultAccount
        );
        Ok(())
    }
}

impl<'info> DistVaults<'info> {
    /// Re-assert the `address = config.*` bindings that the nested struct cannot express.
    fn check(&self, cfg: &DonateConfig) -> Result<()> {
        require_keys_eq!(
            self.treasury.key(),
            cfg.treasury,
            DonateError::BadVaultAccount
        );
        Ok(())
    }
}

#[derive(Accounts)]
pub struct PriceVaults<'info> {
    #[account(address = RAYDIUM_COIN_VAULT)]
    pub amm_coin_vault: Box<InterfaceAccount<'info, TokenAccount>>, // Raydium LIKE(9dp)
    #[account(address = RAYDIUM_PC_VAULT)]
    pub amm_pc_vault: Box<InterfaceAccount<'info, TokenAccount>>, // Raydium WSOL(9dp)
    #[account(address = SOLUSDC_WSOL_VAULT)]
    pub solusdc_wsol_vault: Box<InterfaceAccount<'info, TokenAccount>>, // WSOL(9dp)
    #[account(address = SOLUSDC_USDC_VAULT)]
    pub solusdc_usdc_vault: Box<InterfaceAccount<'info, TokenAccount>>, // USDC(6dp)
}

#[derive(Accounts)]
pub struct DonateAlpha<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(mut, seeds = [b"donate-config", config.config_seed.as_ref()], bump = config.bump)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(
        mut,
        seeds = [b"donate-user", config.key().as_ref(), user.key().as_ref()],
        bump = user_stake.bump,
    )]
    pub user_stake: Box<Account<'info, UserStake>>,
    // ---- Donated asset (Alpha = pool base) + escrow ----
    #[account(mut, address = config.alpha_mint)]
    // mut: add_liquidity's base leg goes through transfer_checked
    pub alpha_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, token::mint = alpha_mint, token::authority = user)]
    pub user_alpha: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, address = config.alpha_escrow)]
    pub alpha_escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    // ---- Shared dex pool (swap + add_liquidity CPI) ----
    #[account(mut, address = config.pool, has_one = lp_mint)]
    pub pool: Box<InterfaceAccount<'info, VenuePool>>,
    #[account(mut, address = config.beta_mint)]
    // pool quote_mint; mut: the burn leg changes its supply
    pub beta_mint: Box<InterfaceAccount<'info, Mint>>,
    // Nested so they get their own try_accounts stack frame — see PoolVaults.
    pub vaults: PoolVaults<'info>,
    // Live DEX quote (read-only balances): Raydium LIKE/WSOL + SOL/USDC deep pool → derive the three prices alpha/sol/beta.
    // Nested so it gets its own try_accounts stack frame — see PriceVaults.
    pub prices: PriceVaults<'info>,
    #[account(mut)] // checked: pool.has_one = lp_mint
    pub lp_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CHECK: legacy minimum-liquidity vault; unused by CPMM, where pass pool itself.
    #[account(mut, constraint=pool.cpmm.is_some() || pool.clmm.is_some() || lp_lock.key()==Pubkey::find_program_address(&[b"lp_lock",pool.key().as_ref()],&dex_pool::ID).0 @ DonateError::BadVaultAccount)]
    pub lp_lock: UncheckedAccount<'info>,
    /// CPMM: canonical incinerator LP ATA. Legacy: existing Locker vault, retained for ABI compatibility.
    #[account(mut, address=venue::lp_sink(&pool,lp_mint.key()),
        token::mint=lp_mint, token::token_program=token_program,
        constraint=locked_lp_vault.owner == venue::lp_sink_owner(&pool) @ DonateError::BadChain)]
    pub locked_lp_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    // ---- config-owned vaults / intermediary accounts (authority = config PDA) ----
    // Nested so they get their own try_accounts stack frame — see DistVaults.
    pub sinks: DistVaults<'info>,
    // Pinned to config's canonical ATA. With only `token::mint/authority` any config-owned Beta account
    // passed here — e.g. another config-owned vault — had its WHOLE balance swept into treasury by the leftover-Beta
    // leg (audit 2026-09-15 DN-01); Anchor does not reject a writable account passed twice.
    #[account(mut, associated_token::mint = beta_mint, associated_token::authority = config)]
    pub config_beta_ata: Box<InterfaceAccount<'info, TokenAccount>>, // temporary Beta intermediary for the liquidity/burn legs
    // The LP intermediary that `add_liquidity` mints into (passed as its `user_lp`). We enforce mint + authority
    // by field comparison (`token::…`) rather than re-deriving the ATA address (`associated_token::…`): the ATA
    // PDA derivation was pushing DonateAlpha's try_accounts frame 8 bytes over the 4 KB SBF ceiling, and any
    // config-owned LP account is equally safe here (it is only a pass-through that is fully emptied into locked_lp_vault).
    #[account(mut, token::mint = lp_mint, token::authority = config)]
    pub config_lp_ata: Box<InterfaceAccount<'info, TokenAccount>>, // recipient of the LP minted by add_liquidity (= the signer)
    // Retain the old account slot and Box for ABI / SBF stack compatibility.
    // CPMM callers pass the official incinerator ATA, prepared atomically before donation.
    /// CHECK: executable and bound to the typed venue owner.
    #[account(executable, constraint=dex_program.key()==pool.program() @ DonateError::BadVaultAccount)]
    pub dex_program: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    /// CHECK: Instructions sysvar — used to introspect whether this tx atomically bundles the
    /// `allocate_operations_direct` covering this 3% (enforced check, see the end of `donate_alpha`). Bound by address constant.
    #[account(address = anchor_lang::solana_program::sysvar::instructions::ID)]
    pub instructions: UncheckedAccount<'info>,
    #[account(init_if_needed, payer = user, space = 8 + ClaimAllowance::INIT_SPACE,
        seeds = [b"claim-allowance", config.key().as_ref(), user.key().as_ref()], bump)]
    pub claim_allowance: Box<Account<'info, ClaimAllowance>>,
    pub system_program: Program<'info, System>,

}

#[derive(Accounts)]
pub struct Accrue<'info> {
    #[account(mut, seeds = [b"donate-config", config.config_seed.as_ref()], bump = config.bump)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(address=config.pool)]
    pub pool: Box<InterfaceAccount<'info, VenuePool>>,
    #[account(address = config.quote_vault)]
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>, // Beta reserve (quote)
    #[account(address = config.treasury)]
    pub treasury: Box<InterfaceAccount<'info, TokenAccount>>,
}

#[derive(Accounts)]
pub struct Claim<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(mut, seeds = [b"donate-config", config.config_seed.as_ref()], bump = config.bump)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(
        mut,
        seeds = [b"donate-user", config.key().as_ref(), user.key().as_ref()],
        bump = user_stake.bump,
    )]
    pub user_stake: Box<Account<'info, UserStake>>,
    // ---- dex pool (read open_price + auto-sell Beta→Alpha) ----
    #[account(mut, address = config.pool)]
    pub pool: Box<InterfaceAccount<'info, VenuePool>>,
    #[account(address = config.alpha_mint)]
    pub base_mint: Box<InterfaceAccount<'info, Mint>>, // Alpha
    #[account(mut, address = config.beta_mint)]
    pub beta_mint: Box<InterfaceAccount<'info, Mint>>, // Beta (the swap's quote_mint); mut: the blackhole share is burned, changing its supply
    #[account(mut, address = config.base_vault)]
    pub base_vault: Box<InterfaceAccount<'info, TokenAccount>>, // Alpha reserve (base)
    #[account(mut, address = config.quote_vault)]
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>, // Beta reserve (quote; the swap writes it)
    // ---- payout source / destination ----
    #[account(mut, address = config.treasury)]
    pub treasury: Box<InterfaceAccount<'info, TokenAccount>>, // Beta payout source (the blackhole share is also burned from here)
    #[account(mut, address = config.alpha_escrow)]
    pub alpha_escrow: Box<InterfaceAccount<'info, TokenAccount>>, // intermediary for the received Alpha
    #[account(mut, associated_token::mint = beta_mint, associated_token::authority = config.operations_wallet)]
    pub operations_beta: Box<InterfaceAccount<'info, TokenAccount>>, // operations share of the withdrawal fee
    #[account(mut, token::mint = base_mint, token::authority = user)]
    pub user_alpha: Box<InterfaceAccount<'info, TokenAccount>>, // the user receives Alpha
    pub prices: PriceVaults<'info>,
    /// CHECK: executable and bound to the typed venue owner.
    #[account(executable, constraint=dex_program.key()==pool.program() @ DonateError::BadVaultAccount)]
    pub dex_program: UncheckedAccount<'info>,
    #[account(address = anchor_spl::token::ID)]
    pub token_program: Interface<'info, TokenInterface>,
    /// CHECK: Instructions sysvar — enforces a subsequent `sync_level` for the same `(config, user_stake)` in this tx.
    #[account(address = anchor_lang::solana_program::sysvar::instructions::ID)]
    pub instructions: UncheckedAccount<'info>,
    #[account(init_if_needed, payer = user, space = 8 + ClaimAllowance::INIT_SPACE,
        seeds = [b"claim-allowance", config.key().as_ref(), user.key().as_ref()], bump)]
    pub claim_allowance: Box<Account<'info, ClaimAllowance>>,
    pub system_program: Program<'info, System>,

}

/// Level reward configuration, controlled by the donate authority.
#[derive(Accounts)]
pub struct LevelRewardAdmin<'info> {
    #[account(address = config.authority @ DonateError::Unauthorized)]
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"donate-config", config.config_seed.as_ref()],
        bump = config.bump,
    )]
    pub config: Box<Account<'info, DonateConfig>>,
    // Settle the previous reward configuration using the current treasury FLOCK balance.
    #[account(address = config.treasury)]
    pub treasury: Box<InterfaceAccount<'info, TokenAccount>>,
}

/// `seed_leg`: config is writable only for the `leg_seeded_count` counter; the upline chain comes
/// in as writable `remaining_accounts` and is validated hop by hop by `load_checked_ancestor`.
#[derive(Accounts)]
pub struct SeedLeg<'info> {
    #[account(mut, seeds = [b"donate-config", config.config_seed.as_ref()], bump = config.bump)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(
        mut,
        seeds = [b"donate-user", config.key().as_ref(), user_stake.owner.as_ref()],
        bump = user_stake.bump,
    )]
    pub user_stake: Box<Account<'info, UserStake>>,
}

/// `migrate_config`: see the handler for why `config` is unchecked here.

/// Shared by `sync_level` and `refresh_max_leg`.
#[derive(Accounts)]
pub struct SyncLevel<'info> {
    #[account(mut, seeds = [b"donate-config", config.config_seed.as_ref()], bump = config.bump)]
    pub config: Box<Account<'info, DonateConfig>>,
    // the user whose power changed; the PDA is re-checked against its own recorded owner. The upline chain is passed as remaining_accounts
    // (all writable UserStake PDAs, nearest upline first), with the handler validating the chain + canonical PDA hop by hop.
    #[account(
        mut,
        seeds = [b"donate-user", config.key().as_ref(), user_stake.owner.as_ref()],
        bump = user_stake.bump,
    )]
    pub user_stake: Box<Account<'info, UserStake>>,
    #[account(address = config.treasury)]
    pub treasury: Box<InterfaceAccount<'info, TokenAccount>>,
}

#[derive(Accounts)]
pub struct AllocateTreasury<'info> {
    /// permissionless: any signer willing to pay the fee can trigger this allocation.
    pub cranker: Signer<'info>,
    #[account(mut, seeds = [b"donate-config", config.config_seed.as_ref()], bump = config.bump)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(mut, address = config.alpha_escrow)]
    pub alpha_escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, address = config.treasury)]
    pub treasury: Box<InterfaceAccount<'info, TokenAccount>>,
    // Typed (like the other pool-touching contexts) so the on-chain fair quote can read pool.fee_bps and
    // mirror the pool's fee — quoting fee-free would spend the max_slippage_bps budget on the fee itself.
    // mut: the swap CPI writes pool's open_price, so the outer instruction must mark pool writable, else the CPI privilege escalation fails.
    #[account(mut, address = config.pool)]
    pub pool: Box<InterfaceAccount<'info, VenuePool>>,
    #[account(address = config.alpha_mint)]
    pub base_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, address = config.beta_mint)]
    // pool quote_mint; mut: the swap's burn leg changes its supply
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, address = config.base_vault)] // Alpha reserve (base)
    pub base_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, address = config.quote_vault)] // Beta reserve (quote)
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: executable and bound to the typed venue owner.
    #[account(executable, constraint=dex_program.key()==pool.program() @ DonateError::BadVaultAccount)]
    pub dex_program: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
}

// configured operations leg: transfer the escrow's Alpha straight to the operations wallet's Alpha ATA (no swap).
// The destination is pinned to the ATA of config.operations_wallet, so it cannot be redirected.
#[derive(Accounts)]
pub struct AllocateOperationsDirect<'info> {
    #[account(mut, seeds = [b"donate-config", config.config_seed.as_ref()], bump = config.bump)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(mut, address = config.alpha_escrow)]
    pub alpha_escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(address = config.alpha_mint)]
    pub alpha_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CHECK: the operations wallet; only used to derive its Alpha ATA. Bound by config.operations_wallet.
    #[account(address = config.operations_wallet)]
    pub operations_wallet: UncheckedAccount<'info>,
    // The operations wallet's Alpha ATA: must exist (the dApp creates it idempotently in the same tx).
    #[account(
        mut,
        associated_token::mint = alpha_mint,
        associated_token::authority = operations_wallet,
    )]
    pub operations_alpha_ata: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

// SOL donation buy leg: configured operations share paid in WSOL, remainder buys
// Alpha into alpha_escrow under exact-debit/minimum-credit/permission guards. config is fixed at index 0 (for
// `donate_sol`'s introspection check). All Box'd to avoid the SBF 4 KB stack frame limit.
/// The Raydium AMM v4 + OpenBook account set for the SOL buy leg.
///
/// Split into its own `#[derive(Accounts)]` for the 4 KB SBF stack frame (same reason as
/// [`PriceVaults`]). Every constraint is an address constant, so the group is self-contained
/// and the fields keep their original adjacency — the on-chain account order is unchanged.
#[derive(Accounts)]
pub struct RaydiumMarket<'info> {
    /// CHECK: the Raydium amm program (validated against an address constant).
    #[account(address = RAYDIUM_AMM_PROGRAM)]
    pub raydium_program: UncheckedAccount<'info>,
    /// CHECK: validated against an address constant.
    #[account(mut, address = RAYDIUM_POOL)]
    pub amm: UncheckedAccount<'info>,
    /// CHECK: validated against an address constant.
    #[account(address = RAYDIUM_AUTHORITY)]
    pub amm_authority: UncheckedAccount<'info>,
    /// CHECK: validated against an address constant.
    #[account(mut, address = RAYDIUM_OPEN_ORDERS)]
    pub amm_open_orders: UncheckedAccount<'info>,
    #[account(mut, address = RAYDIUM_COIN_VAULT)] // LIKE reserve (read .amount to compute min_out)
    pub amm_coin_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, address = RAYDIUM_PC_VAULT)] // WSOL reserve
    pub amm_pc_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: validated against an address constant.
    #[account(address = OPENBOOK_PROGRAM)]
    pub serum_program: UncheckedAccount<'info>,
    /// CHECK: validated against an address constant.
    #[account(mut, address = SERUM_MARKET)]
    pub serum_market: UncheckedAccount<'info>,
    /// CHECK: validated against an address constant.
    #[account(mut, address = SERUM_BIDS)]
    pub serum_bids: UncheckedAccount<'info>,
    /// CHECK: validated against an address constant.
    #[account(mut, address = SERUM_ASKS)]
    pub serum_asks: UncheckedAccount<'info>,
    /// CHECK: validated against an address constant.
    #[account(mut, address = SERUM_EVENT_QUEUE)]
    pub serum_event_queue: UncheckedAccount<'info>,
    /// CHECK: validated against an address constant.
    #[account(mut, address = SERUM_COIN_VAULT)]
    pub serum_coin_vault: UncheckedAccount<'info>,
    /// CHECK: validated against an address constant.
    #[account(mut, address = SERUM_PC_VAULT)]
    pub serum_pc_vault: UncheckedAccount<'info>,
    /// CHECK: validated against an address constant.
    #[account(address = SERUM_VAULT_SIGNER)]
    pub serum_vault_signer: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct AllocateSolBuy<'info> {
    #[account(mut, seeds = [b"donate-config", config.config_seed.as_ref()], bump = config.bump)]
    pub config: Box<Account<'info, DonateConfig>>,
    /// CHECK: operations wallet, bound by config.operations_wallet.
    #[account(mut, address = config.operations_wallet)]
    pub operations_wallet: UncheckedAccount<'info>,
    #[account(address = WSOL_MINT)]
    pub wsol_mint: Box<InterfaceAccount<'info, Mint>>,
    // Config's canonical WSOL ATA: only the registered non-operations input is spent.
    #[account(mut, associated_token::mint = wsol_mint, associated_token::authority = config)]
    pub config_wsol_ata: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, address = config.alpha_escrow)]
    // the Alpha bought on Raydium lands here, to be consumed by the distribution leg
    pub alpha_escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut,token::mint=wsol_mint,token::authority=operations_wallet)]
    pub operations_wsol_ata: Box<InterfaceAccount<'info, TokenAccount>>,
    // ---- Raydium AMM v4 / OpenBook (all validated against address constants) ----
    // Nested so it gets its own try_accounts stack frame — see RaydiumMarket.
    pub market: RaydiumMarket<'info>,
    #[account(address = anchor_spl::token::ID)]
    // Raydium requires the classic SPL Token program (Tokenkeg)
    pub token_program: Interface<'info, TokenInterface>,
}

// SOL donation **distribution leg** (allocate_sol_distribute): distribute the Alpha bought by the buy leg into alpha_escrow by 45/50 treasury/liquidity after operations.
// Accounts ≈ the self-built-dex distribution portion of DonateAlpha (+ instructions sysvar for the buy-leg-precedes check). config is fixed
// at **index 0** (for `donate_sol`'s introspection check). All Box'd to avoid the SBF 4 KB stack frame limit.
#[derive(Accounts)]
pub struct AllocateSolDistribute<'info> {
    #[account(mut, seeds = [b"donate-config", config.config_seed.as_ref()], bump = config.bump)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(mut, address = config.alpha_escrow)]
    // the Alpha bought by the buy leg, consumed here by the distribution leg
    pub alpha_escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    // ---- self-built dex pool (swap + add_liquidity CPI) ----
    #[account(mut, address = config.pool, has_one = lp_mint)]
    pub pool: Box<InterfaceAccount<'info, VenuePool>>,
    #[account(mut, address = config.alpha_mint)] // pool base_mint; mut: add_liquidity's base leg
    pub alpha_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, address = config.beta_mint)]
    // pool quote_mint; mut: the burn leg changes its supply
    pub beta_mint: Box<InterfaceAccount<'info, Mint>>,
    // Nested so they get their own try_accounts stack frames — see PoolVaults / DistVaults.
    pub vaults: PoolVaults<'info>,
    pub sinks: DistVaults<'info>,
    // Pinned to config's canonical ATA. With only `token::mint/authority` any config-owned Beta account
    // passed here — e.g. another config-owned vault — had its WHOLE balance swept into treasury by the leftover-Beta
    // leg (audit 2026-09-15 DN-01); Anchor does not reject a writable account passed twice.
    #[account(mut, associated_token::mint = beta_mint, associated_token::authority = config)]
    pub config_beta_ata: Box<InterfaceAccount<'info, TokenAccount>>, // temporary Beta intermediary for the liquidity/burn legs
    #[account(mut, associated_token::mint = lp_mint, associated_token::authority = config)]
    pub config_lp_ata: Box<InterfaceAccount<'info, TokenAccount>>, // recipient of the LP minted by add_liquidity (= the signer)
    #[account(mut)] // checked: pool.has_one = lp_mint
    pub lp_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CHECK: legacy minimum-liquidity vault; unused by CPMM, where pass pool itself.
    #[account(mut, constraint=pool.cpmm.is_some() || pool.clmm.is_some() || lp_lock.key()==Pubkey::find_program_address(&[b"lp_lock",pool.key().as_ref()],&dex_pool::ID).0 @ DonateError::BadVaultAccount)]
    pub lp_lock: UncheckedAccount<'info>,
    /// CPMM: canonical incinerator LP ATA. Legacy: existing Locker vault, retained for ABI compatibility.
    #[account(mut, address=venue::lp_sink(&pool,lp_mint.key()),
        token::mint=lp_mint, token::token_program=token_program,
        constraint=locked_lp_vault.owner == venue::lp_sink_owner(&pool) @ DonateError::BadChain)]
    pub locked_lp_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    // Retain the old account slot and Box for ABI / SBF stack compatibility.
    // CPMM callers pass the official incinerator ATA, prepared atomically before donation.
    /// CHECK: executable and bound to the typed venue owner.
    #[account(executable, constraint=dex_program.key()==pool.program() @ DonateError::BadVaultAccount)]
    pub dex_program: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    /// CHECK: Instructions sysvar — introspects whether this tx has an earlier `allocate_sol_buy` (see
    /// `require_sol_buy_before`). Bound by address constant.
    #[account(address = anchor_lang::solana_program::sysvar::instructions::ID)]
    pub instructions: UncheckedAccount<'info>,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[event]
pub struct BindEvent {
    pub user: Pubkey,
    pub referrer: Pubkey,
}

#[event]
pub struct DonateEvent {
    pub user: Pubkey,
    pub asset_is_sol: bool,
    pub amount: u64,
    pub usdt_value: u64,
    /// Accrual zeroed by this donation because it had already overflowed the *pre-donation* release
    /// budget (`pending_value > power`) and could never have been claimed. Normally 0 — non-zero only for
    /// users whose emission had saturated against their power. Mirrors `ClaimEvent.forfeited_beta`.
    pub forfeited_beta: u64,
}

#[event]
pub struct PriceDeviationChanged {
    pub config: Pubkey,
    pub authority: Pubkey,
    pub old_bps: u16,
    pub new_bps: u16,
    pub timestamp: i64,
}

#[event]
pub struct ClaimEvent {
    pub user: Pubkey,
    pub percent_bps: u16,
    pub static_beta: u64,
    pub promotion_beta: u64,
    pub level_beta: u64,
    pub beta: u64, // total Beta released this time (take_beta, pre-fee)
    pub value: u128,
    pub power_left: u128,
    pub fee: u64,            // withdrawal fee (Beta)
    pub alpha_out: u64,      // the Alpha delivered to the user after the auto-sell
    pub forfeited_beta: u64, // the static `pending` remainder forfeited to zero when power ran out
}

#[event]
pub struct AllocateEvent {
    pub amount_in: u64,
    pub min_out: u64,
}

#[event]
pub struct PriceSyncEvent {
    pub sol_usd: u64,
    pub alpha_usd: u64,
    pub beta_usd: u64,
}

#[event]
pub struct SetAlphaEvent {
    pub alpha_mint: Pubkey,
    pub pool: Pubkey,
}

#[event]
pub struct MigrationAbortStartedEvent {
    pub config: Pubkey,
    pub authority: Pubkey,
    pub expected_user_count: u32,
}

#[event]
pub struct MigrationUsersClosedEvent {
    pub config: Pubkey,
    pub authority: Pubkey,
    pub batch_count: u32,
    pub refunded_lamports: u64,
    pub closed_user_count: u32,
    pub expected_user_count: u32,
}

#[event]
pub struct MigrationAbortFinalizedEvent {
    pub config: Pubkey,
    pub authority: Pubkey,
    pub closed_user_count: u32,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[error_code]
pub enum DonateError {
    #[msg("Claim percentage must be 25, 50, 75 or 100 percent")]
    BadClaimPercent,
    #[msg("Price observation is warming, stale or circuit-broken")]
    PriceUnavailable,
    #[msg("Instruction is unavailable in the current migration phase")]
    MigrationPhase,
    #[msg("Signer is not authorized")]
    Unauthorized,
    #[msg("Split ratios invalid (must sum to 10000) or param out of range")]
    BadRatios,
    #[msg("Migration seeding is finalized; admin_seed_user is permanently disabled")]
    SeedingLocked,
    #[msg("A referrer must be bound before donating")]
    ReferrerRequired,
    #[msg("Cannot refer yourself")]
    SelfReferral,
    #[msg("Donation exceeds the per-donation USDT cap")]
    DonateCapExceeded,
    #[msg("Price is not set")]
    PriceUnset,
    #[msg("Pool reserves are empty")]
    EmptyPool,
    #[msg("Nothing to claim")]
    NothingToClaim,
    #[msg("Zero amount")]
    ZeroAmount,
    #[msg("Amount exceeds escrow balance")]
    InsufficientEscrow,
    #[msg("Account mint/owner does not match the expected token")]
    WrongMint,
    #[msg("Arithmetic overflow")]
    MathOverflow,
    #[msg("Invalid upline chain (writability/ownership/linkage/PDA/length)")]
    BadChain,
    #[msg("First claim not allowed until T+delay after the first donation")]
    ClaimTooEarly,
    #[msg("Claim cooldown has not elapsed")]
    ClaimCooldown,
    #[msg("day_window_secs must be >= 1")]
    BadTimeWindow,
    #[msg("Donation must bundle an allocate_operations_direct ix covering the configured operations leg in the same tx")]
    OpsLegMissing,
    #[msg("SOL donation must bundle an allocate_sol_donation ix covering the full amount in the same tx")]
    SolDonationLegMissing,
    #[msg("Power-changing donate/claim must be followed immediately by sync_level for the same user")]
    SyncLevelMissing,
    #[msg(
        "User stake level/power snapshot is stale; run sync_level before claiming this dividend"
    )]
    StaleLevel,
    #[msg("Price observation called before the minimum interval elapsed")]
    SyncTooSoon,
    #[msg(
        "Live pool price deviates beyond the configured cap vs the anchor (possible manipulation)"
    )]
    PoolPriceDeviation,
    #[msg("Mint decimals do not match the protocol assumption")]
    BadDecimals,
    #[msg("New authority must not be the default/zero pubkey")]
    InvalidAuthority,
    #[msg("Current power would exceed the per-user cap (300,000)")]
    PowerCapExceeded,
    #[msg("Rent refund accounts must be the donating signer followed by the System Program")]
    InvalidRentRefundAccounts,
    #[msg("Vault account does not match the one recorded in config")]
    BadVaultAccount,
    #[msg("This account's small-area (leg) bookkeeping has already been seeded")]
    AlreadySeeded,
    #[msg("This one-shot migration has already run")]
    AlreadyMigrated,
    #[msg("New FLOCK mint must have its mint and freeze authority revoked")]
    MintNotFinal,
    #[msg("LP recipient must not be the default pubkey or the incinerator")]
    BadLpRecipient,
    #[msg("Vault balances differ from the expected migration snapshot")]
    MigrationSnapshotMismatch,
    #[msg("LP locker must be the bound upgradeable-loader program with canonical ProgramData")]
    LockerNotFinal,
    #[msg("External swap changed escrow identity, token permissions or account state")]
    EscrowPermissionsChanged,
    #[msg("External swap debit differs from the authorized fixed input")]
    ExternalSwapDebit,
    #[msg("External swap output is below the required minimum")]
    ExternalSwapOutput,
    #[msg("Price deviation must be between 1 and 10000 bps")]
    BadPriceDeviation,
    #[msg("At least one UserStake account is required in a close batch")]
    EmptyCloseBatch,
    #[msg("The UserStake close batch exceeds the on-chain limit")]
    CloseBatchTooLarge,
    #[msg("A UserStake close batch contains the same account more than once")]
    DuplicateCloseAccount,
    #[msg("The supplied account is not a canonical closable UserStake")]
    InvalidCloseAccount,
    #[msg("The UserStake close count exceeds the frozen abort commitment")]
    MigrationAbortCountMismatch,
    #[msg("Not every frozen UserStake account has been closed")]
    MigrationAbortIncomplete,
    #[msg("Power-changing donate/claim instructions cannot be invoked through CPI")]
    PowerChangeCpiForbidden,

    #[msg("Treasury burn state, mint or vault does not match the canonical binding")]
    BadTreasuryBurnAccount,
    #[msg("No full treasury burn day has elapsed")]
    TreasuryBurnNotDue,
    #[msg("Settle outstanding treasury burn periods with crank_treasury_burn first")]
    TreasuryBurnCatchUpRequired,

}

#[derive(Accounts)]
#[instruction(owner: Pubkey)]
pub struct AdminSeedUser<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"donate-config", config.config_seed.as_ref()],
        bump = config.bump,
        has_one = authority @ DonateError::Unauthorized,
    )]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(
        init_if_needed,
        payer = authority,
        space = 8 + UserStake::INIT_SPACE,
        seeds = [b"donate-user", config.key().as_ref(), owner.as_ref()],
        bump
    )]
    pub user_stake: Box<Account<'info, UserStake>>,
    /// CHECK: validated for PDA, owner and imported status before incrementing once.
    #[account(mut)]
    pub referrer_stake: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct EmissionAdmin<'info> {
    pub authority: Signer<'info>,
    #[account(mut,seeds=[b"donate-config",config.config_seed.as_ref()],bump=config.bump,has_one=authority)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(address=config.pool)]
    pub pool: Box<InterfaceAccount<'info, VenuePool>>,
    #[account(address=config.treasury)]
    pub treasury: Box<InterfaceAccount<'info, TokenAccount>>,
}

/// Activation is the one-way gate before any real donation can create locked LP.
#[derive(Accounts)]
pub struct Activate<'info> {
    pub authority: Signer<'info>,
    #[account(mut,seeds=[b"donate-config",config.config_seed.as_ref()],bump=config.bump,has_one=authority)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(address=config.pool)]
    pub pool: Box<InterfaceAccount<'info, VenuePool>>,
    #[account(address=config.quote_vault)]
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: fixed program, loader owner and executable; ProgramData linkage checked in activate.
    #[account(address=LP_LOCKER_ID, owner=anchor_lang::solana_program::bpf_loader_upgradeable::ID, executable)]
    pub locker_program: UncheckedAccount<'info>,
    #[account(
        seeds=[LP_LOCKER_ID.as_ref()], bump,
        seeds::program=anchor_lang::solana_program::bpf_loader_upgradeable::ID
    )]
    pub locker_program_data: Box<Account<'info, ProgramData>>,
}

#[derive(Accounts)]
pub struct RecordPriceObservation<'info> {
    #[account(mut, seeds = [b"donate-config", config.config_seed.as_ref()], bump = config.bump)]
    pub config: Box<Account<'info, DonateConfig>>,
    pub keeper: Signer<'info>,
    #[account(mut,seeds=[b"price-observation",config.key().as_ref()],bump,has_one=config)]
    pub observation: Box<Account<'info, PriceObservation>>,
    #[account(address=config.pool)]
    pub pool: Box<InterfaceAccount<'info, VenuePool>>,
    // Raydium LIKE/WSOL pool vaults (to derive LIKE/SOL).
    #[account(address = RAYDIUM_COIN_VAULT)]
    pub amm_coin_vault: Box<InterfaceAccount<'info, TokenAccount>>, // LIKE(9dp)
    #[account(address = RAYDIUM_PC_VAULT)]
    pub amm_pc_vault: Box<InterfaceAccount<'info, TokenAccount>>, // WSOL(9dp)
    // SOL/USDC deep-pool vaults (to derive SOL/USD).
    #[account(address = SOLUSDC_WSOL_VAULT)]
    pub solusdc_wsol_vault: Box<InterfaceAccount<'info, TokenAccount>>, // WSOL(9dp)
    #[account(address = SOLUSDC_USDC_VAULT)]
    pub solusdc_usdc_vault: Box<InterfaceAccount<'info, TokenAccount>>, // USDC(6dp)
    // this pool's vaults (to derive Beta/LIKE).
    #[account(address = config.base_vault)]
    pub base_vault: Box<InterfaceAccount<'info, TokenAccount>>, // Alpha(LIKE)(9dp)
    #[account(address = config.quote_vault)]
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>, // Beta(9dp)
}

#[event]
pub struct SeedUserEvent {
    pub user: Pubkey,
    pub referrer: Pubkey,
    pub power: u128,
    pub power_granted: u128,
}

#[event]
pub struct SeedingFinalizedEvent {
    pub seeded_user_count: u32,
}

fn check_lp_recipient(key: &Pubkey) -> Result<()> {
    require!(
        *key != Pubkey::default() && *key != INCINERATOR,
        DonateError::BadLpRecipient
    );
    Ok(())
}

fn dex_quote_reserve_after_burn(pool: &AccountInfo, reserve_quote: u64) -> Result<u64> {
    let state = venue::read(pool)?;
    Ok(state.quote_reserve_after_burn(reserve_quote, Clock::get()?.unix_timestamp))
}

fn rescale_time_remainder(value: u128, old_window: i64, new_window: i64) -> Result<u128> {
    value
        .checked_mul(new_window.max(1) as u128)
        .map(|scaled| scaled / old_window.max(1) as u128)
        .ok_or_else(|| error!(DonateError::MathOverflow))
}

/// floor(a * b / denominator) and remainder, without a wide intermediate.
/// a <= denominator and b is capped by the u64 token balance.
fn claim_mul_div(a: u128, b: u128, denominator: u128) -> (u128, u128) {
    let (mut q, mut r) = (0u128, 0u128);
    for bit in (0..64).rev() {
        q *= 2;
        if r >= denominator - r { r -= denominator - r; q += 1; } else { r *= 2; }
        if (b >> bit) & 1 != 0 {
            if r >= denominator - a { r -= denominator - a; q += 1; } else { r += a; }
        }
    }
    (q, r)
}

fn combined_claim_amounts(candidates: [u128; 3], power_beta: u128, treasury: u128, percent: u16) -> Result<(u128, [u128; 3])> {
    require!(matches!(percent, 2500 | 5000 | 7500 | 10000), DonateError::BadClaimPercent);
    let sum = candidates.iter().try_fold(0u128, |s, n| s.checked_add(*n)).ok_or(DonateError::MathOverflow)?;
    let available = sum.min(power_beta).min(treasury).min(u64::MAX as u128);
    let total = available * percent as u128 / 10000;
    require!(total > 0, DonateError::NothingToClaim);
    let mut takes = [0u128; 3];
    let mut remainders = [0u128; 3];
    for i in 0..3 { (takes[i], remainders[i]) = claim_mul_div(candidates[i], total, sum); }
    let mut order = [0usize, 1, 2];
    order.sort_by(|a, b| remainders[*b].cmp(&remainders[*a]).then(a.cmp(b)));
    let leftover = total - takes.iter().sum::<u128>();
    for i in 0..leftover as usize { takes[order[i]] += 1; }
    Ok((total, takes))
}

/// Shared first-withdrawal gate; imported users start their wait at activation.
fn prepare_claim(cfg: &DonateConfig, us: &mut UserStake, now: i64) -> Result<()> {
    if us.imported && us.first_donate_ts == 0 {
        us.first_donate_ts = cfg.activation_ts;
    }
    require!(
        us.first_donate_ts > 0
            && now
                >= us
                    .first_donate_ts
                    .saturating_add(cfg.claim_first_delay_secs),
        DonateError::ClaimTooEarly
    );
    Ok(())
}
fn clear_exhausted(us: &mut UserStake, beta_usd: u128, beta_unit: u128) -> u128 {
    if us.power.saturating_mul(beta_unit) / beta_usd > 0 {
        return 0;
    }
    let forfeited = us
        .pending
        .saturating_add(us.promo_pending)
        .saturating_add(us.v_pending);
    us.pending = 0;
    us.promo_pending = 0;
    us.v_pending = 0;
    us.level_reward_remainder = 0;
    forfeited
}

impl<'info> PriceVaults<'info> {
    fn guard(&self, c: &DonateConfig) -> Result<()> {
        oracle::fresh(c, Clock::get()?.unix_timestamp)?;
        let sol = dex_sol_usd(
            self.solusdc_usdc_vault.amount as u128,
            self.solusdc_wsol_vault.amount as u128,
        )?;
        require!(self.amm_coin_vault.amount > 0, DonateError::EmptyPool);
        let like =
            self.amm_pc_vault.amount as u128 * PRICE_SCALE / self.amm_coin_vault.amount as u128;
        require!(
            oracle::within(sol as u128, c.sol_usd as u128, c.max_sync_deviation_bps)
                && oracle::within(like, c.like_sol_anchor, c.max_sync_deviation_bps),
            DonateError::PoolPriceDeviation
        );
        Ok(())
    }
}

#[cfg(test)]
mod reward_rules_20260917 {
    use super::*;
    fn cfg() -> DonateConfig {
        let mut c: DonateConfig = unsafe { std::mem::zeroed() };
        c.migration_phase = crate::migration::ACTIVE;
        c.v_dividend_enabled = true;
        c.level_weights = [14, 12, 10, 8, 6, 5, 3, 2]; // Retain legacy-config coverage.
        c.level_share_cap_bps = 2000;
        c.day_window_secs = SECONDS_PER_DAY;
        c.daily_emission_bps = 100;
        c.total_power = 100;
        c
    }
    fn user(tier: u8) -> UserStake {
        let mut u: UserStake = unsafe { std::mem::zeroed() };
        u.v_tier = tier;
        u
    }
    #[test]
    fn configurable_level_rate_preserves_legacy_and_prior_accrual() {
        let mut c = cfg();
        c.level_weights = [1, 0, 0, 0, 0, 0, 0, 0];
        c.total_v_weight[0] = 5;
        assert_eq!(level_reward_bps(&c), 60); // existing account's zeroed legacy slot
        accrue_v(&mut c, 1_000_000 * TOKEN_UNIT as u64, SECONDS_PER_DAY);
        let old_acc = c.acc_per_v[0];
        assert_eq!(old_acc / ACC_SCALE, 1_200 * TOKEN_UNIT);

        c.min_sync_interval_secs = 120;
        accrue_v(&mut c, 1_000_000 * TOKEN_UNIT as u64, 2 * SECONDS_PER_DAY);
        assert_eq!((c.acc_per_v[0] - old_acc) / ACC_SCALE, 2_400 * TOKEN_UNIT);

        c.min_sync_interval_secs = -1; // explicitly disabled without changing the switch
        accrue_v(&mut c, 1_000_000 * TOKEN_UNIT as u64, 3 * SECONDS_PER_DAY);
        assert_eq!(c.acc_per_v[0], old_acc + 2_400 * TOKEN_UNIT * ACC_SCALE);
    }
    #[test]
    fn all_tiers_counts_and_unallocated_money() {
        for heads in [0, 1, 2, 4, 5, 10] {
            let mut c = cfg();
            c.level_weights = [10, 9, 8, 7, 6, 5, 3, 2];
            assert_eq!(c.level_weights.iter().sum::<u16>(),50);
            c.total_v_weight = [heads; 8];
            accrue_v(&mut c, 1_000_000 * TOKEN_UNIT as u64, SECONDS_PER_DAY);
            for i in 0..8 {
                let mut u = user(i as u8 + 1);
                settle_v(&mut u, &VSnap::of(&c));
                let tier = 6_000 * TOKEN_UNIT * c.level_weights[i] as u128 / 50;
                let expected = if heads == 0 {
                    0
                } else {
                    (tier / heads).min(tier / 5)
                };
                assert_eq!(u.v_pending, expected, "tier {} heads {}", i + 1, heads);
                assert!(u.v_pending * heads <= tier);
            }
        }
    }
    #[test]
    fn cap_is_not_rounded_to_an_integer_divisor() {
        let mut c = cfg();
        c.level_share_cap_bps = 1700;
        c.total_v_weight = [5; 8];
        accrue_v(&mut c, 1_000_000 * TOKEN_UNIT as u64, SECONDS_PER_DAY);
        let mut u = user(1);
        settle_v(&mut u, &VSnap::of(&c));
        assert_eq!(u.v_pending, 238 * TOKEN_UNIT); // 1400 * .17, not 1400 / ceil(1/.17)
        c.total_v_weight[0] = 6;
        let before = u.v_pending;
        accrue_v(&mut c, 1_000_000 * TOKEN_UNIT as u64, (2 * SECONDS_PER_DAY));
        settle_v(&mut u, &VSnap::of(&c));
        assert_eq!(u.v_pending - before, 1400 * TOKEN_UNIT / 6);
    }
    #[test]
    fn empty_and_capped_shares_never_backfill_new_members() {
        let mut c = cfg();
        accrue_v(&mut c, 1_000_000 * TOKEN_UNIT as u64, SECONDS_PER_DAY);
        let mut u = user(0);
        v_rebucket(&mut c, &mut u, &VSnap::of(&cfg()), 1);
        accrue_v(&mut c, 1_000_000 * TOKEN_UNIT as u64, (2 * SECONDS_PER_DAY));
        settle_v(&mut u, &VSnap::of(&c));
        assert_eq!(u.v_pending, 280 * TOKEN_UNIT);
        let snap = VSnap::of(&c);
        let mut newcomer = user(0);
        v_rebucket(&mut c, &mut newcomer, &snap, 1);
        accrue_v(&mut c, 1_000_000 * TOKEN_UNIT as u64, (3 * SECONDS_PER_DAY));
        settle_v(&mut newcomer, &VSnap::of(&c));
        assert_eq!(newcomer.v_pending, 280 * TOKEN_UNIT);
    }
    #[test]
    fn current_treasury_balance_reprices_static_and_level_elapsed() {
        let mut c = cfg();
        accrue_global(&mut c, 4_000_000_000, SECONDS_PER_DAY / 2);
        assert_eq!(c.acc_reward_per_power, 20_000_000 * ACC_SCALE / 100);
        let mut c = cfg();
        c.total_v_weight = [1; 8];
        accrue_v(&mut c, 2_000_000 * TOKEN_UNIT as u64, SECONDS_PER_DAY);
        let mut u = user(1);
        settle_v(&mut u, &VSnap::of(&c));
        assert_eq!(u.v_pending, 560 * TOKEN_UNIT);
    }
    #[test]
    fn split_and_single_settlement_match_with_remainders() {
        let treasury = 1_000_003;
        let mut once = cfg();
        let mut split = cfg();
        accrue_global(&mut once, treasury, SECONDS_PER_DAY);
        accrue_global(&mut split, treasury, SECONDS_PER_DAY / 2);
        accrue_global(&mut split, treasury, SECONDS_PER_DAY);
        assert_eq!(split.acc_reward_per_power, once.acc_reward_per_power);
        assert_eq!(split.emission_remainder, once.emission_remainder);
        assert_eq!(split.reward_remainder, once.reward_remainder);

        let mut once = cfg();
        let mut split = cfg();
        once.total_v_weight = [1; 8];
        split.total_v_weight = [1; 8];
        let mut once_user = user(1);
        let mut split_user = user(1);
        accrue_v(&mut once, 1_000_000 * TOKEN_UNIT as u64, SECONDS_PER_DAY);
        settle_v(&mut once_user, &VSnap::of(&once));
        accrue_v(
            &mut split,
            1_000_000 * TOKEN_UNIT as u64,
            SECONDS_PER_DAY / 2,
        );
        settle_v(&mut split_user, &VSnap::of(&split));
        accrue_v(
            &mut split,
            1_000_000 * TOKEN_UNIT as u64,
            SECONDS_PER_DAY,
        );
        settle_v(&mut split_user, &VSnap::of(&split));
        assert_eq!(split_user.v_pending, once_user.v_pending);
        assert_eq!(split_user.level_reward_remainder, once_user.level_reward_remainder);
    }
    #[test]
    fn empty_treasury_level_period_is_not_backpaid_after_funding() {
        let mut c = cfg();
        c.total_v_weight = [1; 8];
        accrue_v(&mut c, 0, SECONDS_PER_DAY);
        accrue_v(
            &mut c,
            1_000_000 * TOKEN_UNIT as u64,
            2 * SECONDS_PER_DAY,
        );
        let mut u = user(1);
        settle_v(&mut u, &VSnap::of(&c));
        assert_eq!(u.v_pending, 280 * TOKEN_UNIT);
    }
    #[test]
    fn off_period_not_backpaid_and_repeat_settlement_noop() {
        let mut c = cfg();
        c.total_v_weight = [1; 8];
        c.v_dividend_enabled = false;
        accrue_v(&mut c, 1_000_000 * TOKEN_UNIT as u64, SECONDS_PER_DAY);
        c.v_dividend_enabled = true;
        accrue_v(&mut c, 1_000_000 * TOKEN_UNIT as u64, (2 * SECONDS_PER_DAY));
        let mut u = user(1);
        settle_v(&mut u, &VSnap::of(&c));
        assert_eq!(u.v_pending, 280 * TOKEN_UNIT);
        settle_v(&mut u, &VSnap::of(&c));
        accrue_v(&mut c, 1_000_000 * TOKEN_UNIT as u64, (2 * SECONDS_PER_DAY));
        assert_eq!(u.v_pending, 280 * TOKEN_UNIT);
    }
    #[test]
    fn imported_first_wait_and_all_buckets_expire_with_power() {
        let mut c = cfg();
        c.activation_ts = 100;
        c.claim_first_delay_secs = SECONDS_PER_DAY;
        let mut u = user(1);
        u.imported = true;
        assert!(prepare_claim(&c, &mut u, (99 + SECONDS_PER_DAY)).is_err());
        assert!(prepare_claim(&c, &mut u, (100 + SECONDS_PER_DAY)).is_ok());
        u.pending = 10;
        u.promo_pending = 20;
        u.v_pending = 30;
        assert_eq!(clear_exhausted(&mut u, 1_000_000, TOKEN_UNIT), 60);
        assert_eq!((u.pending, u.promo_pending, u.v_pending), (0, 0, 0));
    }
    #[test]
    fn level_moves_settle_old_tier_without_historical_credit() {
        let mut c = cfg();
        c.total_v_weight = [1; 8];
        let mut u = user(1);
        accrue_v(&mut c, 1_000_000 * TOKEN_UNIT as u64, SECONDS_PER_DAY);
        let snap = VSnap::of(&c);
        v_unbucket(&mut c, &mut u, &snap);
        v_rebucket(&mut c, &mut u, &snap, 2);
        assert_eq!(u.v_pending, 280 * TOKEN_UNIT);
        assert_eq!((c.total_v_weight[0], c.total_v_weight[1]), (0, 2));
        settle_v(&mut u, &snap);
        assert_eq!(u.v_pending, 280 * TOKEN_UNIT);
        accrue_v(&mut c, 1_000_000 * TOKEN_UNIT as u64, (2 * SECONDS_PER_DAY));
        let snap = VSnap::of(&c);
        v_unbucket(&mut c, &mut u, &snap);
        v_rebucket(&mut c, &mut u, &snap, 0);
        assert_eq!(u.v_pending, (280 + 240) * TOKEN_UNIT);
        accrue_v(&mut c, 1_000_000 * TOKEN_UNIT as u64, (3 * SECONDS_PER_DAY));
        settle_v(&mut u, &VSnap::of(&c));
        assert_eq!(u.v_pending, 520 * TOKEN_UNIT);
    }
    #[test]
    fn allocated_fractional_units_carry_without_capped_or_empty_shares() {
        let mut c = cfg();
        c.level_weights = [1, 0, 0, 0, 0, 0, 0, 0];
        c.total_v_weight[0] = 6;
        let mut u = user(1);
        for day in 1..=1000 {
            accrue_v(&mut c, 1001, day * SECONDS_PER_DAY);
            settle_v(&mut u, &VSnap::of(&c));
        }
        assert_eq!(u.v_pending, 1001);
        assert_eq!(u.level_reward_remainder, 0);
    }
    #[test]
    fn parameter_boundary_and_multi_day_treasury_emission() {
        let mut c = cfg();
        c.total_v_weight = [1; 8];
        let mut u = user(1);
        accrue_v(&mut c, 1_000_000 * TOKEN_UNIT as u64, SECONDS_PER_DAY);
        c.level_share_cap_bps = 1000;
        accrue_v(&mut c, 1_000_000 * TOKEN_UNIT as u64, (2 * SECONDS_PER_DAY));
        settle_v(&mut u, &VSnap::of(&c));
        assert_eq!(u.v_pending, 420 * TOKEN_UNIT);
        let mut c = cfg();
        accrue_global(&mut c, 4_000_000_000, 3 * SECONDS_PER_DAY);
        assert_eq!(c.acc_reward_per_power, 120_000_000 * ACC_SCALE / 100);
        let previous = c.acc_reward_per_power;
        accrue_global(&mut c, 0, 4 * SECONDS_PER_DAY);
        assert_eq!(c.acc_reward_per_power, previous);
    }
    #[test]
    fn extra_power_cannot_revive_forfeited_level_liability() {
        let mut u = user(1);
        u.power = 100;
        u.pending = 30;
        u.promo_pending = 40;
        u.v_pending = 50;
        assert_eq!(forfeit_pending_over_power(&mut u, 1, 1), 20);
        assert_eq!((u.pending, u.promo_pending, u.v_pending), (30, 40, 30));
        u.power += 100;
        assert_eq!(forfeit_pending_over_power(&mut u, 1, 1), 0);
        assert_eq!(u.v_pending, 30);
        u.power = 0;
        clear_exhausted(&mut u, 1, 1);
        u.power = 100;
        assert_eq!(u.pending + u.promo_pending + u.v_pending, 0);
    }
}

#[cfg(test)]
mod combined_claim_tests {
    use super::*;
    #[test]
    fn percentages_and_shared_limits() {
        for p in [2500,5000,7500,10000] {
            let (total,takes)=combined_claim_amounts([100,200,300],400,200,p).unwrap();
            assert_eq!(total,200*p as u128/10000);
            assert_eq!(takes.iter().sum::<u128>(),total);
        }
        assert!(combined_claim_amounts([1,2,3],10,10,9999).is_err());
        assert!(combined_claim_amounts([1,0,0],10,10,2500).is_err());
        assert!(combined_claim_amounts([0;3],10,10,10000).is_err());
        assert!(combined_claim_amounts([10;3],10,0,10000).is_err());
        assert!(combined_claim_amounts([10;3],0,10,10000).is_err());
        assert_eq!(combined_claim_amounts([1,1,1],2,10,10000).unwrap().1,[1,1,0]);
    }
    #[test]
    fn allocation_conserves_amounts_and_respects_each_bucket() {
        for a in 0..9 { for b in 0..9 { for c in 0..9 { for available in 1..=27 {
            for p in [2500,5000,7500,10000] {
                if let Ok((total,takes))=combined_claim_amounts([a,b,c],available,available,p) {
                    assert_eq!(takes.iter().sum::<u128>(),total);
                    for (take,candidate) in takes.iter().zip([a,b,c]) {assert!(*take<=candidate);}
                }
            }
        }}}}
    }
    #[test]
    fn large_pending_does_not_overflow_proportional_product() {
        let n=u128::MAX/3;
        let (total,takes)=combined_claim_amounts([n,n,n],u128::MAX,u64::MAX as u128,10000).unwrap();
        assert_eq!(total,u64::MAX as u128);
        assert_eq!(takes,[total/3;3]);
        assert!(combined_claim_amounts([u128::MAX,1,0],1,1,10000).is_err());
    }
}
