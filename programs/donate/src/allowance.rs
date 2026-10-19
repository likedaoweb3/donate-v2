use crate::*;

/// Sidecar preserves the deployed UserStake layout. Credit is USDT-e6 * TOKEN_UNIT;
/// consuming one FLOCK base unit costs exactly beta_usd credit units.
#[account]
#[derive(InitSpace, Default)]
pub struct ClaimAllowance {
    pub config: Pubkey,
    pub owner: Pubkey,
    pub version: u8,
    pub checkpoint_ts: i64,
    pub credit: u128,
    pub remainder: u128,
    pub denominator: u64,
    pub bump: u8,
}

impl ClaimAllowance {
    pub fn clear(&mut self) { self.credit = 0; self.remainder = 0; }

    pub fn checkpoint(&mut self, config: Pubkey, cfg: &DonateConfig, us: &UserStake, now: i64, bump: u8) -> Result<()> {
        self.checkpoint_with_resume(config,cfg,us,now,bump,None)
    }

    pub fn checkpoint_with_resume(&mut self, config: Pubkey, cfg: &DonateConfig, us: &UserStake,
        now: i64, bump:u8, early_resume:Option<(i64,i64)>) -> Result<()> {
        let day = cfg.day_window_secs.max(1) as u128;
        let rate = mul_bps(us.power_granted, cfg.user_daily_cap_bps);
        let exhausted = us.power == 0 || (cfg.beta_usd > 0 &&
            us.power.saturating_mul(TOKEN_UNIT) / beta_usd_with_floor(cfg, cfg.beta_usd as u128) == 0);
        if self.version == 0 {
            self.config = config; self.owner = us.owner; self.version = 1; self.bump = bump;
            let baseline = if us.last_claim_ts > 0 { us.last_claim_ts }
                else if us.first_donate_ts > 0 { us.first_donate_ts }
                else if us.imported { cfg.activation_ts } else { now };
            let elapsed = now.saturating_sub(baseline).max(0) as u128;
            // Initialize exactly the old whole-value cap, without restoring earlier resets.
            self.credit = if exhausted { 0 } else { rate.checked_mul(elapsed)
                .and_then(|v| (v / day).checked_mul(TOKEN_UNIT)).ok_or(DonateError::MathOverflow)? };
            self.remainder = 0;
        } else {
            require!((self.version == 1 || self.version == 2) && self.config == config && self.owner == us.owner && self.bump == bump,
                DonateError::Unauthorized);
            if let Some((original,effective))=early_resume {
                require!(effective>0 && effective<original && now>=effective,DonateError::BadTimeWindow);
                // Version 2 consumes the schedule marker once. Reaching the exact old
                // timestamp in a later ordinary checkpoint must never grant credit twice.
                if self.version==1 && self.checkpoint_ts==original {self.checkpoint_ts=effective;}
            }
            require!(now >= self.checkpoint_ts && self.denominator > 0, DonateError::BadTimeWindow);
            if exhausted { self.clear(); } else {
                // Parameter changes retain existing semantics for the uncheckpointed span.
                // Rescale only the sub-credit remainder when the time denominator changes.
                let remainder = self.remainder.checked_mul(day).ok_or(DonateError::MathOverflow)? / self.denominator as u128;
                let n = rate.checked_mul((now - self.checkpoint_ts) as u128)
                    .and_then(|v| v.checked_mul(TOKEN_UNIT)).and_then(|v| v.checked_add(remainder))
                    .ok_or(DonateError::MathOverflow)?;
                self.credit = self.credit.checked_add(n / day).ok_or(DonateError::MathOverflow)?;
                self.remainder = n % day;
            }
        }
        self.checkpoint_ts = now; self.denominator = day as u64;
        if early_resume.is_some() {self.version=2;}
        Ok(())
    }

    pub fn spend(&mut self, static_beta: u128, beta_usd: u128) -> Result<()> {
        let cost = static_beta.checked_mul(beta_usd).ok_or(DonateError::MathOverflow)?;
        self.credit = self.credit.checked_sub(cost).ok_or(DonateError::MathOverflow)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (DonateConfig, UserStake, Pubkey) {
        let mut c: DonateConfig = unsafe { std::mem::zeroed() };
        let mut u: UserStake = unsafe { std::mem::zeroed() };
        c.day_window_secs = 600; c.user_daily_cap_bps = 50; c.beta_usd = 1_000_000;
        u.owner = Pubkey::new_unique(); u.power = 1_000_000_000; u.power_granted = u.power; u.first_donate_ts = 100;
        (c,u,Pubkey::new_unique())
    }
    #[test] fn early_resume_preserves_credit_and_fraction_and_starts_only_at_effective_time() {
        let (c,u,k)=fixture();let mut a=ClaimAllowance::default();
        a.checkpoint(k,&c,&u,700,255).unwrap();
        let prior=a.credit;a.remainder=17;a.checkpoint_ts=2000;
        a.checkpoint_with_resume(k,&c,&u,1000,255,Some((2000,1000))).unwrap();
        assert_eq!(a.credit,prior);assert_eq!(a.remainder,17);assert_eq!(a.version,2);
        a.checkpoint_with_resume(k,&c,&u,1300,255,Some((2000,1000))).unwrap();
        let rate=mul_bps(u.power_granted,c.user_daily_cap_bps);
        assert_eq!(a.credit,prior+(rate*300*TOKEN_UNIT+17)/600);
    }
    #[test] fn early_resume_marker_is_not_reused_at_original_scheduled_second() {
        let (c,u,k)=fixture();let mut a=ClaimAllowance::default();
        a.checkpoint(k,&c,&u,700,255).unwrap();a.checkpoint_ts=2000;
        a.checkpoint_with_resume(k,&c,&u,2000,255,Some((2000,1000))).unwrap();
        let once=a.credit;
        a.checkpoint_with_resume(k,&c,&u,2000,255,Some((2000,1000))).unwrap();
        assert_eq!(a.credit,once);assert_eq!(a.version,2);
    }
    #[test] fn late_first_touch_gets_elapsed_credit_from_effective_resume() {
        let (c,u,k)=fixture();let mut a=ClaimAllowance::default();
        a.checkpoint(k,&c,&u,700,255).unwrap();let prior=a.credit;a.checkpoint_ts=2000;
        a.checkpoint_with_resume(k,&c,&u,2300,255,Some((2000,1000))).unwrap();
        assert_eq!(a.credit,prior+mul_bps(u.power_granted,c.user_daily_cap_bps)*1300*TOKEN_UNIT/600);
    }
    #[test] fn fresh_account_and_exhausted_account_do_not_regain_old_allowance() {
        let (c,mut u,k)=fixture();u.first_donate_ts=1100;
        let mut a=ClaimAllowance::default();
        a.checkpoint_with_resume(k,&c,&u,1100,255,Some((2000,1000))).unwrap();
        assert_eq!(a.credit,0);assert_eq!(a.version,2);
        u.power=0;a.credit=100;a.checkpoint_ts=2000;a.version=1;
        a.checkpoint_with_resume(k,&c,&u,1200,255,Some((2000,1000))).unwrap();
        assert_eq!(a.credit,0);assert_eq!(a.remainder,0);
    }
    #[test] fn partial_claim_carries_and_cooldown_is_independent() {
        for percent in [2500u128,5000,7500,10000] {
            let (c,mut u,k)=fixture(); let mut a=ClaimAllowance::default();
            a.checkpoint(k,&c,&u,700,255).unwrap(); let credit=a.credit;
            let taken=(credit/c.beta_usd as u128)*percent/10000;
            a.spend(taken,c.beta_usd as u128).unwrap(); u.last_claim_ts=700;
            let left=credit-taken*c.beta_usd as u128;
            a.checkpoint(k,&c,&u,700,255).unwrap(); assert_eq!(a.credit,left);
            a.checkpoint(k,&c,&u,1300,255).unwrap(); assert_eq!(a.credit,left+credit);
        }
    }

    #[test] fn production_sixty_percent_accrues_over_24h_and_carries_partial_spend() {
        let (mut c,mut u,k)=fixture();c.day_window_secs=86_400;c.user_daily_cap_bps=6000;
        let mut a=ClaimAllowance::default();a.checkpoint(k,&c,&u,100+43_200,255).unwrap();
        assert_eq!(a.credit,300_000_000*TOKEN_UNIT);
        a.spend(100_000_000*TOKEN_UNIT/1_000_000,1_000_000).unwrap();
        a.checkpoint(k,&c,&u,100+86_400,255).unwrap();
        assert_eq!(a.credit,500_000_000*TOKEN_UNIT);
        assert_eq!(u.power,1_000_000_000); // The allowance does not mint rewards or spend power.
        u.power=0;a.checkpoint(k,&c,&u,100+172_800,255).unwrap();assert_eq!(a.credit,0);
    }
    #[test] fn four_hour_claims_do_not_accelerate_daily_allowance() {
        let (mut c,u,k)=fixture(); c.day_window_secs=86_400; c.user_daily_cap_bps=6000;
        let mut frequent=ClaimAllowance::default(); let mut daily=ClaimAllowance::default();
        for step in 1..=6 { frequent.checkpoint(k,&c,&u,100+step*14_400,255).unwrap(); }
        daily.checkpoint(k,&c,&u,100+86_400,255).unwrap();
        assert_eq!(frequent.credit,daily.credit);
        let mut four=ClaimAllowance::default(); four.checkpoint(k,&c,&u,100+14_400,255).unwrap();
        assert_eq!(four.credit*6,daily.credit);
    }
    #[test] fn lazy_legacy_initialization_does_not_restore_prior_resets() {
        let (mut c,mut u,k)=fixture(); let mut a=ClaimAllowance::default();
        u.last_claim_ts=650; a.checkpoint(k,&c,&u,700,255).unwrap();
        assert_eq!(a.credit, (5_000_000u128*50/600)*TOKEN_UNIT);
        let mut b=ClaimAllowance::default(); u.first_donate_ts=0;u.last_claim_ts=0;u.imported=true;c.activation_ts=400;
        b.checkpoint(k,&c,&u,700,255).unwrap(); assert_eq!(b.credit,2_500_000*TOKEN_UNIT);
        let mut n=ClaimAllowance::default();u.imported=false;u.power=0;n.checkpoint(k,&c,&u,700,255).unwrap();assert_eq!(n.credit,0);
    }
    #[test] fn topup_checkpoints_old_power_and_exhaustion_clears() {
        let (c,mut u,k)=fixture();let mut a=ClaimAllowance::default();
        a.checkpoint(k,&c,&u,700,255).unwrap();let prior=a.credit;
        u.power_granted*=2;u.power*=2;
        a.checkpoint(k,&c,&u,1000,255).unwrap();assert_eq!(a.credit,prior*2);
        u.power=0;a.checkpoint(k,&c,&u,1600,255).unwrap();assert_eq!(a.credit,0);
        u.power=1_000_000_000;a.checkpoint(k,&c,&u,1900,255).unwrap();assert_eq!(a.credit,prior);
    }
    #[test] fn remainders_make_frequent_checkpoints_equivalent() {
        let (mut c,u,k)=fixture();c.day_window_secs=601;
        let mut a=ClaimAllowance::default();a.checkpoint(k,&c,&u,100,255).unwrap();let mut b=ClaimAllowance::default();b.checkpoint(k,&c,&u,100,255).unwrap();
        for t in 101..=701 {a.checkpoint(k,&c,&u,t,255).unwrap();}
        b.checkpoint(k,&c,&u,701,255).unwrap();assert_eq!(a.credit,b.credit);assert_eq!(a.remainder,b.remainder);
        let before=a.credit;c.user_daily_cap_bps=100;c.day_window_secs=600;
        a.checkpoint(k,&c,&u,701,255).unwrap();assert_eq!(a.credit,before);
        a.checkpoint(k,&c,&u,1301,255).unwrap();assert_eq!(a.credit,before+10_000_000*TOKEN_UNIT);
    }
    #[test] fn price_binding_and_overspend_guards() {
        let (c,mut u,k)=fixture();let mut a=ClaimAllowance::default();a.checkpoint(k,&c,&u,700,255).unwrap();
        let before=a.credit;a.spend(5,2_000_000).unwrap();assert_eq!(a.credit,before-10_000_000);
        a.spend(0,1_000_000).unwrap();assert!(a.spend(u128::MAX,2).is_err());
        assert!(a.checkpoint(Pubkey::new_unique(),&c,&u,701,255).is_err());
        u.owner=Pubkey::new_unique();assert!(a.checkpoint(k,&c,&u,701,255).is_err());
    }
}
