//! Bounded, permissionless max-leg maintenance. UserStake v2 is 516 bytes; this
//! layout is for the not-yet-initialized production release, not an in-place
//! upgrade of existing 504-byte business accounts.
use crate::*;

pub const MAX_LEG_PAGE_SIZE: usize = 16;

pub fn invalidate(parent: &mut UserStake) -> Result<()> {
    parent.child_leg_revision = parent.child_leg_revision.checked_add(1)
        .ok_or(DonateError::MathOverflow)?;
    Ok(())
}

#[account]
#[derive(InitSpace)]
pub struct MaxLegRefresh {
    pub initialized: bool,
    pub complete: bool,
    pub revision: u64,
    pub expected_children: u32,
    pub cursor: u32,
    pub maximum: u64,
}

#[derive(Accounts)]
pub struct BeginMaxLegRefresh<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(seeds=[b"donate-config", config.config_seed.as_ref()], bump=config.bump)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(seeds=[b"donate-user", config.key().as_ref(), user_stake.owner.as_ref()], bump=user_stake.bump)]
    pub user_stake: Box<Account<'info, UserStake>>,
    #[account(init_if_needed, payer=payer, space=8+MaxLegRefresh::INIT_SPACE,
        seeds=[b"max-leg-refresh", config.key().as_ref(), user_stake.key().as_ref()], bump)]
    pub refresh: Account<'info, MaxLegRefresh>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RefreshMaxLegPage<'info> {
    #[account(mut, seeds=[b"donate-config", config.config_seed.as_ref()], bump=config.bump)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(mut, seeds=[b"donate-user", config.key().as_ref(), user_stake.owner.as_ref()], bump=user_stake.bump)]
    pub user_stake: Box<Account<'info, UserStake>>,
    #[account(mut, seeds=[b"max-leg-refresh", config.key().as_ref(), user_stake.key().as_ref()], bump)]
    pub refresh: Account<'info, MaxLegRefresh>,
    #[account(address=config.treasury)]
    pub treasury: InterfaceAccount<'info, TokenAccount>,
}

pub fn begin(ctx: Context<BeginMaxLegRefresh>) -> Result<()> {
    migration::active(&ctx.accounts.config)?;
    let u = &ctx.accounts.user_stake;
    let scan = &mut ctx.accounts.refresh;
    // Nobody can grief an unchanged, partially completed scan by restarting it.
    if scan.initialized && !scan.complete && scan.revision == u.child_leg_revision {
        return Ok(());
    }
    scan.initialized = true;
    scan.complete = false;
    scan.revision = u.child_leg_revision;
    scan.expected_children = u.direct_count;
    scan.cursor = 0;
    scan.maximum = 0;
    Ok(())
}

pub fn page<'info>(
    ctx: Context<'_, '_, 'info, 'info, RefreshMaxLegPage<'info>>,
    revision: u64,
    cursor: u32,
) -> Result<()> {
    migration::active(&ctx.accounts.config)?;
    let scan = &mut ctx.accounts.refresh;
    let u = &mut ctx.accounts.user_stake;
    require!(scan.initialized && !scan.complete, MaxLegError::NotInProgress);
    require!(revision == scan.revision && revision == u.child_leg_revision, MaxLegError::StaleScan);
    require!(cursor == scan.cursor, MaxLegError::WrongCursor);
    let n = ctx.remaining_accounts.len();
    require!(n <= MAX_LEG_PAGE_SIZE && (n > 0 || scan.expected_children == 0), MaxLegError::BadPage);
    let end = cursor.checked_add(n as u32).ok_or(DonateError::MathOverflow)?;
    require!(end <= scan.expected_children, MaxLegError::BadPage);
    let config_key = ctx.accounts.config.key();
    for ai in ctx.remaining_accounts {
        require_keys_neq!(ai.key(), u.key(), DonateError::BadChain);
        let child = Account::<UserStake>::try_from(ai)?;
        require_keys_eq!(child.referrer, u.owner, DonateError::BadChain);
        let pda = Pubkey::create_program_address(&[
            b"donate-user", config_key.as_ref(), child.owner.as_ref(), &[child.bump],
        ], &crate::ID).map_err(|_| error!(DonateError::BadChain))?;
        require_keys_eq!(pda, ai.key(), DonateError::BadChain);
        // Consecutive immutable indices prove uniqueness AND completeness, without
        // storing an unbounded set or requiring every child in one transaction.
        require!(child.referral_index == scan.cursor + 1, MaxLegError::WrongChild);
        scan.maximum = scan.maximum.max(child.leg_power);
        scan.cursor += 1;
    }
    if scan.cursor == scan.expected_children {
        // New referrals beyond the frozen prefix still have zero legs: any sync
        // or seed making one nonzero would have invalidated this parent's revision.
        accrue_v(&mut ctx.accounts.config, ctx.accounts.treasury.amount, Clock::get()?.unix_timestamp);
        let snap = VSnap::of(&ctx.accounts.config);
        let level = u.level;
        v_unbucket(&mut ctx.accounts.config, u, &snap);
        u.max_leg = scan.maximum;
        v_rebucket(&mut ctx.accounts.config, u, &snap, level);
        scan.complete = true;
        // As in the atomic API, sync_level separately propagates level/star changes.
    }
    Ok(())
}

#[error_code(offset = 7000)]
pub enum MaxLegError {
    #[msg("Max-leg refresh is not in progress")]
    NotInProgress,
    #[msg("Child legs changed; restart max-leg refresh")]
    StaleScan,
    #[msg("Max-leg refresh cursor has advanced")]
    WrongCursor,
    #[msg("Max-leg page must contain 1..16 children within the frozen prefix")]
    BadPage,
    #[msg("Expected the next canonical child in referral-index order")]
    WrongChild,
}
