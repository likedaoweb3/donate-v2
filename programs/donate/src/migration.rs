//! Fresh-deployment import protocol. The ordered hash binds every imported edge and power.
use super::*;
use anchor_lang::solana_program::hash::hashv;

pub const IMPORTING: u8 = 0;
pub const REBUILDING: u8 = 1;
pub const READY: u8 = 2;
pub const ACTIVE: u8 = 3;
pub const ABORTING: u8 = 4;
pub const ABORTED: u8 = 5;
pub const MAX_ABORT_CLOSE_BATCH: usize = 16;

pub fn active(cfg: &DonateConfig) -> Result<()> {
    require!(cfg.migration_phase == ACTIVE && Clock::get()?.unix_timestamp >= cfg.last_update_ts, DonateError::MigrationPhase);
    Ok(())
}

pub fn commitment(
    ctx: Context<AdminConfig>,
    slot: u64,
    digest: [u8; 32],
    ordered_hash: [u8; 32],
    count: u32,
    power: u128,
) -> Result<()> {
    let c = &mut ctx.accounts.config;
    require!(c.startup_mode == StartupMode::Migration, DonateError::MigrationPhase);
    require!(
        c.migration_phase == IMPORTING && !c.manifest_set,
        DonateError::MigrationPhase
    );
    require!(
        slot > 0 && count > 0 && digest != [0; 32] && ordered_hash != [0; 32],
        DonateError::MigrationSnapshotMismatch
    );
    require!(
        power
            <= USER_POWER_CAP
                .checked_mul(count as u128)
                .ok_or(DonateError::MathOverflow)?,
        DonateError::PowerCapExceeded
    );
    c.snapshot_slot = slot;
    c.snapshot_digest = digest;
    c.expected_import_hash = ordered_hash;
    c.expected_user_count = count;
    c.expected_power = power;
    c.manifest_set = true;
    Ok(())
}

pub fn seed(
    ctx: Context<AdminSeedUser>,
    owner: Pubkey,
    referrer: Pubkey,
    power: u128,
    power_granted: u128,
) -> Result<()> {
    let c = &mut ctx.accounts.config;
    require!(c.startup_mode == StartupMode::Migration, DonateError::MigrationPhase);
    require!(
        c.migration_phase == IMPORTING && c.manifest_set && !c.seeding_locked,
        DonateError::MigrationPhase
    );
    require!(
        power == power_granted && power <= USER_POWER_CAP,
        DonateError::PowerCapExceeded
    );
    require_keys_neq!(owner, Pubkey::default(), DonateError::BadChain);
    require_keys_neq!(referrer, Pubkey::default(), DonateError::BadChain);
    let u = &mut ctx.accounts.user_stake;
    if u.imported {
        require!(
            u.owner == owner
                && u.referrer == referrer
                && u.power == power
                && u.power_granted == power,
            DonateError::MigrationSnapshotMismatch
        );
        return Ok(());
    }
    require!(
        c.seeded_user_count < c.expected_user_count,
        DonateError::MigrationSnapshotMismatch
    );
    if owner == c.root_referrer {
        require!(
            owner == referrer && c.seeded_user_count == 0,
            DonateError::BadChain
        );
    } else {
        require_keys_neq!(owner, referrer, DonateError::SelfReferral);
        let ai = &ctx.accounts.referrer_stake;
        require_keys_eq!(*ai.owner, crate::ID, DonateError::BadChain);
        let expected = Pubkey::find_program_address(
            &[b"donate-user", c.key().as_ref(), referrer.as_ref()],
            &crate::ID,
        )
        .0;
        require_keys_eq!(ai.key(), expected, DonateError::BadChain);
        let mut parent = UserStake::try_deserialize(&mut &ai.try_borrow_data()?[..])?;
        require!(
            parent.imported && parent.owner == referrer,
            DonateError::BadChain
        );
        parent.direct_count = parent
            .direct_count
            .checked_add(1)
            .ok_or(DonateError::MathOverflow)?;
        u.referral_index = parent.direct_count;
        parent.try_serialize(&mut &mut ai.try_borrow_mut_data()?[..])?;
    }
    u.owner = owner;
    u.referrer = referrer;
    u.bump = ctx.bumps.user_stake;
    u.power = power;
    u.power_granted = power;
    u.imported = true;
    u.flags = FLAG_LEG_SEEDED;
    c.total_power = c
        .total_power
        .checked_add(power)
        .ok_or(DonateError::MathOverflow)?;
    require!(
        c.total_power <= c.expected_power,
        DonateError::MigrationSnapshotMismatch
    );
    c.seeded_user_count += 1;
    c.import_hash = hashv(&[
        &c.import_hash,
        owner.as_ref(),
        referrer.as_ref(),
        &power.to_le_bytes(),
    ])
    .to_bytes();
    Ok(())
}

pub fn begin_rebuild(ctx: Context<AdminConfig>) -> Result<()> {
    let c = &mut ctx.accounts.config;
    require!(c.startup_mode == StartupMode::Migration, DonateError::MigrationPhase);
    require!(
        c.migration_phase == IMPORTING && c.manifest_set,
        DonateError::MigrationPhase
    );
    require!(
        c.seeded_user_count == c.expected_user_count
            && c.total_power == c.expected_power
            && c.import_hash == c.expected_import_hash,
        DonateError::MigrationSnapshotMismatch
    );
    c.migration_phase = REBUILDING;
    Ok(())
}

/// Children are supplied bottom-up, in increasing pubkey byte order, across bounded batches.
/// Count + strict ordering prevents omission, duplicates and empty-list erasure.
pub fn rebuild(ctx: Context<RebuildUser>) -> Result<()> {
    let c = &mut ctx.accounts.config;
    require!(c.startup_mode == StartupMode::Migration, DonateError::MigrationPhase);
    require!(c.migration_phase == REBUILDING, DonateError::MigrationPhase);
    let u = &mut ctx.accounts.user_stake;
    require!(u.imported, DonateError::BadChain);
    if u.rebuilt {
        return Ok(());
    }
    for ai in ctx.remaining_accounts {
        require_keys_eq!(*ai.owner, crate::ID, DonateError::BadChain);
        require!(
            ai.key().to_bytes() > u.rebuild_cursor.to_bytes(),
            DonateError::BadChain
        );
        let child = UserStake::try_deserialize(&mut &ai.try_borrow_data()?[..])?;
        let expected = Pubkey::find_program_address(
            &[b"donate-user", c.key().as_ref(), child.owner.as_ref()],
            &crate::ID,
        )
        .0;
        require_keys_eq!(ai.key(), expected, DonateError::BadChain);
        require!(
            child.rebuilt && child.referrer == u.owner && child.owner != u.owner,
            DonateError::BadChain
        );
        require!(u.rebuilt_children < u.direct_count, DonateError::BadChain);
        for depth in 0..MAX_UPLINE {
            let value = if depth == 0 {
                child.power
            } else {
                child.descendant_power[depth - 1]
            };
            u.descendant_power[depth] = u.descendant_power[depth]
                .checked_add(value)
                .ok_or(DonateError::MathOverflow)?;
        }
        u.max_leg = u.max_leg.max(child.leg_power);
        if child.is_valid {
            u.direct_valid_count = u
                .direct_valid_count
                .checked_add(1)
                .ok_or(DonateError::MathOverflow)?;
        }
        if child.level >= STAR_COUNT_LEVEL {
            u.direct_ge6 = u.direct_ge6.saturating_add(1);
        }
        u.rebuild_cursor = ai.key();
        u.rebuilt_children += 1;
    }
    if u.rebuilt_children == u.direct_count {
        u.direct_power = u.descendant_power[0];
        u.team_power = u
            .descendant_power
            .iter()
            .try_fold(0u128, |a, b| a.checked_add(*b))
            .ok_or(DonateError::MathOverflow)?;
        let leg = u.descendant_power[..LEG_UPLINE]
            .iter()
            .try_fold(u.power, |a, b| a.checked_add(*b))
            .ok_or(DonateError::MathOverflow)?;
        u.leg_power = u64::try_from(leg).map_err(|_| error!(DonateError::MathOverflow))?;
        u.synced_power = u.power;
        u.is_valid = u.power >= c.valid_invite_min_power;
        u.level = level_of(u, &LevelThresholds::of(c));
        u.v_tier = u.level;
        if let Some(i) = v_idx(u.level) {
            c.total_v_weight[i] += 1;
        }
        u.rebuilt = true;
        c.rebuilt_user_count = c
            .rebuilt_user_count
            .checked_add(1)
            .ok_or(DonateError::MathOverflow)?;
    }
    Ok(())
}

pub fn finalize(ctx: Context<AdminConfig>) -> Result<()> {
    let c = &mut ctx.accounts.config;
    require!(c.startup_mode == StartupMode::Migration, DonateError::MigrationPhase);
    require!(c.migration_phase == REBUILDING, DonateError::MigrationPhase);
    require!(
        c.rebuilt_user_count == c.expected_user_count
            && c.seeded_user_count == c.expected_user_count
            && c.total_power == c.expected_power
            && c.import_hash == c.expected_import_hash,
        DonateError::MigrationSnapshotMismatch
    );
    c.seeding_locked = true;
    c.migration_phase = READY;
    Ok(())
}

pub fn activate(ctx: Context<Activate>) -> Result<()> {
    // Bind the loader Program state to canonical ProgramData. Upgrade authority may
    // remain present: revoking it is an independent, optional administrator action.
    {
        let data = ctx.accounts.locker_program.try_borrow_data()?;
        require!(
            data.len() == 36
                && data[..4] == 2u32.to_le_bytes()
                && data[4..36] == ctx.accounts.locker_program_data.key().to_bytes(),
            DonateError::LockerNotFinal
        );
    }
    let c = &mut ctx.accounts.config;
    require!(
        c.migration_phase == READY && c.seeding_locked,
        DonateError::MigrationPhase
    );
    let now = Clock::get()?.unix_timestamp;
    require!(now > 0, DonateError::BadTimeWindow);
    oracle::fresh(c, now)?;
    c.activation_ts = now;
    c.last_update_ts = now;
    c.v_last_update_ts = now;
    c.migration_phase = ACTIVE;
    Ok(())
}

/// Irreversibly freeze a failed pre-activation migration so its UserStake
/// accounts can be closed safely before the program itself is closed.
pub fn begin_abort(ctx: Context<BeginMigrationAbort>) -> Result<()> {
    let c = &mut ctx.accounts.config;
    let root = &ctx.accounts.root_user;
    require!(
        c.startup_mode == StartupMode::Migration
            && matches!(c.migration_phase, IMPORTING | REBUILDING | READY),
        DonateError::MigrationPhase
    );

    // The root is created with config. Once any import succeeds it must be the
    // first seeded account; before that it is the only UserStake that exists.
    if c.seeded_user_count == 0 {
        require!(!root.imported, DonateError::MigrationSnapshotMismatch);
    } else {
        require!(
            root.imported && root.owner == c.root_referrer,
            DonateError::MigrationSnapshotMismatch
        );
    }

    let expected_close_count = c.seeded_user_count.max(1);
    c.seeding_locked = true;
    // These two migration counters are no longer needed after the irreversible
    // phase transition. Reuse them to make omission and early finalization
    // impossible without changing the stable DonateConfig account layout.
    c.expected_user_count = expected_close_count;
    c.rebuilt_user_count = 0;
    c.migration_phase = ABORTING;
    emit!(MigrationAbortStartedEvent {
        config: c.key(),
        authority: ctx.accounts.authority.key(),
        expected_user_count: expected_close_count,
    });
    Ok(())
}

/// Close a bounded, fully validated batch. All checks run before the first
/// account is closed, so any malformed/substituted/duplicate account rolls the
/// whole transaction back. Rent always goes to the current config authority.
pub fn close_abort_users<'info>(
    ctx: Context<'_, '_, 'info, 'info, CloseMigrationUsers<'info>>,
) -> Result<()> {
    let c = &mut ctx.accounts.config;
    require!(c.migration_phase == ABORTING, DonateError::MigrationPhase);
    require!(
        !ctx.remaining_accounts.is_empty(),
        DonateError::EmptyCloseBatch
    );
    require!(
        ctx.remaining_accounts.len() <= MAX_ABORT_CLOSE_BATCH,
        DonateError::CloseBatchTooLarge
    );

    let config_key = c.key();
    let mut refunded_lamports = 0u64;
    for (index, ai) in ctx.remaining_accounts.iter().enumerate() {
        require!(ai.is_writable, DonateError::InvalidCloseAccount);
        require_keys_eq!(*ai.owner, crate::ID, DonateError::InvalidCloseAccount);
        require!(
            ctx.remaining_accounts[..index]
                .iter()
                .all(|previous| previous.key() != ai.key()),
            DonateError::DuplicateCloseAccount
        );

        let stake = UserStake::try_deserialize(&mut &ai.try_borrow_data()?[..])?;
        let (expected, bump) = Pubkey::find_program_address(
            &[b"donate-user", config_key.as_ref(), stake.owner.as_ref()],
            &crate::ID,
        );
        require_keys_eq!(ai.key(), expected, DonateError::InvalidCloseAccount);
        require!(stake.bump == bump, DonateError::InvalidCloseAccount);
        require!(
            stake.imported
                || (!stake.imported
                    && stake.owner == c.root_referrer
                    && c.expected_user_count == 1),
            DonateError::InvalidCloseAccount
        );
        refunded_lamports = refunded_lamports
            .checked_add(ai.lamports())
            .ok_or(DonateError::MathOverflow)?;
    }

    let batch_count = u32::try_from(ctx.remaining_accounts.len())
        .map_err(|_| error!(DonateError::MathOverflow))?;
    let closed_user_count = c
        .rebuilt_user_count
        .checked_add(batch_count)
        .ok_or(DonateError::MathOverflow)?;
    require!(
        closed_user_count <= c.expected_user_count,
        DonateError::MigrationAbortCountMismatch
    );

    let destination = ctx.accounts.authority.to_account_info();
    for ai in ctx.remaining_accounts {
        let destination_lamports = destination
            .lamports()
            .checked_add(ai.lamports())
            .ok_or(DonateError::MathOverflow)?;
        **destination.try_borrow_mut_lamports()? = destination_lamports;
        **ai.try_borrow_mut_lamports()? = 0;
        ai.assign(&anchor_lang::solana_program::system_program::ID);
        ai.resize(0)?;
    }
    c.rebuilt_user_count = closed_user_count;
    emit!(MigrationUsersClosedEvent {
        config: config_key,
        authority: ctx.accounts.authority.key(),
        batch_count,
        refunded_lamports,
        closed_user_count,
        expected_user_count: c.expected_user_count,
    });
    Ok(())
}

pub fn finalize_abort(ctx: Context<FinalizeMigrationAbort>) -> Result<()> {
    let c = &mut ctx.accounts.config;
    require!(c.migration_phase == ABORTING, DonateError::MigrationPhase);
    require!(
        c.rebuilt_user_count == c.expected_user_count,
        DonateError::MigrationAbortIncomplete
    );
    c.migration_phase = ABORTED;
    emit!(MigrationAbortFinalizedEvent {
        config: c.key(),
        authority: ctx.accounts.authority.key(),
        closed_user_count: c.rebuilt_user_count,
    });
    Ok(())
}

#[derive(Accounts)]
pub struct RebuildUser<'info> {
    pub authority: Signer<'info>,
    #[account(mut, seeds=[b"donate-config",config.config_seed.as_ref()], bump=config.bump, has_one=authority)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(mut, seeds=[b"donate-user",config.key().as_ref(),user_stake.owner.as_ref()], bump=user_stake.bump)]
    pub user_stake: Box<Account<'info, UserStake>>,
}

#[derive(Accounts)]
pub struct BeginMigrationAbort<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"donate-config", config.config_seed.as_ref()],
        bump = config.bump,
        has_one = authority @ DonateError::Unauthorized,
    )]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(
        seeds = [b"donate-user", config.key().as_ref(), config.root_referrer.as_ref()],
        bump = root_user.bump,
    )]
    pub root_user: Box<Account<'info, UserStake>>,
}

#[derive(Accounts)]
pub struct CloseMigrationUsers<'info> {
    #[account(mut)]
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
pub struct FinalizeMigrationAbort<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"donate-config", config.config_seed.as_ref()],
        bump = config.bump,
        has_one = authority @ DonateError::Unauthorized,
    )]
    pub config: Box<Account<'info, DonateConfig>>,
}

/// Seal a newly configured empty system. No snapshot, import, or reward balance is permitted.
pub fn finalize_fresh(ctx: Context<FinalizeFresh>) -> Result<()> {
    let c = &mut ctx.accounts.config;
    let u = &ctx.accounts.root_user;
    require!(c.startup_mode == StartupMode::Fresh && c.migration_phase == IMPORTING && c.seeding_locked,
        DonateError::MigrationPhase);
    require!(
        !c.manifest_set && c.seeded_user_count == 0 && c.rebuilt_user_count == 0
        && c.expected_user_count == 0 && c.expected_power == 0 && c.total_power == 0
        && c.import_hash == [0; 32] && c.acc_reward_per_power == 0
        && c.total_v_weight == [0; V_TIERS] && c.acc_per_v == [0; V_TIERS]
        && u.owner == c.root_referrer && u.referrer == c.root_referrer
        && !u.imported && !u.rebuilt && u.power == 0 && u.power_granted == 0
        && u.pending == 0 && u.promo_pending == 0 && u.v_pending == 0
        && u.direct_count == 0 && u.first_donate_ts == 0 && u.last_claim_ts == 0,
        DonateError::MigrationSnapshotMismatch
    );
    c.migration_phase = READY;
    Ok(())
}

#[derive(Accounts)]
pub struct FinalizeFresh<'info> {
    pub authority: Signer<'info>,
    #[account(mut, seeds=[b"donate-config",config.config_seed.as_ref()], bump=config.bump,
        has_one=authority @ DonateError::Unauthorized)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(seeds=[b"donate-user",config.key().as_ref(),config.root_referrer.as_ref()], bump=root_user.bump)]
    pub root_user: Box<Account<'info, UserStake>>,
}
