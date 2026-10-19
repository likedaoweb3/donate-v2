//! Daily LIKER treasury burn. Existing persisted reward/user layouts are untouched.
use crate::*;

pub const INTERVAL: i64 = 86_400;
pub const MAX_PERIODS: u64 = 3_650;
/// 1,000,000 LKR, whose canonical mint has 9 decimals. Daily burns never cross this floor.
pub const MIN_TREASURY_BALANCE: u64 = 1_000_000_000_000_000;
/// Leave compute headroom for the following swap, position creation and uplines.
pub const MAX_BUSINESS_PERIODS: i64 = 180;
const SCALE: u128 = 1_000_000_000_000;

#[account]
#[derive(InitSpace)]
pub struct TreasuryBurn {
    pub config: Pubkey,
    pub treasury: Pubkey,
    pub mint: Pubkey,
    pub start_ts: i64,
    pub last_burn_ts: i64,
    pub total_burned: u128,
}

#[derive(Accounts)]
pub struct CrankTreasuryBurn<'info> {
    #[account(mut,seeds=[b"donate-config",config.config_seed.as_ref()],bump=config.bump)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(mut,seeds=[b"treasury-burn",config.key().as_ref()],bump,has_one=config)]
    pub treasury_burn: Box<Account<'info, TreasuryBurn>>,
    #[account(mut,address=config.treasury,token::mint=beta_mint,token::authority=config)]
    pub treasury: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut,address=config.beta_mint,constraint=beta_mint.key()==venue::LEGACY_LIKER @ DonateError::WrongMint)]
    pub beta_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(address=anchor_spl::token::ID)]
    pub token_program: Interface<'info, TokenInterface>,
}

/// Preserve fixed-point compound/rounding above the floor and cap burns at the excess.
/// At/below the floor (or on reaching it), retire all elapsed periods without burn debt.
pub fn compute(amount: u64, elapsed: i64) -> (u64, u64) {
    if elapsed < INTERVAL { return (0, 0); }
    let due = (elapsed / INTERVAL) as u64;
    if amount <= MIN_TREASURY_BALANCE { return (due, 0); }
    let periods = due.min(MAX_PERIODS);
    let mut factor = SCALE;
    for _ in 0..periods { factor = factor * 9_900 / 10_000; }
    let remain = (((amount as u128) * factor / SCALE) as u64).max(MIN_TREASURY_BALANCE);
    (if remain == MIN_TREASURY_BALANCE { due } else { periods }, amount - remain)
}

pub fn crank(ctx: Context<CrankTreasuryBurn>) -> Result<()> {
    settle(&mut ctx.accounts.config, &mut ctx.accounts.treasury,
        &ctx.accounts.beta_mint.to_account_info(), &ctx.accounts.token_program.to_account_info(),
        &ctx.accounts.treasury_burn.to_account_info(), true)?;
    // Anchor exit must serialize the updated state, not the pre-CPI decoded copy.
    ctx.accounts.treasury_burn.reload()?;
    Ok(())
}

/// Business ABIs retain their existing fixed accounts. CLMM builders append the canonical
/// writable burn PDA LAST, after venue extras and uplines. SOL additionally appends its mint.
pub fn business<'info>(cfg: &mut Account<'info, DonateConfig>,
    treasury: &mut InterfaceAccount<'info, TokenAccount>, mint: &AccountInfo<'info>,
    token: &AccountInfo<'info>, remaining: &[AccountInfo<'info>]) -> Result<()> {
    if cfg.pool != venue::EXISTING_CLMM_POOL { return Ok(()); }
    let state = remaining.last().ok_or(DonateError::BadTreasuryBurnAccount)?;
    settle(cfg, treasury, mint, token, state, false)
}

fn settle<'info>(cfg: &mut Account<'info, DonateConfig>,
    treasury: &mut InterfaceAccount<'info, TokenAccount>, mint: &AccountInfo<'info>,
    token: &AccountInfo<'info>, state: &AccountInfo<'info>, standalone: bool) -> Result<()> {
    migration::active(cfg)?;
    require_keys_eq!(cfg.pool, venue::EXISTING_CLMM_POOL, DonateError::MigrationPhase);
    let key = cfg.key();
    let expected = Pubkey::find_program_address(&[b"treasury-burn",key.as_ref()], &crate::ID).0;
    require!(state.key()==expected && *state.owner==crate::ID && state.is_writable,
        DonateError::BadTreasuryBurnAccount);
    require!(treasury.key()==cfg.treasury && treasury.mint==cfg.beta_mint && treasury.owner==key
        && *treasury.to_account_info().owner==anchor_spl::token::ID,
        DonateError::BadTreasuryBurnAccount);
    require!(mint.key()==cfg.beta_mint && mint.key()==venue::LEGACY_LIKER
        && *mint.owner==anchor_spl::token::ID && mint.is_writable
        && token.key()==anchor_spl::token::ID, DonateError::BadTreasuryBurnAccount);
    let parsed = Mint::try_deserialize(&mut &mint.try_borrow_data()?[..])?;
    require!(parsed.decimals==9, DonateError::BadDecimals);
    let mut s = TreasuryBurn::try_deserialize(&mut &state.try_borrow_data()?[..])?;
    require!(s.config==key && s.treasury==cfg.treasury && s.mint==cfg.beta_mint
        && s.last_burn_ts>=s.start_ts, DonateError::BadTreasuryBurnAccount);
    let now = Clock::get()?.unix_timestamp;
    let elapsed = now.saturating_sub(s.last_burn_ts);
    if !standalone && treasury.amount>MIN_TREASURY_BALANCE {
        require!(elapsed/INTERVAL<=MAX_BUSINESS_PERIODS, DonateError::TreasuryBurnCatchUpRequired);
    }
    let (periods, burned) = compute(treasury.amount, elapsed);
    if standalone { require!(periods>0, DonateError::TreasuryBurnNotDue); }
    if periods==0 { return Ok(()); }
    let through = s.last_burn_ts.checked_add((periods as i64).checked_mul(INTERVAL)
        .ok_or(DonateError::MathOverflow)?).ok_or(DonateError::MathOverflow)?;
    if !standalone { require!(now.saturating_sub(through)<INTERVAL, DonateError::TreasuryBurnCatchUpRequired); }
    // Preserve settlement-time revaluation: settle elapsed rewards on the pre-burn balance.
    accrue_global(cfg, treasury.amount, now);
    accrue_v(cfg, treasury.amount, now);
    if burned>0 {
        let seed=cfg.config_seed; let bump=cfg.bump;
        let signer: &[&[&[u8]]] = &[&[b"donate-config",seed.as_ref(),&[bump]]];
        burn(CpiContext::new_with_signer(token.clone(), Burn {
            mint:mint.clone(), from:treasury.to_account_info(), authority:cfg.to_account_info(),
        }, signer), burned)?;
        treasury.reload()?;
    }
    s.last_burn_ts=through;
    s.total_burned=s.total_burned.checked_add(burned as u128).ok_or(DonateError::MathOverflow)?;
    s.try_serialize(&mut &mut state.try_borrow_mut_data()?[..])?;
    emit!(TreasuryBurnEvent { config:key, mint:mint.key(), periods, amount:burned,
        treasury_after:treasury.amount, settled_through:through, total_burned:s.total_burned });
    Ok(())
}

#[event]
pub struct TreasuryBurnEvent {
    pub config: Pubkey, pub mint: Pubkey, pub periods: u64, pub amount: u64,
    pub treasury_after: u64, pub settled_through: i64, pub total_burned: u128,
}

#[cfg(test)]
mod tests {
    use super::*;
    const FLOOR: u64 = MIN_TREASURY_BALANCE;
    #[test] fn boundary_and_rounding() {
        for elapsed in [i64::MIN,-1,0,INTERVAL-1] { assert_eq!(compute(FLOOR*2,elapsed),(0,0)); }
        for amount in [0,1,99,100,101,FLOOR-1,FLOOR] {
            assert_eq!(compute(amount,INTERVAL),(1,0));
        }
        assert_eq!(compute(FLOOR+1,INTERVAL),(1,1));
        assert_eq!(compute(FLOOR+1_000_000_000,INTERVAL),(1,1_000_000_000));
        assert_eq!(compute(FLOOR*2+1,INTERVAL),(1,FLOOR*2/100+1));
    }
    #[test] fn reaches_floor_and_stops_next_day() {
        let amount=FLOOR+1_000_000_000;
        let (_,first)=compute(amount,INTERVAL);
        let balance=amount-first;
        assert_eq!(balance,FLOOR);
        assert_eq!(compute(balance,INTERVAL),(1,0));
        assert_eq!(compute(balance,INTERVAL*10_000),(10_000,0));
    }
    #[test] fn protected_balances_retire_elapsed_periods_without_funding_debt() {
        for amount in [0,1,FLOOR-1,FLOOR] {
            assert_eq!(compute(amount,INTERVAL*10_000),(10_000,0));
        }
        for amount in [FLOOR+1,FLOOR*2,u64::MAX] {
            assert_eq!(compute(amount,INTERVAL*10_000),(10_000,amount-FLOOR));
        }
        let start=100;let now=start+INTERVAL*10_000+17;
        let (n,_)=compute(FLOOR,now-start);let cursor=start+n as i64*INTERVAL;
        assert_eq!(compute(FLOOR*2,now-cursor),(0,0));
        assert_eq!(compute(FLOOR*2,now+INTERVAL-cursor),(1,FLOOR*2/100));
    }
    #[test] fn compound_preserves_rounding_and_never_crosses_floor() {
        for amount in [0,1,99,101,FLOOR-1,FLOOR,FLOOR+123,FLOOR*2,u64::MAX] {
            for days in [1,2,10,365,3650,10_000] {
                let mut factor=SCALE;
                for _ in 0..days.min(MAX_PERIODS) { factor=factor*9900/10000; }
                let retained=(((amount as u128)*factor/SCALE) as u64).max(FLOOR).min(amount);
                let (n,b)=compute(amount,days as i64*INTERVAL);
                assert_eq!(b,amount-retained);
                assert_eq!(n,if retained<=FLOOR { days } else { days.min(MAX_PERIODS) });
                assert!(b<=amount.saturating_sub(FLOOR));
            }
        }
    }
    #[test] fn clock_cursor_keeps_remainder_and_idempotence() {
        let start=1234; let now=start+INTERVAL*3+77;
        let (n,b)=compute(FLOOR*2,now-start);
        assert!(b>0); assert_eq!(n,3);
        let cursor=start+n as i64*INTERVAL;
        assert_eq!(now-cursor,77); assert_eq!(compute(FLOOR*2-b,now-cursor),(0,0));
    }
    #[test] fn reward_settlement_preserves_pending_basis() {
        let mut cfg=DonateConfig::try_deserialize_unchecked(&mut &vec![0;8+DonateConfig::INIT_SPACE][..]).unwrap();
        cfg.migration_phase=migration::ACTIVE;cfg.total_power=ACC_SCALE;
        cfg.daily_emission_bps=100;cfg.day_window_secs=INTERVAL;
        let balance=FLOOR*2-1;
        accrue_global(&mut cfg,balance,INTERVAL);
        let before=cfg.acc_reward_per_power;
        let (_,burned)=compute(balance,INTERVAL);
        accrue_global(&mut cfg,balance-burned,INTERVAL);
        assert_eq!(cfg.acc_reward_per_power,before);
        accrue_global(&mut cfg,balance-burned,INTERVAL*2);
        assert_eq!(cfg.acc_reward_per_power-before,((balance-burned) as u128+99)/100);
    }
}
