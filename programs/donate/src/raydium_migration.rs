//! Paused one-shot venue migration; all existing user layouts are retained.
use crate::*;
use anchor_lang::solana_program::hash::hashv;
const LEGACY_POOL: Pubkey = pubkey!("2A9fNRik2FogYNcnCJw6seYL7Xpjx6zRYAzcM7AZsxZA");
const LEGACY_MINT: Pubkey = pubkey!("8UqYNLpm5fzG8LZXVaje6nf6gEdvB6WeLY1rLzoAnoc6");
const LEGACY_LP: Pubkey = pubkey!("7rmNkUJg8uZwmPeyN29QdnGjPvc49FmsT5LwKTJSrF5N");
pub const DEPLOYMENT: Pubkey = pubkey!("FJF6YMp72V8NHUniRZxweeZd5CyyTNYY96dUKuaG2kmw");
pub const CHECKPOINTING: u8 = 6;
pub const RAYDIUM_WARMING: u8 = 7;
#[account]
#[derive(InitSpace)]
pub struct RaydiumMigration {
    pub config: Pubkey,
    pub legacy_mint: Pubkey,
    pub legacy_treasury: Pubkey,
    pub legacy_pool: Pubkey,
    pub new_mint: Pubkey,
    pub target_pool: Pubkey,
    pub fee_recipient: Pubkey,
    pub supply: u64,
    pub treasury_balance: u64,
    pub cutoff_ts: i64,
    pub resume_ts: i64,
    pub expected_users: u32,
    pub checkpointed_users: u32,
    pub expected_hash: [u8; 32],
    pub checkpoint_hash: [u8; 32],
    pub open_day: u32,
    pub open_price: u128,
    pub bound: bool,
    pub completed: bool,
}
#[account]
#[derive(InitSpace)]
pub struct RaydiumUserCheckpoint {
    pub config: Pubkey,
    pub owner: Pubkey,
    pub cutoff_ts: i64,
    pub resume_ts: i64,
}
#[derive(Accounts)]
pub struct BeginRaydiumMigration<'info> {
    #[account(mut,address=DEPLOYMENT)]
    pub authority: Signer<'info>,
    #[account(mut,seeds=[b"donate-config",config.config_seed.as_ref()],bump=config.bump,has_one=authority)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(address=LEGACY_MINT,constraint=config.beta_mint==LEGACY_MINT @ DonateError::WrongMint)]
    pub legacy_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(address=config.treasury,token::mint=legacy_mint,token::authority=config)]
    pub treasury: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(init,payer=authority,space=8+RaydiumMigration::INIT_SPACE,seeds=[b"raydium-migration",config.key().as_ref()],bump)]
    pub migration: Box<Account<'info, RaydiumMigration>>,
    #[account(address=LEGACY_POOL,constraint=config.pool==LEGACY_POOL && pool.lp_mint==LEGACY_LP && pool.burn_bps==0 && pool.operator==Pubkey::default() && !pool.buy_enabled && !pool.sell_enabled @ DonateError::MigrationPhase)]
    pub pool: Box<Account<'info, dex_pool::Pool>>,
    pub system_program: Program<'info, System>,
}
pub fn begin(
    ctx: Context<BeginRaydiumMigration>,
    users: u32,
    hash: [u8; 32],
    resume_ts: i64,
) -> Result<()> {
    let c = &mut ctx.accounts.config;
    active(c)?;
    let now = Clock::get()?.unix_timestamp;
    require!(
        users >= c.seeded_user_count
            && users > 0
            && hash != [0; 32]
            && resume_ts > now
            && resume_ts <= now + 86400,
        DonateError::MigrationSnapshotMismatch
    );
    require!(
        c.sol_budget == 0 && c.sol_alpha_budget == 0 && c.ops_alpha_budget == 0,
        DonateError::InsufficientEscrow
    );
    require!(
        c.alpha_mint == venue::LIKE && ctx.accounts.legacy_mint.decimals == 9,
        DonateError::WrongMint
    );
    accrue_global(c, ctx.accounts.treasury.amount, now);
    accrue_v(c, ctx.accounts.treasury.amount, now);
    ctx.accounts.migration.set_inner(RaydiumMigration {
        config: c.key(),
        legacy_mint: c.beta_mint,
        legacy_treasury: c.treasury,
        legacy_pool: c.pool,
        new_mint: Pubkey::default(),
        target_pool: Pubkey::default(),
        fee_recipient: Pubkey::default(),
        supply: ctx.accounts.legacy_mint.supply,
        treasury_balance: ctx.accounts.treasury.amount,
        cutoff_ts: now,
        resume_ts,
        expected_users: users,
        checkpointed_users: 0,
        expected_hash: hash,
        checkpoint_hash: [0; 32],
        open_day: 0,
        open_price: 0,
        bound: false,
        completed: false,
    });
    c.migration_phase = CHECKPOINTING;
    c.price_ready = false;
    Ok(())
}
#[derive(Accounts)]
#[instruction(user_owner:Pubkey)]
pub struct CheckpointRaydiumUser<'info> {
    #[account(mut,address=DEPLOYMENT)]
    pub authority: Signer<'info>,
    #[account(seeds=[b"donate-config",config.config_seed.as_ref()],bump=config.bump,has_one=authority)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(mut,seeds=[b"raydium-migration",config.key().as_ref()],bump,has_one=config)]
    pub migration: Box<Account<'info, RaydiumMigration>>,
    #[account(mut,seeds=[b"donate-user",config.key().as_ref(),user_owner.as_ref()],bump=user_stake.bump,constraint=user_stake.owner==user_owner)]
    pub user_stake: Box<Account<'info, UserStake>>,
    #[account(init_if_needed,payer=authority,space=8+ClaimAllowance::INIT_SPACE,seeds=[b"claim-allowance",config.key().as_ref(),user_owner.as_ref()],bump)]
    pub claim_allowance: Box<Account<'info, ClaimAllowance>>,
    #[account(init_if_needed,payer=authority,space=8+RaydiumUserCheckpoint::INIT_SPACE,seeds=[b"raydium-user",config.key().as_ref(),user_owner.as_ref()],bump)]
    pub checkpoint: Box<Account<'info, RaydiumUserCheckpoint>>,
    pub system_program: Program<'info, System>,
}
pub fn checkpoint(ctx: Context<CheckpointRaydiumUser>, _owner: Pubkey) -> Result<()> {
    let c = &ctx.accounts.config;
    let m = &mut ctx.accounts.migration;
    require!(
        (c.migration_phase == CHECKPOINTING || c.migration_phase == RAYDIUM_WARMING)
            && m.checkpointed_users < m.expected_users
            && Clock::get()?.unix_timestamp < m.resume_ts,
        DonateError::MigrationPhase
    );
    let u = &mut ctx.accounts.user_stake;
    let previous = &ctx.accounts.checkpoint;
    require!(
        previous.resume_ts != m.resume_ts,
        DonateError::MigrationSnapshotMismatch
    );
    if previous.resume_ts == 0 {
        settle(u, c.acc_reward_per_power);
        settle_v(u, &VSnap::of(c));
        ctx.accounts.claim_allowance.checkpoint(
            c.key(),
            c,
            u,
            m.cutoff_ts,
            ctx.bumps.claim_allowance,
        )?;
    } else {
        require!(
            previous.config == c.key()
                && previous.owner == u.owner
                && previous.cutoff_ts == m.cutoff_ts
                && ctx.accounts.claim_allowance.checkpoint_ts == previous.resume_ts,
            DonateError::MigrationSnapshotMismatch
        );
    }
    ctx.accounts.claim_allowance.checkpoint_ts = m.resume_ts;
    ctx.accounts.checkpoint.set_inner(RaydiumUserCheckpoint {
        config: c.key(),
        owner: u.owner,
        cutoff_ts: m.cutoff_ts,
        resume_ts: m.resume_ts,
    });
    m.checkpoint_hash = hashv(&[&m.checkpoint_hash, u.owner.as_ref()]).to_bytes();
    m.checkpointed_users += 1;
    Ok(())
}
/// Bind the already-live classic-SPL LIKER/LIKE CLMM pool. No minting or new
/// reward vault occurs: the existing LIKER treasury continues to pay rewards.
#[derive(Accounts)]
pub struct BindExistingClmmPool<'info> {
    #[account(address=DEPLOYMENT)]
    pub authority: Signer<'info>,
    #[account(mut,seeds=[b"donate-config",config.config_seed.as_ref()],bump=config.bump,has_one=authority)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(mut,seeds=[b"raydium-migration",config.key().as_ref()],bump,has_one=config)]
    pub migration: Box<Account<'info, RaydiumMigration>>,
    #[account(address=venue::EXISTING_CLMM_POOL)]
    pub pool: Box<InterfaceAccount<'info, VenuePool>>,
    #[account(address=migration.legacy_treasury,token::mint=legacy_mint,token::authority=config)]
    pub treasury: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(address=venue::LEGACY_LIKER)]
    pub legacy_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(address=pool.base_vault,token::mint=config.alpha_mint)]
    pub base_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(address=pool.quote_vault,token::mint=legacy_mint)]
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut,seeds=[b"price-observation",config.key().as_ref()],bump,has_one=config)]
    pub observation: Box<Account<'info, PriceObservation>>,
    /// CHECK: fixed Locker release record, fully decoded in the handler.
    #[account(seeds=[b"legacy-release"],bump,seeds::program=LP_LOCKER_ID,owner=LP_LOCKER_ID)]
    pub legacy_release: UncheckedAccount<'info>,
    /// CHECK: canonical CLMM position PDA and bytes verified in the handler.
    #[account(owner=venue::CLMM)]
    pub deployment_position: UncheckedAccount<'info>,
    pub position_nft_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(token::mint=position_nft_mint,token::authority=authority)]
    pub deployment_position_nft: Box<InterfaceAccount<'info, TokenAccount>>,
}

pub fn bind_existing_clmm(ctx: Context<BindExistingClmmPool>) -> Result<()> {
    let c = &mut ctx.accounts.config;
    let m = &mut ctx.accounts.migration;
    let p = &ctx.accounts.pool;
    require!(
        c.migration_phase == CHECKPOINTING && !m.bound
            && m.checkpointed_users == m.expected_users
            && m.checkpoint_hash == m.expected_hash,
        DonateError::MigrationSnapshotMismatch
    );
    require!(
        p.clmm.is_some() && p.key() == venue::EXISTING_CLMM_POOL
            && p.base_mint == c.alpha_mint && p.quote_mint == m.legacy_mint
            && p.canonical_pool() == p.key()
            && ctx.accounts.legacy_mint.decimals == 9
            && ctx.accounts.treasury.amount == m.treasury_balance
            && ctx.accounts.base_vault.amount > 0 && ctx.accounts.quote_vault.amount > 0,
        DonateError::BadVaultAccount
    );
    let release = ctx.accounts.legacy_release.try_borrow_data()?;
    require!(
        release.len() == 112
            && release[..8] == anchor_lang::solana_program::hash::hash(b"account:LegacyRelease").to_bytes()[..8]
            && Pubkey::new_from_array(release[8..40].try_into().unwrap()) == m.legacy_pool
            && Pubkey::new_from_array(release[40..72].try_into().unwrap()) == DEPLOYMENT
            && u64::from_le_bytes(release[72..80].try_into().unwrap()) > 0,
        DonateError::MigrationSnapshotMismatch
    );
    let position = ctx.accounts.deployment_position.try_borrow_data()?;
    require!(
        position.len() == 281
            && position[..8] == anchor_lang::solana_program::hash::hash(b"account:PersonalPositionState").to_bytes()[..8]
            && Pubkey::new_from_array(position[9..41].try_into().unwrap()) == ctx.accounts.position_nft_mint.key()
            && Pubkey::new_from_array(position[41..73].try_into().unwrap()) == p.key()
            && u128::from_le_bytes(position[81..97].try_into().unwrap()) > 0
            && ctx.accounts.deployment_position.key() == Pubkey::find_program_address(
                &[b"position", ctx.accounts.position_nft_mint.key().as_ref()], &venue::CLMM).0
            && ctx.accounts.position_nft_mint.supply == 1
            && ctx.accounts.deployment_position_nft.amount == 1,
        DonateError::MigrationSnapshotMismatch
    );
    m.target_pool = p.key();
    m.new_mint = m.legacy_mint;
    m.fee_recipient = Pubkey::default();
    m.bound = true;
    c.pool = p.key();
    c.base_vault = p.base_vault;
    c.quote_vault = p.quote_vault;
    c.lp_recipient = INCINERATOR;
    // The legacy one-million-token floor was tied to the old pool reserves.
    // This existing CLMM holds far less LIKER, so retaining that floor would
    // silently disable the explicitly retained Claim blackhole burn.
    c.blackhole_floor = 0;
    c.migration_phase = RAYDIUM_WARMING;
    c.price_ready = false;
    c.price_breaker = false;
    ctx.accounts.observation.set_inner(PriceObservation {
        config: c.key(), ..PriceObservation::default_state()
    });
    Ok(())
}
/// Bind the fixed, prepared CLMM independently of who funded or owns its positions.
/// Existing migration checkpoints, treasury and fixed Locker release remain mandatory.
#[derive(Accounts)]
pub struct BindPreparedClmmPool<'info> {
    #[account(address=DEPLOYMENT)]
    pub authority: Signer<'info>,
    #[account(mut,seeds=[b"donate-config",config.config_seed.as_ref()],bump=config.bump,has_one=authority)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(mut,seeds=[b"raydium-migration",config.key().as_ref()],bump,has_one=config)]
    pub migration: Box<Account<'info, RaydiumMigration>>,
    #[account(address=venue::EXISTING_CLMM_POOL)]
    pub pool: Box<InterfaceAccount<'info, VenuePool>>,
    #[account(address=migration.legacy_treasury,token::mint=legacy_mint,token::authority=config)]
    pub treasury: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(address=venue::LEGACY_LIKER)]
    pub legacy_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(address=pool.base_vault,token::mint=config.alpha_mint)]
    pub base_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(address=pool.quote_vault,token::mint=legacy_mint)]
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut,seeds=[b"price-observation",config.key().as_ref()],bump,has_one=config)]
    pub observation: Box<Account<'info, PriceObservation>>,
    /// CHECK: fixed Locker release record, fully decoded in the handler.
    #[account(seeds=[b"legacy-release"],bump,seeds::program=LP_LOCKER_ID,owner=LP_LOCKER_ID)]
    pub legacy_release: UncheckedAccount<'info>,
}

pub fn bind_prepared_clmm(ctx: Context<BindPreparedClmmPool>) -> Result<()> {
    let c = &mut ctx.accounts.config;
    let m = &mut ctx.accounts.migration;
    let p = &ctx.accounts.pool;
    require!(
        c.migration_phase == CHECKPOINTING && !m.bound
            && m.checkpointed_users == m.expected_users
            && m.checkpoint_hash == m.expected_hash,
        DonateError::MigrationSnapshotMismatch
    );
    require!(
        p.clmm.is_some() && p.key() == venue::EXISTING_CLMM_POOL
            && p.base_mint == c.alpha_mint && p.quote_mint == m.legacy_mint
            && p.canonical_pool() == p.key()
            && ctx.accounts.legacy_mint.decimals == 9
            && ctx.accounts.treasury.amount == m.treasury_balance
            && ctx.accounts.base_vault.amount > 0 && ctx.accounts.quote_vault.amount > 0,
        DonateError::BadVaultAccount
    );
    let release = ctx.accounts.legacy_release.try_borrow_data()?;
    require!(
        release.len() == 112
            && release[..8] == anchor_lang::solana_program::hash::hash(b"account:LegacyRelease").to_bytes()[..8]
            && Pubkey::new_from_array(release[8..40].try_into().unwrap()) == m.legacy_pool
            && Pubkey::new_from_array(release[40..72].try_into().unwrap()) == DEPLOYMENT
            && u64::from_le_bytes(release[72..80].try_into().unwrap()) > 0,
        DonateError::MigrationSnapshotMismatch
    );
    // Funding and ownership of pre-existing positions remain external.
    // Require live active liquidity at the current price before resetting observations.
    require!(
        p.clmm.as_ref().is_some_and(|clmm| clmm.liquidity > 0 && clmm.sqrt_price_x64 > 0),
        DonateError::MigrationSnapshotMismatch
    );
    m.target_pool = p.key();
    m.new_mint = m.legacy_mint;
    m.fee_recipient = Pubkey::default();
    m.bound = true;
    c.pool = p.key();
    c.base_vault = p.base_vault;
    c.quote_vault = p.quote_vault;
    c.lp_recipient = INCINERATOR;
    // The legacy one-million-token floor was tied to the old pool reserves.
    // This existing CLMM holds far less LIKER, so retaining that floor would
    // silently disable the explicitly retained Claim blackhole burn.
    c.blackhole_floor = 0;
    c.migration_phase = RAYDIUM_WARMING;
    c.price_ready = false;
    c.price_breaker = false;
    ctx.accounts.observation.set_inner(PriceObservation {
        config: c.key(), ..PriceObservation::default_state()
    });
    Ok(())
}

#[derive(Accounts)]
pub struct FinalizeRaydiumMigration<'info> {
    #[account(mut,address=DEPLOYMENT)]
    pub authority: Signer<'info>,
    #[account(mut,seeds=[b"donate-config",config.config_seed.as_ref()],bump=config.bump,has_one=authority)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(mut,seeds=[b"raydium-migration",config.key().as_ref()],bump,has_one=config)]
    pub migration: Box<Account<'info, RaydiumMigration>>,
    #[account(address=config.treasury)]
    pub treasury: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(init,payer=authority,space=8+TreasuryBurn::INIT_SPACE,seeds=[b"treasury-burn",config.key().as_ref()],bump)]
    pub treasury_burn: Box<Account<'info, TreasuryBurn>>,
    pub system_program: Program<'info, System>,
}

pub fn finalize(ctx: Context<FinalizeRaydiumMigration>) -> Result<()> {
    let c = &mut ctx.accounts.config;
    let m = &mut ctx.accounts.migration;
    let now = Clock::get()?.unix_timestamp;
    require!(
        c.migration_phase == RAYDIUM_WARMING
            && m.bound
            && !m.completed
            && m.checkpointed_users == m.expected_users
            && m.checkpoint_hash == m.expected_hash
            && now <= m.resume_ts
            && ctx.accounts.treasury.amount == m.treasury_balance,
        DonateError::MigrationPhase
    );
    oracle::fresh(c, now)?;
    require!(!c.price_breaker, DonateError::PriceUnavailable);
    c.last_update_ts = m.resume_ts;
    c.v_last_update_ts = m.resume_ts;
    ctx.accounts.treasury_burn.set_inner(TreasuryBurn {
        config:c.key(), treasury:c.treasury, mint:c.beta_mint,
        start_ts:m.resume_ts, last_burn_ts:m.resume_ts, total_burned:0,
    });
    c.migration_phase = ACTIVE;
    m.completed = true;
    Ok(())
}

/// A completed migration may start early once, before the original schedule.
/// Keep resume_ts as the immutable original allowance marker. TreasuryBurn.start_ts
/// records the effective start, so existing business accounts need no reallocation.
#[derive(Accounts)]
pub struct ResumeRaydiumNow<'info> {
    #[account(address=DEPLOYMENT)]
    pub authority: Signer<'info>,
    #[account(mut,seeds=[b"donate-config",config.config_seed.as_ref()],bump=config.bump,has_one=authority)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(seeds=[b"raydium-migration",config.key().as_ref()],bump,has_one=config)]
    pub migration: Box<Account<'info, RaydiumMigration>>,
    #[account(mut,seeds=[b"treasury-burn",config.key().as_ref()],bump,has_one=config)]
    pub treasury_burn: Box<Account<'info, TreasuryBurn>>,
    #[account(address=config.treasury,token::mint=config.beta_mint,token::authority=config)]
    pub treasury: Box<InterfaceAccount<'info, TokenAccount>>,
}

pub fn resume_now(ctx: Context<ResumeRaydiumNow>) -> Result<()> {
    let c=&mut ctx.accounts.config;
    let m=&ctx.accounts.migration;
    let b=&mut ctx.accounts.treasury_burn;
    let now=Clock::get()?.unix_timestamp;
    require!(c.migration_phase==ACTIVE && m.completed && m.bound
        && c.pool==venue::EXISTING_CLMM_POOL && m.target_pool==c.pool
        && c.beta_mint==LEGACY_MINT && c.alpha_mint==venue::LIKE
        && c.lp_recipient==INCINERATOR
        && m.checkpointed_users==m.expected_users && m.expected_users>0
        && m.checkpoint_hash==m.expected_hash
        && now>m.cutoff_ts && now<m.resume_ts
        && c.last_update_ts==m.resume_ts && c.v_last_update_ts==m.resume_ts
        && b.treasury==c.treasury && b.mint==c.beta_mint
        && b.start_ts==m.resume_ts && b.last_burn_ts==m.resume_ts && b.total_burned==0
        && ctx.accounts.treasury.amount==m.treasury_balance
        && c.sol_budget==0 && c.sol_alpha_budget==0 && c.ops_alpha_budget==0,
        DonateError::MigrationPhase);
    oracle::fresh(c,now)?;
    require!(!c.price_breaker,DonateError::PriceUnavailable);
    c.last_update_ts=now;
    c.v_last_update_ts=now;
    b.start_ts=now;
    b.last_burn_ts=now;
    emit!(RaydiumEarlyResume {config:c.key(), original_resume_ts:m.resume_ts, effective_resume_ts:now});
    Ok(())
}

#[event]
pub struct RaydiumEarlyResume {
    pub config: Pubkey,
    pub original_resume_ts: i64,
    pub effective_resume_ts: i64,
}

/// Read the existing canonical migration and burn accounts, never caller timestamps.
pub fn allowance_resume(config:Pubkey, cfg:&DonateConfig, migration:&AccountInfo,
    burn:&AccountInfo) -> Result<Option<(i64,i64)>> {
    let mp=Pubkey::find_program_address(&[b"raydium-migration",config.as_ref()],&crate::ID).0;
    let bp=Pubkey::find_program_address(&[b"treasury-burn",config.as_ref()],&crate::ID).0;
    require!(migration.key()==mp && *migration.owner==crate::ID
        && burn.key()==bp && *burn.owner==crate::ID,DonateError::BadTreasuryBurnAccount);
    let m=RaydiumMigration::try_deserialize(&mut &migration.try_borrow_data()?[..])?;
    let b=TreasuryBurn::try_deserialize(&mut &burn.try_borrow_data()?[..])?;
    require!(m.config==config && m.completed && m.bound && m.target_pool==cfg.pool
        && b.config==config && b.treasury==cfg.treasury && b.mint==cfg.beta_mint
        && b.start_ts>m.cutoff_ts && b.start_ts<=m.resume_ts,
        DonateError::MigrationSnapshotMismatch);
    Ok(if b.start_ts<m.resume_ts {Some((m.resume_ts,b.start_ts))} else {None})
}

pub fn business_allowance_resume(config:Pubkey,cfg:&DonateConfig,remaining:&[AccountInfo],sol:bool)
    -> Result<Option<(i64,i64)>> {
    if cfg.pool!=venue::EXISTING_CLMM_POOL {return Ok(None);}
    let migration=remaining.get(if sol {1} else {3}).ok_or(DonateError::BadTreasuryBurnAccount)?;
    let burn=remaining.last().ok_or(DonateError::BadTreasuryBurnAccount)?;
    allowance_resume(config,cfg,migration,burn)
}
pub fn day_price(
    config: Pubkey,
    pool: Pubkey,
    remaining: &[AccountInfo],
    now: i64,
    spot: Option<u128>,
) -> Result<u128> {
    require!(remaining.len() >= 4, DonateError::BadVaultAccount);
    let a = &remaining[3];
    require!(
        a.key()
            == Pubkey::find_program_address(&[b"raydium-migration", config.as_ref()], &crate::ID).0
            && *a.owner == crate::ID,
        DonateError::BadVaultAccount
    );
    let mut m = RaydiumMigration::try_deserialize(&mut &a.try_borrow_data()?[..])?;
    require!(
        m.config == config && m.target_pool == pool && m.bound,
        DonateError::BadVaultAccount
    );
    let day = (now / 86400) as u32;
    if let Some(price) = spot {
        if m.open_day != day || m.open_price == 0 {
            require!(a.is_writable, DonateError::BadVaultAccount);
            m.open_day = day;
            m.open_price = price;
            m.try_serialize(&mut &mut a.try_borrow_mut_data()?[..])?;
        }
    }
    Ok(if m.open_day == day { m.open_price } else { 0 })
}

#[derive(Accounts)]
pub struct RescheduleRaydiumMigration<'info> {
    #[account(address=DEPLOYMENT)]
    pub authority: Signer<'info>,
    #[account(seeds=[b"donate-config",config.config_seed.as_ref()],bump=config.bump,has_one=authority)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(mut,seeds=[b"raydium-migration",config.key().as_ref()],bump,has_one=config)]
    pub migration: Box<Account<'info, RaydiumMigration>>,
}
pub fn reschedule(ctx: Context<RescheduleRaydiumMigration>, resume_ts: i64) -> Result<()> {
    let m = &mut ctx.accounts.migration;
    let now = Clock::get()?.unix_timestamp;
    require!(
        (ctx.accounts.config.migration_phase == CHECKPOINTING
            || ctx.accounts.config.migration_phase == RAYDIUM_WARMING)
            && !m.completed
            && resume_ts > now
            && resume_ts <= now + 86400
            && resume_ts > m.resume_ts,
        DonateError::MigrationPhase
    );
    m.resume_ts = resume_ts;
    m.checkpointed_users = 0;
    m.checkpoint_hash = [0; 32];
    Ok(())
}
