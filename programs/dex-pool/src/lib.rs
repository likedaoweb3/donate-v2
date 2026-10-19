//! dex-pool — a minimal constant-product (x*y=k) AMM with an admin-controlled buy/sell switch built in.
//!
//! Why it exists: when a token trades on a third-party DEX, you cannot use a transfer hook to implement a
//! "buy/sell switch" — nearly every major Solana DEX will "reject" a mint carrying an active intercepting transfer hook
//! (historical evidence in `../../../docs/dex-transfer-hook-test-results.md`). The viable
//! alternative is to "own the DEX yourself": this program puts the `buy_enabled` / `sell_enabled`
//! switch checks directly "inside" the swap instruction to enforce them.
//!
//! The currently live base token Alpha = $LIKE is a **classic SPL** (no hook), and quote = Beta is also
//! a classic SPL. Only the classic token program is accepted: transfer-fee, hook and permanent-delegate
//! extensions would invalidate the nominal-amount reserve accounting. The transfer helper is retained,
//! but supporting Token-2022 requires a separate review of all token extensions and actual received amounts.

use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    burn, mint_to, Burn, Mint, MintTo, TokenAccount, TokenInterface,
};
use spl_token_2022::onchain::invoke_transfer_checked;

pub mod legacy;
use legacy::*;

declare_id!("6HeKfShuMepwCkwqaRUGipiwM3eHT3WMmMUWiijpZUko");

/// Decimals (precision) of the pool's LP (liquidity share) mint.
const LP_DECIMALS: u8 = 9;
/// Minimum liquidity permanently locked on first deposit (in LP base units). Minted to the `lp_lock` account on first deposit and never redeemed by the program,
/// keeping `lp_supply` permanently >= this value, preventing the "1-wei first deposit + direct inflation to raise per-share value" inflation/rounding attack.
const MINIMUM_LIQUIDITY: u64 = 1_000;
/// Denominator for basis points (bps): 100% = 10_000 bps (parts per ten-thousand).
const BPS_DENOM: u16 = 10_000;

/// Trade direction: buy (side=0) = quote->base (Beta for Alpha);
/// sell (side=1) = base->quote (Alpha for Beta).
const SIDE_BUY: u8 = 0;
const SIDE_SELL: u8 = 1;

/// Seconds in a day (UTC daily rolling window: day = unix_ts / 86400).
#[cfg(not(feature = "mainnet-test-10m"))]
const SECONDS_PER_DAY: i64 = 86_400;
#[cfg(feature = "mainnet-test-10m")]
const SECONDS_PER_DAY: i64 = 600;
/// Opening-price fixed-point scale: open_price = base * PRICE_SCALE / quote (Alpha per Beta, *1e12).
const PRICE_SCALE: u128 = 1_000_000_000_000;

/// Default per-period burn ratio of the quote (Beta) vault: 100 bps = 1%.
/// (2026-09: raised from 0.2% → 1% per docs/new.md; equals MAX_BURN_BPS, the on-chain ceiling.)
const DEFAULT_BURN_BPS: u16 = 100;
/// Hard cap on the burn ratio: 100 bps = 1%; even the authority cannot set it higher (an on-chain commitment).
const MAX_BURN_BPS: u16 = 100;
/// Hard lower bound (seconds) on the burn interval. Set to 1 so integration tests can exercise the real path without faking the on-chain clock;
/// mainnet configures it to 86400 (24h) via set_burn_config.
const MIN_BURN_INTERVAL_SECS: i64 = 1;
/// Floor reserve for deflationary burn: when the quote (Beta) reserve is below this value, no more automatic burn happens (protecting small pools, avoiding
/// deflating the pool further). 5,000,000 Beta (9 decimals) = 5e6 * 1e9 base units. Hardcoded on-chain.
const BURN_MIN_QUOTE_RESERVE: u64 = 5_000_000_000_000_000;
/// Maximum number of periods settled in a single catch-up burn (prevents an extreme elapsed from making the loop too long). When the cap is hit,
/// last_burn_ts only advances by n*interval, and the remaining debt is left for the next trigger to keep settling.
const MAX_BURN_PERIODS: u64 = 3_650;

#[program]
pub mod dex_pool {
    use super::*;

    pub fn freeze_legacy_pool(ctx: Context<FreezeLegacyPool>) -> Result<()> { legacy::freeze(ctx) }
    pub fn release_locked_legacy_liquidity(ctx: Context<ReleaseLockedLegacyLiquidity>, min_base_out:u64, min_quote_out:u64) -> Result<()> {
        legacy::release(ctx,min_base_out,min_quote_out)
    }

    /// Create the pool: initialize the base/quote vaults and the LP mint, and write the config into the pool account.
    /// base = Alpha ($LIKE, classic SPL mint), quote = Beta (classic SPL mint).
    pub fn initialize_pool(
        ctx: Context<InitializePool>,
        fee_bps: u16,
        buy_enabled: bool,
        sell_enabled: bool,
    ) -> Result<()> {
        // The fee rate must be below 100% (i.e. below 10_000 bps), otherwise a trade yields no output at all and is meaningless.
        require!(fee_bps < BPS_DENOM, DexError::InvalidFee);
        // base and quote cannot be the same mint, otherwise it is not a trading pair.
        require_keys_neq!(
            ctx.accounts.base_mint.key(),
            ctx.accounts.quote_mint.key(),
            DexError::WrongMint
        );

        // Write all config into the pool PDA account at once, as the authoritative state of this pool.
        let pool = &mut ctx.accounts.pool;
        pool.authority = ctx.accounts.authority.key();
        pool.base_mint = ctx.accounts.base_mint.key();
        pool.quote_mint = ctx.accounts.quote_mint.key();
        pool.base_vault = ctx.accounts.base_vault.key();
        pool.quote_vault = ctx.accounts.quote_vault.key();
        pool.lp_mint = ctx.accounts.lp_mint.key();
        pool.buy_enabled = buy_enabled;
        pool.sell_enabled = sell_enabled;
        pool.fee_bps = fee_bps;
        // The operator whitelist defaults to zero (no bypass); the authority assigns it later via set_operator (e.g. donate's config PDA).
        pool.operator = Pubkey::default();
        // The opening price is not recorded yet (written from the day's reserves on the first swap).
        pool.open_price = 0;
        pool.open_day = 0;
        pool.day_window_secs = SECONDS_PER_DAY; // default 86400; the authority can shorten it via set_day_window
                                                // Deflationary burn: by default burns 1% of the quote vault every 24h, counted from pool creation.
        pool.burn_bps = DEFAULT_BURN_BPS;
        pool.burn_interval_secs = SECONDS_PER_DAY;
        pool.last_burn_ts = Clock::get()?.unix_timestamp;
        // Share ledger starts empty. Only authority/operator may add; only authority may remove.
        // Retain the two legacy switch bytes for existing Pool account layout compatibility.
        pool.total_lp = 0;
        pool.cumulative_ts = Clock::get()?.unix_timestamp;
        pool.add_enabled = false;
        pool.remove_enabled = true;
        // Record the pool PDA's bump, to reuse directly for PDA signing (signer seeds) later, avoiding re-derivation.
        pool.bump = ctx.bumps.pool;
        Ok(())
    }

    /// Authority/operator only: deposit base + quote, minting LP tokens to that signer.
    /// The first deposit mints LP by the geometric mean: `sqrt(base*quote)`;
    /// subsequent deposits mint by the current share ratio, taking the smaller of the two legs:
    /// `min(base*supply/reserve_base, quote*supply/reserve_quote)`,
    /// which prevents a provider from diluting others' shares via an imbalanced deposit ratio.
    pub fn add_liquidity<'info>(
        ctx: Context<'_, '_, '_, 'info, AddLiquidity<'info>>,
        base_amount: u64,
        quote_amount: u64,
        min_lp_out: u64,
    ) -> Result<()> {
        require!(!legacy::frozen(&ctx.accounts.pool), DexError::Unauthorized);
        ctx.accounts.pool.observe_reserves(
            ctx.accounts.base_vault.amount,
            ctx.accounts.quote_vault.amount,
            Clock::get()?.unix_timestamp,
        )?;

        // Both legs must be greater than 0, otherwise it cannot form a valid two-sided deposit.
        require!(base_amount > 0 && quote_amount > 0, DexError::ZeroAmount);

        // Key: read the vault balances and the current total LP supply "before any transfer happens".
        // Because the transfers below change the vault balances, the share math must use the pre-deposit snapshot.
        let user_key = ctx.accounts.user.key();
        require!(
            user_key == ctx.accounts.pool.authority
                || (ctx.accounts.pool.operator != Pubkey::default()
                    && user_key == ctx.accounts.pool.operator),
            DexError::Unauthorized
        );
        // Settle the old pool before new funds arrive: new LP must not inherit historical burn debt.
        settle_and_reload(
            &mut ctx.accounts.pool,
            &ctx.accounts.quote_mint,
            &mut ctx.accounts.quote_vault,
            &ctx.accounts.token_program,
        )?;
        let reserve_base = ctx.accounts.base_vault.amount;
        let reserve_quote = ctx.accounts.quote_vault.amount;
        // Share denominator = the pool's own ledger, never `lp_mint.supply` (see Pool::total_lp).
        let lp_supply = ctx.accounts.pool.total_lp;
        // Invariant: only the pool mints LP and outside burns only lower the supply.
        require!(
            ctx.accounts.lp_mint.supply <= lp_supply,
            DexError::LpAccountingBroken
        );

        let lp_out: u64 = if lp_supply == 0 {
            // First deposit: the pool is empty, with no existing ratio to reference,
            // use the geometric mean sqrt(base*quote) of the two token amounts as the initial LP amount.
            let product = (base_amount as u128)
                .checked_mul(quote_amount as u128)
                .ok_or(DexError::MathOverflow)?;
            let root = u64::try_from(isqrt_u128(product)).map_err(|_| DexError::MathOverflow)?;
            // Permanently lock MINIMUM_LIQUIDITY: the geometric mean minted on first deposit must be strictly greater than it,
            // the user actually receives root - MINIMUM_LIQUIDITY, and the remaining MINIMUM_LIQUIDITY is minted to the lock account later.
            root.checked_sub(MINIMUM_LIQUIDITY)
                .ok_or(DexError::ZeroLiquidity)?
        } else {
            require!(reserve_base > 0 && reserve_quote > 0, DexError::EmptyPool);
            // Subsequent deposits: for the base and quote legs, take each leg's share relative to the existing reserves,
            // convert each into its "deserved LP amount", then take the smaller.
            // Taking min ensures the deposit ratio matches the pool's existing ratio: the over-supplied leg is "wasted",
            // preventing anyone from diluting others with an imbalanced deposit. All multiplications are promoted to u128 first to avoid overflow.
            let from_base = (base_amount as u128)
                .checked_mul(lp_supply as u128)
                .ok_or(DexError::MathOverflow)?
                / (reserve_base as u128);
            let from_quote = (quote_amount as u128)
                .checked_mul(lp_supply as u128)
                .ok_or(DexError::MathOverflow)?
                / (reserve_quote as u128);
            u64::try_from(from_base.min(from_quote)).map_err(|_| DexError::MathOverflow)?
        };
        // Must mint at least 1 LP, otherwise this deposit is meaningless (the amount is too small and rounds down to 0).
        require!(lp_out > 0, DexError::ZeroLiquidity);
        // Slippage protection: the LP actually minted cannot be less than the caller's minimum acceptable value min_lp_out.
        require!(lp_out >= min_lp_out, DexError::SlippageExceeded);

        // The base (Alpha) leg may carry a hook -> forward remaining_accounts along with it;
        // the funds come from the user's wallet, so the user is the signer.
        transfer_checked_hooked(
            &ctx.accounts.token_program.to_account_info(),
            &ctx.accounts.user_base.to_account_info(),
            &ctx.accounts.base_mint.to_account_info(),
            &ctx.accounts.base_vault.to_account_info(),
            &ctx.accounts.user.to_account_info(),
            ctx.remaining_accounts,
            base_amount,
            ctx.accounts.base_mint.decimals,
            &[],
        )?;
        // The quote (Beta) leg has no hook; no extra accounts are needed (pass empty remaining), also signed by the user.
        transfer_checked_hooked(
            &ctx.accounts.token_program.to_account_info(),
            &ctx.accounts.user_quote.to_account_info(),
            &ctx.accounts.quote_mint.to_account_info(),
            &ctx.accounts.quote_vault.to_account_info(),
            &ctx.accounts.user.to_account_info(),
            &[],
            quote_amount,
            ctx.accounts.quote_mint.decimals,
            &[],
        )?;

        // Mint LP to the liquidity provider; the LP mint's mint authority belongs to the pool PDA,
        // so the mint_to CPI here must be signed with the pool's PDA (signer seeds).
        let pool_bump = [ctx.accounts.pool.bump];
        let base_mint_key = ctx.accounts.pool.base_mint;
        let quote_mint_key = ctx.accounts.pool.quote_mint;
        // The pool PDA's seeds must exactly match those used at creation: ["pool", base_mint, quote_mint, bump],
        // so the runtime lets the program sign on behalf of pool, proving this CPI is indeed authorized by pool.
        let signer_seeds: &[&[&[u8]]] = &[&[
            b"pool",
            base_mint_key.as_ref(),
            quote_mint_key.as_ref(),
            &pool_bump,
        ]];
        // With pool as the mint authority, mint lp_out LP tokens to the user's LP account.
        mint_to(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                MintTo {
                    mint: ctx.accounts.lp_mint.to_account_info(),
                    to: ctx.accounts.user_lp.to_account_info(),
                    authority: ctx.accounts.pool.to_account_info(),
                },
                signer_seeds,
            ),
            lp_out,
        )?;

        // First deposit: permanently mint MINIMUM_LIQUIDITY into the lock account lp_lock (separate from the user's share, never redeemed by the program).
        // This keeps lp_supply permanently >= MINIMUM_LIQUIDITY, so later LP share prices cannot be dragged into a roundable-to-zero range by a 1-wei first-deposit inflation.
        if lp_supply == 0 {
            mint_to(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    MintTo {
                        mint: ctx.accounts.lp_mint.to_account_info(),
                        to: ctx.accounts.lp_lock.to_account_info(),
                        authority: ctx.accounts.pool.to_account_info(),
                    },
                    signer_seeds,
                ),
                MINIMUM_LIQUIDITY,
            )?;
        }
        let minted = if lp_supply == 0 {
            lp_out
                .checked_add(MINIMUM_LIQUIDITY)
                .ok_or(DexError::MathOverflow)?
        } else {
            lp_out
        };
        let pool = &mut ctx.accounts.pool;
        pool.total_lp = lp_supply
            .checked_add(minted)
            .ok_or(DexError::MathOverflow)?;
        ctx.accounts.base_vault.reload()?;
        ctx.accounts.quote_vault.reload()?;
        ctx.accounts.pool.observe_reserves(
            ctx.accounts.base_vault.amount,
            ctx.accounts.quote_vault.amount,
            Clock::get()?.unix_timestamp,
        )?;
        Ok(())
    }

    /// Authority only: burn the authority's own LP tokens and, proportionally to their share of the total,
    /// return the corresponding amounts of base (Alpha) and quote (Beta).
    pub fn remove_liquidity<'info>(
        ctx: Context<'_, '_, '_, 'info, RemoveLiquidity<'info>>,
        lp_amount: u64,
        min_base_out: u64,
        min_quote_out: u64,
    ) -> Result<()> {
        ctx.accounts.pool.observe_reserves(
            ctx.accounts.base_vault.amount,
            ctx.accounts.quote_vault.amount,
            Clock::get()?.unix_timestamp,
        )?;

        // The amount of LP to burn must be positive.
        require!(lp_amount > 0, DexError::ZeroAmount);
        require_keys_eq!(
            ctx.accounts.user.key(),
            ctx.accounts.pool.authority,
            DexError::Unauthorized
        );
        // Share denominator = the pool's own ledger, never `lp_mint.supply` (see Pool::total_lp).
        let lp_supply = ctx.accounts.pool.total_lp;
        require!(
            ctx.accounts.lp_mint.supply <= lp_supply,
            DexError::LpAccountingBroken
        );
        // The pool must have LP issued, otherwise the ratio denominator is 0 and nothing can be computed.
        require!(lp_supply > 0, DexError::ZeroLiquidity);
        // An LP exit owes the same pending burn as a swap; calculate its claim after settlement.
        settle_and_reload(
            &mut ctx.accounts.pool,
            &ctx.accounts.quote_mint,
            &mut ctx.accounts.quote_vault,
            &ctx.accounts.token_program,
        )?;

        // Read the current reserves on both sides (the pre-add/remove snapshot), promoted to u128 for proportional distribution.
        let reserve_base = ctx.accounts.base_vault.amount as u128;
        let reserve_quote = ctx.accounts.quote_vault.amount as u128;

        // By the ratio lp_amount / lp_supply, compute how much base and quote to return:
        //   base_out  = reserve_base  * lp_amount / lp_supply
        //   quote_out = reserve_quote * lp_amount / lp_supply
        // Multiply before dividing and compute in u128 to avoid precision loss and overflow.
        let base_out = u64::try_from(
            reserve_base
                .checked_mul(lp_amount as u128)
                .ok_or(DexError::MathOverflow)?
                / lp_supply as u128,
        )
        .map_err(|_| DexError::MathOverflow)?;
        let quote_out = u64::try_from(
            reserve_quote
                .checked_mul(lp_amount as u128)
                .ok_or(DexError::MathOverflow)?
                / lp_supply as u128,
        )
        .map_err(|_| DexError::MathOverflow)?;
        // Slippage protection: the amounts actually returned on both sides cannot fall below the caller's lower bounds.
        require!(base_out >= min_base_out, DexError::SlippageExceeded);
        require!(quote_out >= min_quote_out, DexError::SlippageExceeded);
        // One side may legitimately round to zero in a highly imbalanced pool. Its explicit
        // min_out still applies; never trap the other asset merely because this side is dust.
        require!(base_out > 0 || quote_out > 0, DexError::ZeroAmount);

        // Burn the LP first (the LP tokens belong to the user, so the user directly authorizes the burn as the signer).
        // Burn before refunding, to avoid "refund first, then burn fails" causing assets to be over-withdrawn.
        burn(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Burn {
                    mint: ctx.accounts.lp_mint.to_account_info(),
                    from: ctx.accounts.user_lp.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            lp_amount,
        )?;
        // The MINIMUM_LIQUIDITY lock is never redeemable, so the ledger can never drop below it.
        let remaining = lp_supply
            .checked_sub(lp_amount)
            .ok_or(DexError::MathOverflow)?;
        require!(remaining >= MINIMUM_LIQUIDITY, DexError::ZeroLiquidity);
        ctx.accounts.pool.total_lp = remaining;

        // Funds are refunded from the vaults to the user; the payer is the vault (whose authority is the pool PDA),
        // so both refund legs must be signed with the pool's PDA (seeds matching those at creation).
        let pool_bump = [ctx.accounts.pool.bump];
        let base_mint_key = ctx.accounts.pool.base_mint;
        let quote_mint_key = ctx.accounts.pool.quote_mint;
        let signer_seeds: &[&[&[u8]]] = &[&[
            b"pool",
            base_mint_key.as_ref(),
            quote_mint_key.as_ref(),
            &pool_bump,
        ]];
        // The base (Alpha, hook-aware) leg: transfer from the base_vault to the user's user_base; signed by the pool PDA.
        // Because it may carry a hook, forward remaining_accounts along with it.
        transfer_checked_hooked(
            &ctx.accounts.token_program.to_account_info(),
            &ctx.accounts.base_vault.to_account_info(),
            &ctx.accounts.base_mint.to_account_info(),
            &ctx.accounts.user_base.to_account_info(),
            &ctx.accounts.pool.to_account_info(),
            ctx.remaining_accounts,
            base_out,
            ctx.accounts.base_mint.decimals,
            signer_seeds,
        )?;
        // The quote (Beta, no hook) leg: transfer from the quote_vault to the user's user_quote; also signed by the pool PDA.
        transfer_checked_hooked(
            &ctx.accounts.token_program.to_account_info(),
            &ctx.accounts.quote_vault.to_account_info(),
            &ctx.accounts.quote_mint.to_account_info(),
            &ctx.accounts.user_quote.to_account_info(),
            &ctx.accounts.pool.to_account_info(),
            &[],
            quote_out,
            ctx.accounts.quote_mint.decimals,
            signer_seeds,
        )?;
        ctx.accounts.base_vault.reload()?;
        ctx.accounts.quote_vault.reload()?;
        ctx.accounts.pool.observe_reserves(
            ctx.accounts.base_vault.amount,
            ctx.accounts.quote_vault.amount,
            Clock::get()?.unix_timestamp,
        )?;
        Ok(())
    }

    /// Swap, enforcing the buy/sell switch inside the instruction.
    /// `side`: 0 = buy (quote->base, i.e. Beta for Alpha, requires buy_enabled to be true);
    ///         1 = sell (base->quote, i.e. Alpha for Beta, requires sell_enabled to be true).
    pub fn swap<'info>(
        ctx: Context<'_, '_, '_, 'info, Swap<'info>>,
        amount_in: u64,
        min_amount_out: u64,
        side: u8,
    ) -> Result<()> {
        require!(!legacy::frozen(&ctx.accounts.pool), DexError::Unauthorized);
        ctx.accounts.pool.observe_reserves(
            ctx.accounts.base_vault.amount,
            ctx.accounts.quote_vault.amount,
            Clock::get()?.unix_timestamp,
        )?;

        // The input amount must be positive.
        require!(amount_in > 0, DexError::ZeroAmount);

        // Read the pool's scalar fields into locals, to avoid conflicting with the mutable borrow of "record opening price" below.
        let buy_enabled = ctx.accounts.pool.buy_enabled;
        let sell_enabled = ctx.accounts.pool.sell_enabled;
        let fee_bps = ctx.accounts.pool.fee_bps;
        let pool_bump = ctx.accounts.pool.bump;
        let base_mint_key = ctx.accounts.pool.base_mint;
        let quote_mint_key = ctx.accounts.pool.quote_mint;

        // Operator whitelist: an authorized operator (e.g. donate's config PDA) can bypass the buy/sell switch,
        // letting program-internal CPIs (claim auto-sell / allocate) not be restricted by the switch. user must be a signer,
        // and this PDA can only be signed by the donate program via CPI, so external users cannot impersonate it to bypass; setting it to zero disables the whitelist.
        let operator = ctx.accounts.pool.operator;
        let is_operator = operator != Pubkey::default() && ctx.accounts.user.key() == operator;

        // Per-address whitelist: a trader whose `[b"whitelist", pool, user]` PDA exists (created by `add_whitelist`)
        // may bypass the buy/sell switch just like the operator, and can freely buy AND sell. The PDA is passed as an
        // OPTIONAL account in `remaining_accounts` (kept out of the typed `Swap` struct to protect the 4 KB SBF stack
        // frame). We accept it only when: it is the exact PDA for (pool, user), it is owned by THIS program, and it is
        // an initialized `Whitelist` account (correct discriminator + fields). A wrong/foreign/uninitialized account is
        // silently ignored (no bypass), never trusted — so it cannot be used to spoof authorization.
        let pool_key = ctx.accounts.pool.key();
        let user_key = ctx.accounts.user.key();
        let (expected_wl, _) = Pubkey::find_program_address(
            &[b"whitelist", pool_key.as_ref(), user_key.as_ref()],
            &crate::ID,
        );
        let is_whitelisted = ctx.remaining_accounts.iter().any(|ai| {
            ai.key() == expected_wl && ai.owner == &crate::ID && {
                let data = ai.try_borrow_data();
                match data {
                    Ok(d) => {
                        d.len() >= 8 + Whitelist::INIT_SPACE && &d[..8] == Whitelist::DISCRIMINATOR
                    }
                    Err(_) => false,
                }
            }
        });
        let can_bypass = is_operator || is_whitelisted;

        // This is the core of the "buy/sell switch": based on the direction, check whether the corresponding switch is on,
        // and when off, reject the trade with an error (except for the operator or a whitelisted trader); a side other than 0/1 is treated as an invalid direction.
        match side {
            SIDE_BUY => require!(buy_enabled || can_bypass, DexError::BuyDisabled),
            SIDE_SELL => require!(sell_enabled || can_bypass, DexError::SellDisabled),
            _ => return err!(DexError::InvalidSide),
        }

        // Lazily trigger the deflationary burn: if at least one full burn period has elapsed since the last settlement, burn the due Beta first,
        // then read reserves for pricing / record the opening price — ensuring this trade and the day's opening price both use the "post-burn" reserves.
        let now = Clock::get()?.unix_timestamp;
        {
            let pool_ai = ctx.accounts.pool.to_account_info();
            let quote_mint_ai = ctx.accounts.quote_mint.to_account_info();
            let quote_vault_ai = ctx.accounts.quote_vault.to_account_info();
            let token_program_ai = ctx.accounts.token_program.to_account_info();
            let vault_amount = ctx.accounts.quote_vault.amount;
            let (_, burned) = settle_burn(
                &mut ctx.accounts.pool,
                &pool_ai,
                &quote_mint_ai,
                &quote_vault_ai,
                vault_amount,
                &token_program_ai,
                now,
            )?;
            if burned > 0 {
                // The vault balance was changed by the burn CPI; refresh the cache before using it for pricing.
                ctx.accounts.quote_vault.reload()?;
            }
        }

        // Read both reserves and, by direction, determine the "input-side reserve reserve_in" and "output-side reserve reserve_out":
        // on a buy the input is quote and the output is base; on a sell it is exactly the reverse.
        let reserve_base = ctx.accounts.base_vault.amount;
        let reserve_quote = ctx.accounts.quote_vault.amount;
        let (reserve_in, reserve_out) = if side == SIDE_BUY {
            (reserve_quote, reserve_base)
        } else {
            (reserve_base, reserve_quote)
        };
        // Both reserves must be non-empty, otherwise pricing is impossible.
        require!(reserve_in > 0 && reserve_out > 0, DexError::EmptyPool);

        // Record the UTC daily opening price: on the day's first swap, use the "pre-trade" reserve ratio
        // (base/quote = Alpha per Beta, *1e12 fixed-point) as the day's "first traded price", for donate's
        // anti-crash fee tier and the front-end's change percentage to read (reserve_quote>0 is guaranteed by the non-empty check above).
        let day = (now / ctx.accounts.pool.day_window_secs.max(1)) as u32;
        if ctx.accounts.pool.open_price == 0 || ctx.accounts.pool.open_day != day {
            let p = &mut ctx.accounts.pool;
            p.open_day = day;
            p.open_price = (reserve_base as u128)
                .checked_mul(PRICE_SCALE)
                .ok_or(DexError::MathOverflow)?
                / reserve_quote as u128;
        }

        // Constant-product pricing (x*y=k) with an "input-side fee".
        // First deduct the fee from the input: in_after_fee = amount_in * (10000 - fee_bps) / 10000,
        // i.e. only the post-fee portion actually participates in the trade and moves the price.
        let in_after_fee = (amount_in as u128)
            .checked_mul((BPS_DENOM - fee_bps) as u128)
            .ok_or(DexError::MathOverflow)?
            / BPS_DENOM as u128;
        // The output amount derived from the constant-product formula:
        //   amount_out = reserve_out * in_after_fee / (reserve_in + in_after_fee)
        // Both numerator and denominator are computed in u128 to prevent overflow.
        let numerator = (reserve_out as u128)
            .checked_mul(in_after_fee)
            .ok_or(DexError::MathOverflow)?;
        let denominator = (reserve_in as u128)
            .checked_add(in_after_fee)
            .ok_or(DexError::MathOverflow)?;
        require!(denominator > 0, DexError::EmptyPool);
        let amount_out =
            u64::try_from(numerator / denominator).map_err(|_| DexError::MathOverflow)?;
        // Never accept a payment that rounds down to no output, even if the caller used min_out=0.
        require!(amount_out > 0, DexError::ZeroAmount);
        // Slippage protection: the actual output must not fall below the caller's minimum acceptable output.
        require!(amount_out >= min_amount_out, DexError::SlippageExceeded);
        // The output must be strictly less than the output-side reserve, otherwise it would drain the vault (should not happen under constant product; kept as a backstop).
        require!(
            (amount_out as u128) < reserve_out as u128,
            DexError::InsufficientLiquidity
        );

        // Pre-trade constant product, to be re-checked against the settled vault balances below.
        // Taken after the lazy burn has already been settled and the vaults reloaded, so the burn
        // (a deliberate one-sided reduction of k) is outside the compared window and cannot trip it.
        let k_before = (reserve_base as u128)
            .checked_mul(reserve_quote as u128)
            .ok_or(DexError::MathOverflow)?;

        // The swap's "transfer-out" leg is paid by the vault and needs the pool PDA to sign; prepare signer seeds matching those at creation.
        let pool_bump_arr = [pool_bump];
        let signer_seeds: &[&[&[u8]]] = &[&[
            b"pool",
            base_mint_key.as_ref(),
            quote_mint_key.as_ref(),
            &pool_bump_arr,
        ]];
        let base_decimals = ctx.accounts.base_mint.decimals;
        let quote_decimals = ctx.accounts.quote_mint.decimals;

        if side == SIDE_BUY {
            // Buy: first transfer quote (Beta) from the user into the vault (user-signed, no hook, pass empty remaining).
            transfer_checked_hooked(
                &ctx.accounts.token_program.to_account_info(),
                &ctx.accounts.user_quote.to_account_info(),
                &ctx.accounts.quote_mint.to_account_info(),
                &ctx.accounts.quote_vault.to_account_info(),
                &ctx.accounts.user.to_account_info(),
                &[],
                amount_in,
                quote_decimals,
                &[],
            )?;
            // Then transfer base (Alpha) from the vault to the user (pool PDA-signed, hook-aware -> the destination account is user_base,
            // and forward remaining_accounts to the hook).
            transfer_checked_hooked(
                &ctx.accounts.token_program.to_account_info(),
                &ctx.accounts.base_vault.to_account_info(),
                &ctx.accounts.base_mint.to_account_info(),
                &ctx.accounts.user_base.to_account_info(),
                &ctx.accounts.pool.to_account_info(),
                ctx.remaining_accounts,
                amount_out,
                base_decimals,
                signer_seeds,
            )?;
        } else {
            // Sell: first transfer base (Alpha) from the user into the vault (user-signed, hook-aware -> the destination account is base_vault,
            // and forward remaining_accounts to the hook).
            transfer_checked_hooked(
                &ctx.accounts.token_program.to_account_info(),
                &ctx.accounts.user_base.to_account_info(),
                &ctx.accounts.base_mint.to_account_info(),
                &ctx.accounts.base_vault.to_account_info(),
                &ctx.accounts.user.to_account_info(),
                ctx.remaining_accounts,
                amount_in,
                base_decimals,
                &[],
            )?;
            // Then transfer quote (Beta) from the vault to the user (pool PDA-signed, no hook).
            transfer_checked_hooked(
                &ctx.accounts.token_program.to_account_info(),
                &ctx.accounts.quote_vault.to_account_info(),
                &ctx.accounts.quote_mint.to_account_info(),
                &ctx.accounts.user_quote.to_account_info(),
                &ctx.accounts.pool.to_account_info(),
                &[],
                amount_out,
                quote_decimals,
                signer_seeds,
            )?;
        }
        ctx.accounts.base_vault.reload()?;
        ctx.accounts.quote_vault.reload()?;
        // Defense in depth (mirrors Raydium's `require_gte!(constant_after, constant_before)`):
        // a swap must never leave the pool with a smaller constant product than it started with.
        // Checked against the *actual* post-transfer vault balances, not the computed amounts, so a
        // leg that moved less than requested is caught too. Unreachable today — amount_out floors
        // and the fee stays in the vault, so k strictly grows — which is the point: if the pricing
        // math is ever changed and gets this wrong, the trade aborts instead of silently draining.
        let k_after = (ctx.accounts.base_vault.amount as u128)
            .checked_mul(ctx.accounts.quote_vault.amount as u128)
            .ok_or(DexError::MathOverflow)?;
        require!(k_after >= k_before, DexError::InvariantViolated);
        ctx.accounts.pool.observe_reserves(
            ctx.accounts.base_vault.amount,
            ctx.accounts.quote_vault.amount,
            Clock::get()?.unix_timestamp,
        )?;
        Ok(())
    }

    /// Authority only: toggle the buy/sell switch at runtime.
    /// The `has_one = authority` account constraint ensures only the pool's authority can call it.
    pub fn set_switches(
        ctx: Context<SetSwitches>,
        buy_enabled: bool,
        sell_enabled: bool,
    ) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        pool.buy_enabled = buy_enabled;
        pool.sell_enabled = sell_enabled;
        Ok(())
    }

    /// Permissionless checkpoint for direct token transfers to the vaults. Accrue the previous
    /// observation up to now, then start observing the actual balances; never backdate a donation.
    /// This also lets an oracle recover while trading/liquidity operations are paused.
    pub fn sync_reserves(ctx: Context<SyncReserves>) -> Result<()> {
        ctx.accounts.pool.observe_reserves(
            ctx.accounts.base_vault.amount,
            ctx.accounts.quote_vault.amount,
            Clock::get()?.unix_timestamp,
        )
    }

    /// Authority only: set the number of seconds in the opening-price "day" (default 86400; can be shortened locally to test change percentages / fee tiers).
    /// `has_one = authority` ensures only the pool's authority can call it; the window must be >= 1.
    pub fn set_day_window(ctx: Context<SetDayWindow>, day_window_secs: i64) -> Result<()> {
        require!(day_window_secs >= 1, DexError::BadTimeWindow);
        ctx.accounts.pool.day_window_secs = day_window_secs;
        // A baseline from a different bucket duration cannot describe the new window.
        ctx.accounts.pool.open_price = 0;
        Ok(())
    }

    /// Authority only: set the operator whitelist (a single pubkey) that can bypass the buy/sell switch.
    /// Used to let program-internal CPIs (e.g. claim auto-sell / allocate initiated by donate's config PDA) not be restricted by the switch;
    /// setting it to `Pubkey::default()` (all zeros) disables the whitelist. `has_one = authority` ensures only the admin can change it.
    pub fn set_operator(ctx: Context<SetOperator>, operator: Pubkey) -> Result<()> {
        ctx.accounts.pool.operator = operator;
        Ok(())
    }

    /// (2026-09) Authority only: add `trader` to the pool's trade whitelist. Unlike the single
    /// `operator` (one program PDA that bypasses the switch for internal CPIs), the whitelist holds an
    /// unbounded set of ordinary addresses that may freely buy **and** sell even while both switches are
    /// off. Each entry is its own PDA `[b"whitelist", pool, trader]`; its mere existence = allowed. `init`
    /// (not `init_if_needed`) so re-adding an existing entry is a no-op error, not a silent overwrite.
    pub fn add_whitelist(ctx: Context<AddWhitelist>, trader: Pubkey) -> Result<()> {
        let w = &mut ctx.accounts.whitelist;
        w.pool = ctx.accounts.pool.key();
        w.trader = trader;
        w.bump = ctx.bumps.whitelist;
        emit!(WhitelistEvent {
            pool: w.pool,
            trader,
            allowed: true,
        });
        Ok(())
    }

    /// (2026-09) Authority only: remove `trader` from the whitelist by closing its PDA (rent refunded to
    /// the authority). Idempotent from the caller's view: a missing entry simply fails the seeds/`close`.
    pub fn remove_whitelist(ctx: Context<RemoveWhitelist>, trader: Pubkey) -> Result<()> {
        emit!(WhitelistEvent {
            pool: ctx.accounts.pool.key(),
            trader,
            allowed: false,
        });
        Ok(())
    }

    // (2026-09-17) `lock_lp` was REMOVED from this program. Permanent LP locking now lives in the
    // standalone `lp-locker` program, which is deployed with its upgrade authority revoked.
    //
    // Why it had to move: this program deliberately keeps its upgrade authority so real bugs stay
    // fixable. That made an in-program "permanent lock" only as permanent as that key — whoever
    // holds it could upgrade dex-pool and add an instruction moving the locked LP out. LP locked in
    // lp-locker sits under a PDA that program owns, which no dex-pool upgrade can reach.
    //
    // The `lp_lock` PDA itself stays: `MINIMUM_LIQUIDITY` is still minted into it on the first
    // deposit, and it is still never signed out of. What is gone is only the external entry point
    // that let callers add to it.

    /// Permissionless crank (anyone can call): settle the due deflationary burn — for each full
    /// `burn_interval_secs` period elapsed since the last settlement, burn `burn_bps` of the quote (Beta) vault balance (compounded: missing n
    /// periods catches up in one burn of `1 - (1-bps)^n`). Reports BurnNotDue when no full period has elapsed.
    /// Shares the same settlement logic as swap's lazy trigger; the two are idempotent and complementary: whichever arrives first burns the same single amount.
    /// At or below BURN_MIN_QUOTE_RESERVE (5 million), only advance the timeline.
    pub fn crank_burn(ctx: Context<CrankBurn>) -> Result<()> {
        ctx.accounts.pool.accumulate(Clock::get()?.unix_timestamp)?;

        let now = Clock::get()?.unix_timestamp;
        let pool_ai = ctx.accounts.pool.to_account_info();
        let quote_mint_ai = ctx.accounts.quote_mint.to_account_info();
        let quote_vault_ai = ctx.accounts.quote_vault.to_account_info();
        let token_program_ai = ctx.accounts.token_program.to_account_info();
        let vault_amount = ctx.accounts.quote_vault.amount;
        let (periods, _) = settle_burn(
            &mut ctx.accounts.pool,
            &pool_ai,
            &quote_mint_ai,
            &quote_vault_ai,
            vault_amount,
            &token_program_ai,
            now,
        )?;
        // When cranked on its own, if no full period has elapsed, report an explicit error (swap's lazy-trigger path silently skips instead).
        require!(periods > 0, DexError::BurnNotDue);
        ctx.accounts.quote_vault.reload()?;
        let base = ctx.accounts.pool.observed_base;
        ctx.accounts.pool.observe_reserves(
            base,
            ctx.accounts.quote_vault.amount,
            Clock::get()?.unix_timestamp,
        )?;
        Ok(())
    }

    /// Authority only: set the deflationary-burn parameters. bps has a hard cap of MAX_BURN_BPS (1%),
    /// the interval has a hard floor of MIN_BURN_INTERVAL_SECS; bps = 0 disables the burn.
    /// Every change resets last_burn_ts to now — preventing "shortening the interval" from letting the elapsed accumulated over the old interval
    /// be converted under the new interval into many catch-up periods, instantly burning far more than expected.
    pub fn set_burn_config(
        ctx: Context<SetBurnConfig>,
        burn_bps: u16,
        burn_interval_secs: i64,
    ) -> Result<()> {
        require!(burn_bps <= MAX_BURN_BPS, DexError::InvalidBurnConfig);
        require!(
            burn_interval_secs >= MIN_BURN_INTERVAL_SECS,
            DexError::InvalidBurnConfig
        );
        let pool = &mut ctx.accounts.pool;
        pool.burn_bps = burn_bps;
        pool.burn_interval_secs = burn_interval_secs;
        pool.last_burn_ts = Clock::get()?.unix_timestamp;
        Ok(())
    }

    /// Authority only: **renounce** — set pool.authority to the all-zero address, irreversibly.
    /// Admin settings and trade whitelist mutations become permanently uncallable.
    /// No authority remains to withdraw LP; only a nonzero operator can continue adding liquidity.
    pub fn renounce_authority(ctx: Context<RenounceAuthority>) -> Result<()> {
        ctx.accounts.pool.authority = Pubkey::default();
        Ok(())
    }

    /// Authority only: **transfer** admin to `new_authority`. Reuses the same has_one gate as renounce.
    /// Rejects the all-zero address so a transfer can never silently become an irreversible renounce
    /// (use renounce_authority explicitly for that).
    pub fn transfer_authority(
        ctx: Context<RenounceAuthority>,
        new_authority: Pubkey,
    ) -> Result<()> {
        require_keys_neq!(new_authority, Pubkey::default(), DexError::InvalidAuthority);
        ctx.accounts.pool.authority = new_authority;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// State: the on-chain persistent pool account
// ---------------------------------------------------------------------------

/// Pool — all on-chain state of a single pool; it is itself a PDA.
/// `#[derive(InitSpace)]` lets Anchor automatically compute the space this struct needs, for allocation at init.
#[account]
#[derive(InitSpace)]
pub struct Pool {
    pub cumulative_ts: i64,
    /// Price-time counter modulo 2^128. Consumers must use wrapping subtraction over a bounded
    /// window (the maximum possible raw price wraps no faster than once per 213 days).
    pub cumulative_price: u128,
    pub cumulative_quote: u128,
    pub observed_base: u64,
    pub observed_quote: u64,
    pub authority: Pubkey, // admin pubkey: the only account allowed to toggle the buy/sell switch
    pub base_mint: Pubkey, // base = Alpha ($LIKE, classic SPL mint)
    pub quote_mint: Pubkey, // quote = Beta (classic SPL mint)
    pub base_vault: Pubkey, // base vault (Token account, authority is the pool PDA)
    pub quote_vault: Pubkey, // quote vault (Token account, authority is the pool PDA)
    pub lp_mint: Pubkey,   // LP share mint (mint authority is the pool PDA)
    pub buy_enabled: bool, // buy switch: when false, side=0 swaps are forbidden
    pub sell_enabled: bool, // sell switch: when false, side=1 swaps are forbidden
    pub fee_bps: u16,      // fee rate in basis points (parts per ten-thousand); must be < 10000
    pub open_price: u128, // day's opening price: base*1e12/quote (Alpha per Beta, *1e12); 0 = not recorded for the day
    pub open_day: u32, // the UTC day index the opening price belongs to (unix_ts / day_window_secs)
    pub day_window_secs: i64, // seconds in the opening-price "day" (default 86400; can be shortened locally to test change percentages / fee tiers)
    pub operator: Pubkey, // whitelist: when swap's user equals this pubkey, bypass the buy/sell switch (default all zeros = disabled)
    pub burn_bps: u16, // per-period burn ratio of the quote vault (bps; 0 = disabled; capped at MAX_BURN_BPS)
    pub burn_interval_secs: i64, // burn interval in seconds (default 86400 = 24h; floored at MIN_BURN_INTERVAL_SECS)
    pub last_burn_ts: i64, // the timestamp burns are settled up to (advances by whole periods, with the remainder still accumulating)
    pub bump: u8,          // the pool PDA's bump, cached for reuse in PDA signing
    // ---- 2026-09 fix (tail-appended; pools created before this upgrade no longer deserialize) ----
    /// The pool's own LP share ledger (includes the MINIMUM_LIQUIDITY lock). Only this program's
    /// add/remove move it. It is the share denominator instead of `lp_mint.supply`, because SPL Token
    /// lets anyone burn LP held by an incinerator / System-Program-owned account without a signature,
    /// and such an outside burn used to shrink the denominator and drain the vaults (2026-09-15).
    /// With this ledger an outside burn only strands its own share inside the pool.
    pub total_lp: u64,
    /// Legacy layout placeholders. These bytes never grant or restrict liquidity permissions.
    /// Kept at the same offsets so existing Pool accounts need no reallocation.
    pub add_enabled: bool,
    pub remove_enabled: bool,
}

impl Pool {
    /// Exact reserve used by the next swap/add/remove after its lazy burn settlement.
    /// CPI callers can share this calculation instead of spending slippage on a scheduled burn.
    pub fn quote_reserve_after_burn(&self, reserve: u64, now: i64) -> u64 {
        let (_, amount) = compute_burn(
            reserve,
            self.burn_bps,
            now.saturating_sub(self.last_burn_ts),
            self.burn_interval_secs,
        );
        reserve - amount
    }

    pub fn quote_integral(&self, now: i64) -> Result<u128> {
        require!(now >= self.cumulative_ts, DexError::MathOverflow);
        let elapsed = now
            .checked_sub(self.cumulative_ts)
            .ok_or(DexError::MathOverflow)? as u128;
        self.cumulative_quote
            .checked_add(
                (self.observed_quote as u128)
                    .checked_mul(elapsed)
                    .ok_or(DexError::MathOverflow)?,
            )
            .ok_or_else(|| error!(DexError::MathOverflow))
    }
    pub fn price_integral(&self, now: i64) -> Result<u128> {
        require!(now >= self.cumulative_ts, DexError::MathOverflow);
        let elapsed = now
            .checked_sub(self.cumulative_ts)
            .ok_or(DexError::MathOverflow)? as u128;
        let price = if self.observed_quote == 0 {
            0
        } else {
            self.observed_base as u128 * PRICE_SCALE / self.observed_quote as u128
        };
        // A checked lifetime accumulator would eventually disable swap, LP withdrawals and even
        // sync. Modular counters retain exact short-window deltas across rollover instead.
        Ok(self
            .cumulative_price
            .wrapping_add(price.wrapping_mul(elapsed)))
    }
    pub fn accumulate(&mut self, now: i64) -> Result<()> {
        self.cumulative_quote = self.quote_integral(now)?;
        self.cumulative_price = self.price_integral(now)?;
        self.cumulative_ts = now;
        Ok(())
    }
    pub fn observe_reserves(&mut self, base: u64, quote: u64, now: i64) -> Result<()> {
        self.accumulate(now)?;
        self.observed_base = base;
        self.observed_quote = quote;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Accounts: the set of accounts and constraints each instruction needs
// ---------------------------------------------------------------------------

/// Accounts for the initialize_pool instruction: create the pool, the two vaults, and the LP mint.
#[derive(Accounts)]
pub struct InitializePool<'info> {
    // Every account is boxed: once Pool grew (total_lp + switches) the unboxed try_accounts frame
    // measured 4104 > 4096 bytes under platform-tools v1.50.
    // The payer and initial admin; needs mut because rent is deducted from its account to create the new accounts.
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(constraint = program.programdata_address()? == Some(program_data.key()) @ DexError::Unauthorized)]
    pub program: Program<'info, crate::program::DexPool>,
    #[account(constraint = program_data.upgrade_authority_address == Some(authority.key()) @ DexError::Unauthorized)]
    pub program_data: Box<Account<'info, ProgramData>>,

    #[account(owner = anchor_spl::token::ID @ DexError::UnsupportedTokenProgram)]
    pub base_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(owner = anchor_spl::token::ID @ DexError::UnsupportedTokenProgram)]
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,

    // Note: the `pool` field must be declared "before" the vault/lp_mint that reference it —
    // because they reference pool in `seeds` and `*::authority`. Anchor initializes accounts one by one
    // in field "declaration order", so `pool` must already exist when those vaults/LP mint are created.
    // The pool PDA, seeds ["pool", base_mint, quote_mint]; created and allocated on the spot via init.
    #[account(
        init,
        payer = authority,
        space = 8 + Pool::INIT_SPACE,
        seeds = [b"pool", base_mint.key().as_ref(), quote_mint.key().as_ref()],
        bump
    )]
    pub pool: Box<Account<'info, Pool>>,

    // base vault (PDA Token account): mint is base_mint, authority is pool (so only pool can move its funds).
    #[account(
        init,
        payer = authority,
        seeds = [b"base_vault", pool.key().as_ref()],
        bump,
        token::mint = base_mint,
        token::authority = pool,
        token::token_program = token_program,
    )]
    pub base_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    // quote vault (PDA Token account): mint is quote_mint, authority is also pool.
    #[account(
        init,
        payer = authority,
        seeds = [b"quote_vault", pool.key().as_ref()],
        bump,
        token::mint = quote_mint,
        token::authority = pool,
        token::token_program = token_program,
    )]
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    // LP share mint (PDA): decimals fixed at LP_DECIMALS, mint authority is pool,
    // so LP can only be minted/managed when this program signs on behalf of pool.
    #[account(
        init,
        payer = authority,
        seeds = [b"lp_mint", pool.key().as_ref()],
        bump,
        mint::decimals = LP_DECIMALS,
        mint::authority = pool,
        mint::token_program = token_program,
    )]
    pub lp_mint: Box<InterfaceAccount<'info, Mint>>,

    // LP lock account (PDA Token account, mint = lp_mint, authority = pool): on first deposit, MINIMUM_LIQUIDITY is permanently minted into it,
    // and the program never transfers out of / burns it -> lp_supply permanently >= MINIMUM_LIQUIDITY,
    // preventing the first-deposit "1-wei + inflation" attack that raises per-share value. It must be declared after lp_mint (which it references).
    #[account(
        init,
        payer = authority,
        seeds = [b"lp_lock", pool.key().as_ref()],
        bump,
        token::mint = lp_mint,
        token::authority = pool,
        token::token_program = token_program,
    )]
    pub lp_lock: Box<InterfaceAccount<'info, TokenAccount>>,

    // Restrict the retained interface type to classic SPL; extension semantics are not supported.
    #[account(address = anchor_spl::token::ID @ DexError::UnsupportedTokenProgram)]
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

// Note: all account fields here are wrapped in `Box` to keep Anchor's generated `try_accounts`
// within the SBF 4 KB stack-frame limit — if this many Token-2022 `InterfaceAccount`s
// were deserialized directly on the stack, they would blow the stack frame and corrupt memory. Box moves the data to the heap, leaving only a pointer on the stack.
/// Accounts for the add_liquidity instruction.
#[derive(Accounts)]
pub struct AddLiquidity<'info> {
    // The liquidity provider; mut because LP is minted to it and it pays out base/quote.
    #[account(mut)]
    pub user: Signer<'info>,

    // The has_one constraints ensure the passed-in mint/vault/LP mint all match what pool records, preventing account-substitution attacks.
    #[account(
        mut,
        seeds = [b"pool", base_mint.key().as_ref(), quote_mint.key().as_ref()],
        bump = pool.bump,
        has_one = base_mint,
        has_one = quote_mint,
        has_one = base_vault,
        has_one = quote_vault,
        has_one = lp_mint,
    )]
    pub pool: Box<Account<'info, Pool>>,

    pub base_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut)]
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,

    // The pool's two vaults and the LP mint are all modified (receiving funds / minting LP), so they are marked mut.
    #[account(mut)]
    pub base_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub lp_mint: Box<InterfaceAccount<'info, Mint>>,

    // LP lock account: MINIMUM_LIQUIDITY is minted into it on first deposit (other deposits leave it untouched). Validated via seeds + mint/authority,
    // without needing to be stored in the Pool struct; mut because the first deposit mints into it.
    #[account(
        mut,
        seeds = [b"lp_lock", pool.key().as_ref()],
        bump,
        token::mint = lp_mint,
        token::authority = pool,
    )]
    pub lp_lock: Box<InterfaceAccount<'info, TokenAccount>>,

    // The user's own three Token accounts: pay base, pay quote, receive LP; constrained so their mint and owner must be correct.
    #[account(mut, token::mint = base_mint, token::authority = user)]
    pub user_base: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint = quote_mint, token::authority = user)]
    pub user_quote: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint = lp_mint, token::authority = user)]
    pub user_lp: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(address = anchor_spl::token::ID @ DexError::UnsupportedTokenProgram)]
    pub token_program: Interface<'info, TokenInterface>,
}

/// Accounts for the remove_liquidity instruction. The fields are likewise all `Box`-ed, for the reason given in the AddLiquidity note above
/// (avoiding SBF 4 KB stack-frame overflow).
#[derive(Accounts)]
pub struct RemoveLiquidity<'info> {
    // The withdrawing liquidity provider; mut because its LP is burned and assets are returned to it.
    #[account(mut)]
    pub user: Signer<'info>,

    // Likewise use the has_one constraints to bind and validate the passed-in accounts against what pool records.
    #[account(
        mut,
        seeds = [b"pool", base_mint.key().as_ref(), quote_mint.key().as_ref()],
        bump = pool.bump,
        has_one = base_mint,
        has_one = quote_mint,
        has_one = base_vault,
        has_one = quote_vault,
        has_one = lp_mint,
    )]
    pub pool: Box<Account<'info, Pool>>,

    pub base_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut)]
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,

    // The two vaults (refund) and the LP mint (burn) are all modified, so mut.
    #[account(mut)]
    pub base_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub lp_mint: Box<InterfaceAccount<'info, Mint>>,

    // The user's three Token accounts: receive base, receive quote, have LP burned.
    #[account(mut, token::mint = base_mint, token::authority = user)]
    pub user_base: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint = quote_mint, token::authority = user)]
    pub user_quote: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint = lp_mint, token::authority = user)]
    pub user_lp: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(address = anchor_spl::token::ID @ DexError::UnsupportedTokenProgram)]
    pub token_program: Interface<'info, TokenInterface>,
}

/// Accounts for the swap instruction. The fields are likewise `Box`-ed to dodge SBF 4 KB stack-frame overflow (see the AddLiquidity note for why).
/// Note: swap does not involve LP, so there is no lp_mint / user_lp here.
#[derive(Accounts)]
pub struct Swap<'info> {
    // The trader initiating the swap; mut because it pays the input token and receives the output token.
    #[account(mut)]
    pub user: Signer<'info>,

    // The has_one constraints ensure the vaults/mints match pool; swap does not touch LP, so lp_mint need not be validated.
    #[account(
        mut,
        seeds = [b"pool", base_mint.key().as_ref(), quote_mint.key().as_ref()],
        bump = pool.bump,
        has_one = base_mint,
        has_one = quote_mint,
        has_one = base_vault,
        has_one = quote_vault,
    )]
    pub pool: Box<Account<'info, Pool>>,

    pub base_mint: Box<InterfaceAccount<'info, Mint>>,
    // quote_mint needs mut: the lazily triggered deflationary burn reduces its supply via a burn CPI.
    #[account(mut)]
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,

    // Both vaults change as the swap moves funds in and out, so mut.
    #[account(mut)]
    pub base_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    // The user's base / quote Token accounts; by direction, one pays and the other receives.
    #[account(mut, token::mint = base_mint, token::authority = user)]
    pub user_base: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint = quote_mint, token::authority = user)]
    pub user_quote: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(address = anchor_spl::token::ID @ DexError::UnsupportedTokenProgram)]
    pub token_program: Interface<'info, TokenInterface>,
}

/// A checkpoint reads only the two canonical vaults and cannot move funds or change permissions.
#[derive(Accounts)]
pub struct SyncReserves<'info> {
    #[account(
        mut,
        seeds = [b"pool", pool.base_mint.as_ref(), pool.quote_mint.as_ref()],
        bump = pool.bump,
        has_one = base_vault,
        has_one = quote_vault,
    )]
    pub pool: Box<Account<'info, Pool>>,
    pub base_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>,
}

/// Authority-controlled switch accounts.
#[derive(Accounts)]
pub struct SetSwitches<'info> {
    // The caller must be a signer, and the has_one check below confirms it is the pool's authority.
    pub authority: Signer<'info>,
    // The seeds here are derived directly from the base_mint/quote_mint recorded in pool's own fields, guaranteeing the address is correct;
    // `has_one = authority @ DexError::Unauthorized` ensures only the admin can change the switch, otherwise it returns Unauthorized.
    #[account(
        mut,
        seeds = [b"pool", pool.base_mint.as_ref(), pool.quote_mint.as_ref()],
        bump = pool.bump,
        has_one = authority @ DexError::Unauthorized,
    )]
    pub pool: Account<'info, Pool>,
}

/// Accounts for the set_day_window instruction: structurally identical to set_switches — admin signature + the pool to modify.
#[derive(Accounts)]
pub struct SetDayWindow<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"pool", pool.base_mint.as_ref(), pool.quote_mint.as_ref()],
        bump = pool.bump,
        has_one = authority @ DexError::Unauthorized,
    )]
    pub pool: Account<'info, Pool>,
}

/// Accounts for the renounce_authority instruction: structurally identical to set_switches — admin signature + the pool to modify.
#[derive(Accounts)]
pub struct RenounceAuthority<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"pool", pool.base_mint.as_ref(), pool.quote_mint.as_ref()],
        bump = pool.bump,
        has_one = authority @ DexError::Unauthorized,
    )]
    pub pool: Account<'info, Pool>,
}

/// Accounts for the set_operator instruction: structurally identical to set_switches — admin signature + the pool to modify.
#[derive(Accounts)]
pub struct SetOperator<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"pool", pool.base_mint.as_ref(), pool.quote_mint.as_ref()],
        bump = pool.bump,
        has_one = authority @ DexError::Unauthorized,
    )]
    pub pool: Account<'info, Pool>,
}

/// Accounts for the set_burn_config instruction: structurally identical to set_switches — admin signature + the pool to modify.
#[derive(Accounts)]
pub struct SetBurnConfig<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"pool", pool.base_mint.as_ref(), pool.quote_mint.as_ref()],
        bump = pool.bump,
        has_one = authority @ DexError::Unauthorized,
    )]
    pub pool: Account<'info, Pool>,
}

/// (2026-09) A single whitelist entry. Its mere existence at PDA `[b"whitelist", pool, trader]` grants
/// `trader` the right to bypass the buy/sell switch on `pool`. No mutable state beyond identity + bump.
#[account]
#[derive(InitSpace)]
pub struct Whitelist {
    pub pool: Pubkey,
    pub trader: Pubkey,
    pub bump: u8,
}

/// Accounts for `add_whitelist`: admin signs, `whitelist` PDA is created (`init`, so re-adding fails).
#[derive(Accounts)]
#[instruction(trader: Pubkey)]
pub struct AddWhitelist<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        seeds = [b"pool", pool.base_mint.as_ref(), pool.quote_mint.as_ref()],
        bump = pool.bump,
        has_one = authority @ DexError::Unauthorized,
    )]
    pub pool: Account<'info, Pool>,
    #[account(
        init,
        payer = authority,
        space = 8 + Whitelist::INIT_SPACE,
        seeds = [b"whitelist", pool.key().as_ref(), trader.as_ref()],
        bump,
    )]
    pub whitelist: Account<'info, Whitelist>,
    pub system_program: Program<'info, System>,
}

/// Accounts for `remove_whitelist`: admin signs, `whitelist` PDA is closed (rent → authority).
#[derive(Accounts)]
#[instruction(trader: Pubkey)]
pub struct RemoveWhitelist<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        seeds = [b"pool", pool.base_mint.as_ref(), pool.quote_mint.as_ref()],
        bump = pool.bump,
        has_one = authority @ DexError::Unauthorized,
    )]
    pub pool: Account<'info, Pool>,
    #[account(
        mut,
        close = authority,
        seeds = [b"whitelist", pool.key().as_ref(), trader.as_ref()],
        bump = whitelist.bump,
    )]
    pub whitelist: Account<'info, Whitelist>,
}

// (2026-09-17) The `LockLp` accounts struct was removed together with the `lock_lp` instruction.
// See the note where that instruction used to be, above.

/// Accounts for the crank_burn instruction: **no Signer at all** — a decentralized public crank that anyone can trigger by paying gas
/// (a fee payer is still needed at the transaction level, but it does not enter the instruction accounts, and there is no permission check).
/// has_one binds and validates quote_mint/quote_vault against what pool records, preventing account substitution.
#[derive(Accounts)]
pub struct CrankBurn<'info> {
    #[account(
        mut,
        seeds = [b"pool", pool.base_mint.as_ref(), pool.quote_mint.as_ref()],
        bump = pool.bump,
        has_one = quote_mint,
        has_one = quote_vault,
    )]
    pub pool: Box<Account<'info, Pool>>,

    // The burn CPI reduces both supply (mint) and the vault balance (vault), so both must be mut.
    #[account(mut)]
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut)]
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(address = anchor_spl::token::ID @ DexError::UnsupportedTokenProgram)]
    pub token_program: Interface<'info, TokenInterface>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A transfer_checked wrapper that "forwards the Token-2022 transfer hook's extra accounts".
///
/// For the leg that "moves a hooked mint", the caller passes the already-resolved hook accounts
/// (the extra-meta-list account, the hook program itself, and any PDAs the hook uses) as
/// `remaining`; we hand them straight to Token-2022 as the trailing accounts of this CPI,
/// letting Token-2022 itself resolve the hook and re-CPI — this matches exactly the account layout produced off-chain by
/// `createTransferCheckedWithTransferHookInstruction`.
/// For a "hookless" mint, just pass `remaining = &[]`.
/// `seeds` are the signing seeds for the pool PDA on a vault transfer-out (vault -> user);
/// when the account owner (user) signs directly, pass the empty slice `&[]`.
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
    // Key: must use `invoke_transfer_checked`, not anchor-spl's `transfer_checked`.
    // `invoke_transfer_checked` checks whether the mint carries a transfer hook, and if so, appends the resolved
    // hook accounts (those in `remaining`) onto the CPI instruction itself, so the hook can be triggered.
    // anchor-spl's `transfer_checked` cannot do this — it hardcodes only the 4 base accounts,
    // and "silently drops" the remaining accounts, causing hooked transfers to fail or bypass the hook.
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

/// Pure computation: given the vault balance, burn ratio, elapsed since last settlement, and interval length,
/// return (the number of full periods n to settle, the amount to burn).
/// Compound basis: missing n periods catches up in one burn of `vault * (1 - (1-bps)^n)`, equivalent to burning on time every period.
/// When n exceeds MAX_BURN_PERIODS it is truncated (the debt is left for the next trigger to keep settling).
fn compute_burn(vault_amount: u64, burn_bps: u16, elapsed: i64, interval_secs: i64) -> (u64, u64) {
    // bps=0 (disabled), an invalid interval (guarding against divide-by-zero), or not yet a full period elapsed -> nothing happens.
    if burn_bps == 0 || burn_bps >= BPS_DENOM || interval_secs <= 0 || elapsed < interval_secs {
        return (0, 0);
    }
    let elapsed_periods = (elapsed / interval_secs) as u64;
    // A stopped pool has no burn debt. In particular, do not apply old skipped periods to funds
    // deposited later. No loop is needed to retire all elapsed periods once at/below the floor.
    if vault_amount <= BURN_MIN_QUOTE_RESERVE {
        return (elapsed_periods, 0);
    }
    let periods = elapsed_periods.min(MAX_BURN_PERIODS);
    // Compound retention factor (1-bps)^n, iterated in fixed-point *PRICE_SCALE:
    // each step factor <= 1e12 * 1e4 = 1e16, well within u128 range.
    let mut factor: u128 = PRICE_SCALE;
    for _ in 0..periods {
        factor = factor * (BPS_DENOM - burn_bps) as u128 / BPS_DENOM as u128;
    }
    // remain rounds down -> burn rounds up, ensuring the long-run actual burn rate is no lower than the nominal value.
    let remain =
        ((vault_amount as u128) * factor / PRICE_SCALE).max(BURN_MIN_QUOTE_RESERVE as u128) as u64;
    // Retire the rest of the elapsed periods when the hard floor was reached: they could not
    // have burned anything. Otherwise retain the bounded catch-up behavior.
    let settled = if remain == BURN_MIN_QUOTE_RESERVE {
        elapsed_periods
    } else {
        periods
    };
    (settled, vault_amount - remain)
}

fn settle_and_reload<'info>(
    pool: &mut Account<'info, Pool>,
    quote_mint: &InterfaceAccount<'info, Mint>,
    quote_vault: &mut InterfaceAccount<'info, TokenAccount>,
    token_program: &Interface<'info, TokenInterface>,
) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    // Check the catch-up limit before any token CPI. LP ownership must not change while
    // periods older than this bounded batch remain; permissionless crank can retire them first.
    let (periods, _) = compute_burn(
        quote_vault.amount,
        pool.burn_bps,
        now.saturating_sub(pool.last_burn_ts),
        pool.burn_interval_secs,
    );
    let settled_through = pool
        .last_burn_ts
        .saturating_add((periods as i64).saturating_mul(pool.burn_interval_secs));
    require!(
        pool.burn_bps == 0 || now.saturating_sub(settled_through) < pool.burn_interval_secs,
        DexError::BurnCatchUpRequired
    );
    let pool_ai = pool.to_account_info();
    let (_, burned) = settle_burn(
        pool,
        &pool_ai,
        &quote_mint.to_account_info(),
        &quote_vault.to_account_info(),
        quote_vault.amount,
        &token_program.to_account_info(),
        now,
    )?;
    if burned > 0 {
        quote_vault.reload()?;
    }
    Ok(())
}

/// Settle the due deflationary burn (shared by crank_burn and swap's lazy trigger).
/// Returns (the number of periods settled, the amount actually burned); returns (0, 0) without erroring when no full period has elapsed (idempotent).
/// last_burn_ts advances only by whole periods (+= n*interval, not = now), with the sub-period remainder still accumulating,
/// so no matter how frequently/sparsely it is triggered, the burn cadence over the long-run timeline is exactly consistent.
#[allow(clippy::too_many_arguments)]
fn settle_burn<'info>(
    pool: &mut Pool,
    pool_ai: &AccountInfo<'info>,
    quote_mint_ai: &AccountInfo<'info>,
    quote_vault_ai: &AccountInfo<'info>,
    quote_vault_amount: u64,
    token_program_ai: &AccountInfo<'info>,
    now: i64,
) -> Result<(u64, u64)> {
    let elapsed = now.saturating_sub(pool.last_burn_ts);
    let (periods, burn_amount) = compute_burn(
        quote_vault_amount,
        pool.burn_bps,
        elapsed,
        pool.burn_interval_secs,
    );
    // compute_burn enforces the reserve floor on the final balance, including catch-up burns.
    if periods == 0 {
        return Ok((0, 0));
    }
    // When the vault balance is too small, burn_amount may round to 0: skip the CPI but still advance the timeline (the period has passed).
    if burn_amount > 0 {
        let pool_bump = [pool.bump];
        let base_mint_key = pool.base_mint;
        let quote_mint_key = pool.quote_mint;
        let signer_seeds: &[&[&[u8]]] = &[&[
            b"pool",
            base_mint_key.as_ref(),
            quote_mint_key.as_ref(),
            &pool_bump,
        ]];
        // burn only needs the token account owner (= pool PDA) to sign, not the mint authority.
        burn(
            CpiContext::new_with_signer(
                token_program_ai.clone(),
                Burn {
                    mint: quote_mint_ai.clone(),
                    from: quote_vault_ai.clone(),
                    authority: pool_ai.clone(),
                },
                signer_seeds,
            ),
            burn_amount,
        )?;
    }
    pool.last_burn_ts = pool
        .last_burn_ts
        .saturating_add((periods as i64).saturating_mul(pool.burn_interval_secs));
    emit!(BurnEvent {
        pool: pool_ai.key(),
        amount: burn_amount,
        periods,
        vault_after: quote_vault_amount - burn_amount,
        ts: now,
    });
    Ok((periods, burn_amount))
}

/// Integer square root of a u128 (rounded down), implemented via the Babylonian method / Newton's iteration.
/// Used to compute the initial LP amount sqrt(base*quote) on the first liquidity deposit.
fn isqrt_u128(n: u128) -> u128 {
    // The square root of 0 and 1 is itself; return directly to avoid entering the iteration.
    if n < 2 {
        return n;
    }
    // Use the most significant bit to derive an initial x no less than the true square root, ensuring Newton's iteration converges monotonically decreasing.
    let bitlen = 128 - n.leading_zeros();
    let mut x = 1u128 << ((bitlen + 1) / 2);
    loop {
        // Newton iteration step: y = (x + n/x) / 2, progressively approaching the square root.
        let y = (x + n / x) >> 1;
        // When it no longer decreases (y >= x), x is the floored square root.
        if y >= x {
            return x;
        }
        x = y;
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

// Note: the English error messages inside the `#[msg("...")]` below are part of the on-chain IDL; keep them as-is and do NOT translate;
// the comment after each one explains "when it triggers".
#[error_code]
pub enum DexError {
    // The caller is not the pool's authority — set_switches' has_one check failed.
    #[msg("Signer is not the pool authority")]
    Unauthorized,
    // The opening-price day-window seconds are invalid: set_day_window requires day_window_secs >= 1.
    #[msg("day_window_secs must be >= 1")]
    BadTimeWindow,
    // The buy switch is off: attempted a side=0 swap while buy_enabled is false.
    #[msg("Buy side is disabled")]
    BuyDisabled,
    // The sell switch is off: attempted a side=1 swap while sell_enabled is false.
    #[msg("Sell side is disabled")]
    SellDisabled,
    // Invalid direction: side is neither 0 (buy) nor 1 (sell).
    #[msg("Invalid side (must be 0 = buy or 1 = sell)")]
    InvalidSide,
    // Slippage tolerance exceeded: the actual output / LP amount is below the caller's lower bound.
    #[msg("Slippage tolerance exceeded")]
    SlippageExceeded,
    // Arithmetic overflow: a multiplication or type conversion went out of range (mitigated by computing in u128 where possible).
    #[msg("Arithmetic overflow")]
    MathOverflow,
    // Invalid fee: fee_bps must be below 10000 bps (i.e. < 100%).
    #[msg("Invalid fee (must be < 10000 bps)")]
    InvalidFee,
    // Zero amount: the input or the add/remove amount is 0.
    #[msg("Zero amount")]
    ZeroAmount,
    // Zero liquidity: the LP to mint is 0, or a removal is attempted with no LP at all.
    #[msg("Zero liquidity")]
    ZeroLiquidity,
    // Pool reserves are empty: one side (or both) has 0 reserve, making pricing impossible.
    #[msg("Pool reserves are empty")]
    EmptyPool,
    // Insufficient output-side liquidity: the computed output is not less than the output-side reserve (a backstop, should not normally happen).
    #[msg("Not enough output liquidity")]
    InsufficientLiquidity,
    // Wrong token mint: base and quote are the same, or a passed-in mint does not match the config.
    #[msg("Wrong token mint")]
    WrongMint,
    // Burn not due: no full burn_interval_secs period has elapsed since the last settlement.
    #[msg("Burn period has not elapsed yet")]
    BurnNotDue,
    // Invalid burn config: burn_bps exceeds the hard cap, or burn_interval_secs is below the hard floor.
    #[msg("Invalid burn config (bps > max or interval < min)")]
    InvalidBurnConfig,
    // transfer_authority was given the all-zero pubkey (use renounce_authority to give up admin).
    #[msg("New authority must not be the default/zero pubkey")]
    InvalidAuthority,
    // Retired error code retained to preserve subsequent error numbers.
    #[msg("Liquidity add/remove is disabled for this pool")]
    LiquidityPaused,
    // lp_mint.supply exceeded the pool's own share ledger — impossible unless LP was minted outside the pool.
    #[msg("LP supply exceeds the pool share ledger")]
    LpAccountingBroken,
    #[msg("Only classic SPL Token is supported")]
    UnsupportedTokenProgram,
    #[msg("Settle outstanding burn periods with crank_burn before changing liquidity")]
    BurnCatchUpRequired,
    // A swap left reserve_base * reserve_quote below its pre-trade value. Unreachable with the
    // current floor-rounded pricing; a backstop so any future change to the math fails loudly.
    #[msg("Swap decreased the constant product")]
    InvariantViolated,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Emitted on each successful deflationary-burn settlement (crank_burn or swap's lazy trigger).
#[event]
pub struct BurnEvent {
    pub pool: Pubkey,     // the pool where the burn happened
    pub amount: u64,      // the amount of quote (Beta) actually burned this time
    pub periods: u64, // the number of full periods this settlement covers (>1 = compounded catch-up burn)
    pub vault_after: u64, // the quote vault balance after the burn
    pub ts: i64,      // the on-chain timestamp at settlement
}

/// (2026-09) Emitted when the authority adds (`allowed=true`) or removes (`allowed=false`) a whitelist entry.
#[event]
pub struct WhitelistEvent {
    pub pool: Pubkey,
    pub trader: Pubkey,
    pub allowed: bool,
}

// (2026-09-17) `LpLockedEvent` was removed with the `lock_lp` instruction. Permanent locking now
// emits `lp_locker::LockedEvent` from the standalone locker program instead.

// ---------------------------------------------------------------------------
// Unit tests (pure-computation part)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const VAULT: u64 = 1_000_000_000_000_000_000;

    #[test]
    fn not_due_yet() {
        assert_eq!(compute_burn(VAULT, 20, 86_399, 86_400), (0, 0));
        assert_eq!(compute_burn(VAULT, 20, 0, 86_400), (0, 0));
        assert_eq!(compute_burn(VAULT, 20, -5, 86_400), (0, 0));
    }

    #[test]
    fn disabled_or_bad_interval() {
        assert_eq!(compute_burn(VAULT, 0, 1_000_000, 86_400), (0, 0));
        assert_eq!(compute_burn(VAULT, 20, 1_000_000, 0), (0, 0));
        assert_eq!(compute_burn(VAULT, 20, 1_000_000, -1), (0, 0));
    }

    #[test]
    fn single_period_is_exact_bps() {
        // Exactly one period: burn 0.2%
        let (n, burned) = compute_burn(VAULT, 20, 86_400, 86_400);
        assert_eq!(n, 1);
        assert_eq!(burned, VAULT / 10_000 * 20);
        // Less than two periods still settles only one
        let (n, _) = compute_burn(VAULT, 20, 2 * 86_400 - 1, 86_400);
        assert_eq!(n, 1);
    }

    #[test]
    fn compound_three_periods() {
        // 1 - 0.998^3; the compound factor is exactly representable at 1e12 precision.
        let (n, burned) = compute_burn(VAULT, 20, 3 * 86_400, 86_400);
        assert_eq!(n, 3);
        let remain = (((VAULT as u128) * 9_980 / 10_000) * 9_980 / 10_000) * 9_980 / 10_000;
        assert_eq!(burned as u128, VAULT as u128 - remain);
    }

    #[test]
    fn periods_clamped() {
        let elapsed = (MAX_BURN_PERIODS as i64 + 100) * 86_400;
        let (n, _) = compute_burn(u64::MAX, 1, elapsed, 86_400);
        assert_eq!(n, MAX_BURN_PERIODS);
    }

    #[test]
    fn burn_floor_and_empty_retire_all_skipped_periods() {
        for amount in [0, 3, BURN_MIN_QUOTE_RESERVE - 1, BURN_MIN_QUOTE_RESERVE] {
            assert_eq!(compute_burn(amount, 100, 10_000, 1), (10_000, 0));
        }
        assert_eq!(
            compute_burn(BURN_MIN_QUOTE_RESERVE + 1, 100, 10_000, 1),
            (10_000, 1)
        );
    }

    #[test]
    fn catch_up_can_never_burn_through_the_floor() {
        for amount in [
            BURN_MIN_QUOTE_RESERVE + 1,
            BURN_MIN_QUOTE_RESERVE * 2,
            VAULT,
            u64::MAX,
        ] {
            for bps in [1, 20, 100] {
                for periods in [1, 90, 2349, 3650, 10_000, i64::MAX] {
                    let (settled, burned) = compute_burn(amount, bps, periods, 1);
                    assert!(settled > 0 && settled <= periods as u64);
                    assert!(amount - burned >= BURN_MIN_QUOTE_RESERVE);
                    if amount - burned == BURN_MIN_QUOTE_RESERVE {
                        assert_eq!(settled, periods as u64);
                    }
                }
            }
        }
    }

    #[test]
    fn integer_sqrt_is_exact_at_boundaries_and_random_products() {
        let check = |n: u128| {
            let root = isqrt_u128(n);
            assert!(root == 0 || root <= n / root);
            assert!(root + 1 > n / (root + 1));
        };
        for n in [
            0,
            1,
            2,
            3,
            4,
            999_999,
            1_000_000,
            u64::MAX as u128,
            u128::MAX,
        ] {
            check(n);
        }
        let mut seed = 17u64;
        for _ in 0..10_000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let a = seed;
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            check(a as u128 * seed as u128);
        }
    }

    #[test]
    fn checkpoint_never_backdates_direct_deposits() {
        let data = vec![0; 8 + Pool::INIT_SPACE];
        let mut pool = Pool::try_deserialize_unchecked(&mut data.as_slice()).unwrap();
        pool.observe_reserves(100, 200, 10).unwrap();
        pool.observe_reserves(300, 600, 20).unwrap();
        assert_eq!(pool.cumulative_quote, 2_000);
        assert_eq!(pool.cumulative_price, 5 * PRICE_SCALE);
        pool.observe_reserves(100, 200, 20).unwrap();
        assert_eq!(pool.cumulative_quote, 2_000);
        assert_eq!(pool.quote_integral(25).unwrap(), 3_000);
        assert!(pool.quote_integral(19).is_err());
    }

    #[test]
    fn extreme_price_counter_rolls_over_without_locking_the_pool() {
        let data = vec![0; 8 + Pool::INIT_SPACE];
        let mut pool = Pool::try_deserialize_unchecked(&mut data.as_slice()).unwrap();
        pool.observe_reserves(u64::MAX, 1, 1).unwrap();
        let price = u64::MAX as u128 * PRICE_SCALE;
        let before_rollover = (u128::MAX / price) as i64;
        pool.accumulate(before_rollover).unwrap();
        let before = pool.cumulative_price;
        pool.accumulate(before_rollover + 1_920).unwrap();
        assert!(pool.cumulative_price < before);
        assert_eq!(pool.cumulative_price.wrapping_sub(before), price * 1_920);
        pool.observe_reserves(1_000, 1_000, before_rollover + 1_920)
            .unwrap();
        assert!(pool.price_integral(before_rollover + 2_000).is_ok());
    }
}
