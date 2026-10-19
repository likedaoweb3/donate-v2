//! A keeper triggers reads, never supplies prices. External prices are sampled, not a trade TWAP.
use super::*;
pub const WINDOW: i64 = 300;
pub const MIN_INTERVAL: i64 = 60;
pub const MAX_GAP: i64 = 120;
pub const DEFAULT_DEVIATION_BPS: u16 = 10_000;
pub const LEGACY_FALLBACK_DEVIATION_BPS: u16 = 1000;

/// Invalid legacy values retain the original mandatory 10% protection.
pub fn effective_deviation_bps(value: u16) -> u16 {
    if (1..=10_000).contains(&value) { value } else { LEGACY_FALLBACK_DEVIATION_BPS }
}
pub const SAMPLES: usize = 32; // Keep the deployed account layout unchanged.
pub const MIN_SAMPLES: u8 = (WINDOW / MIN_INTERVAL + 1) as u8;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, InitSpace)]
pub struct Observation {
    pub ts: i64,
    pub cumulative_sol: u128,
    pub cumulative_like: u128,
    pub cumulative_dex: u128,
}

#[account]
#[derive(InitSpace)]
pub struct PriceObservation {
    pub config: Pubkey,
    pub last_slot: u64,
    pub last_ts: i64,
    pub last_sol: u128,
    pub last_like: u128,
    pub last_dex: u128,
    pub cumulative_sol: u128,
    pub cumulative_like: u128,
    pub count: u8,
    pub next: u8,
    pub breaker: bool,
    pub observations: [Observation; SAMPLES],
}

pub fn within(spot: u128, average: u128, deviation_bps: u16) -> bool {
    let bps = effective_deviation_bps(deviation_bps) as u128;
    // Exact floor(average * bps / 10000), without overflowing at large prices.
    let band = (average / 10_000) * bps + (average % 10_000) * bps / 10_000;
    average > 0 && spot > 0 && spot.abs_diff(average) <= band
}

impl PriceObservation {
    pub fn default_state()->Self { Self {config:Pubkey::default(),last_slot:0,last_ts:0,last_sol:0,last_like:0,last_dex:0,cumulative_sol:0,cumulative_like:0,count:0,next:0,breaker:false,observations:[Observation::default();SAMPLES]} }
    /// The CLMM pool publishes sqrtPriceX64, not a CPMM-style price integral.
    /// Integrate the last keeper-observed spot in the existing observation ring
    /// without changing its deployed layout. The six-sample warm-up bounds the
    /// window and rejects stale samples through `record_for_venue`.
    pub fn clmm_integral(&self, now: i64) -> Result<u128> {
        if self.count == 0 { return Ok(0); }
        require!(now >= self.last_ts, DonateError::BadTimeWindow);
        let prior = self.observations[(self.next as usize + SAMPLES - 1) % SAMPLES];
        prior.cumulative_dex.checked_add(
            self.last_dex.checked_mul((now - self.last_ts) as u128).ok_or(DonateError::MathOverflow)?
        ).ok_or_else(|| error!(DonateError::MathOverflow))
    }
    /// None means warming/broken. Return success to persist a breaker; reverting would lose it.
    pub fn record(&mut self,deviation_bps:u16,now:i64,slot:u64,sol:u128,like:u128,dex:u128,dex_integral:u128)->Result<Option<[u128;3]>> {self.record_for_venue(deviation_bps,now,slot,sol,like,dex,dex_integral,false)}
    pub fn record_for_venue(
        &mut self,
        deviation_bps: u16,
        now: i64,
        slot: u64,
        sol: u128,
        like: u128,
        dex: u128,
        dex_integral: u128,
        dex_q32: bool,
    ) -> Result<Option<[u128; 3]>> {
        require!(now > 0 && now >= self.last_ts, DonateError::BadTimeWindow);
        if self.count > 0 && (slot == self.last_slot || now - self.last_ts < MIN_INTERVAL) {
            return err!(DonateError::SyncTooSoon);
        }
        let abnormal = sol == 0
            || like == 0
            || dex == 0
            || (self.count > 0
                && (!within(sol, self.last_sol, deviation_bps)
                    || !within(like, self.last_like, deviation_bps)
                    || !within(dex, self.last_dex, deviation_bps)));
        let gap = self.count > 0 && now - self.last_ts > MAX_GAP;
        if abnormal || gap {
            self.count = 0;
            self.next = 0;
            self.cumulative_sol = 0;
            self.cumulative_like = 0;
            self.breaker = abnormal;
            self.observations = [Observation::default(); SAMPLES];
        }
        if self.count > 0 {
            let dt = (now - self.last_ts) as u128;
            self.cumulative_sol = self
                .cumulative_sol
                .checked_add(
                    self.last_sol
                        .checked_mul(dt)
                        .ok_or(DonateError::MathOverflow)?,
                )
                .ok_or(DonateError::MathOverflow)?;
            self.cumulative_like = self
                .cumulative_like
                .checked_add(
                    self.last_like
                        .checked_mul(dt)
                        .ok_or(DonateError::MathOverflow)?,
                )
                .ok_or(DonateError::MathOverflow)?;
        }
        self.last_ts = now;
        self.last_slot = slot;
        self.last_sol = sol;
        self.last_like = like;
        self.last_dex = dex;
        if sol == 0 || like == 0 || dex == 0 {
            return Ok(None);
        }
        let current = Observation {
            ts: now,
            cumulative_sol: self.cumulative_sol,
            cumulative_like: self.cumulative_like,
            cumulative_dex: dex_integral,
        };
        self.observations[self.next as usize] = current;
        self.next = (self.next + 1) % SAMPLES as u8;
        self.count = self.count.saturating_add(1).min(SAMPLES as u8);
        let baseline = self
            .observations
            .iter()
            .filter(|p| p.ts > 0 && now - p.ts >= WINDOW && now - p.ts <= WINDOW + MAX_GAP)
            .max_by_key(|p| p.ts)
            .copied();
        // After a reset, stale entries are excluded by requiring a complete count/window below.
        if self.count < MIN_SAMPLES {
            return Ok(None);
        }
        if let Some(old) = baseline {
            // Both cumulative external counters reset to zero; old-cycle entries cannot contribute.
            if old.cumulative_sol > current.cumulative_sol
                || old.cumulative_like > current.cumulative_like
            {
                return Ok(None);
            }
            let dt = (now - old.ts) as u128;
            // DEX's lifetime counter is modulo 2^128. A valid window is at most 420 seconds;
            // even (u64::MAX * PRICE_SCALE) * 420 fits in u128, so this modular delta is exact.
            let avg = [
                (current.cumulative_sol - old.cumulative_sol) / dt,
                (current.cumulative_like - old.cumulative_like) / dt,
                {let avg=current.cumulative_dex.wrapping_sub(old.cumulative_dex)/dt; if dex_q32 {avg.checked_mul(PRICE_SCALE).ok_or(DonateError::MathOverflow)?/(1u128<<32)} else {avg}},
            ];
            if within(sol, avg[0], deviation_bps) && within(like, avg[1], deviation_bps) && within(dex, avg[2], deviation_bps) {
                self.breaker = false;
                return Ok(Some(avg));
            }
            self.count = 0;
            self.next = 0;
            self.observations = [Observation::default(); SAMPLES];
            self.cumulative_sol = 0;
            self.cumulative_like = 0;
            self.breaker = true;
        }
        Ok(None)
    }
}

pub fn fresh(c: &DonateConfig, now: i64) -> Result<()> {
    require!(
        c.price_ready && now >= c.last_sync_ts && now - c.last_sync_ts <= MAX_GAP,
        DonateError::PriceUnavailable
    );
    Ok(())
}
pub fn pool_guard(c: &DonateConfig, base: u64, quote: u64, now: i64) -> Result<()> {
    fresh(c, now)?;
    require!(
        quote > 0
            && within(
                base as u128 * PRICE_SCALE / (quote.max(1) as u128),
                c.pool_ratio_anchor,
                c.max_sync_deviation_bps,
            ),
        DonateError::PoolPriceDeviation
    );
    Ok(())
}

#[derive(Accounts)]
pub struct InitializeObservation<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(seeds=[b"donate-config",config.config_seed.as_ref()],bump=config.bump,has_one=authority)]
    pub config: Box<Account<'info, DonateConfig>>,
    #[account(init,payer=authority,space=8+PriceObservation::INIT_SPACE,seeds=[b"price-observation",config.key().as_ref()],bump)]
    pub observation: Box<Account<'info, PriceObservation>>,
    pub system_program: Program<'info, System>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configured_deviation_boundaries_and_legacy_fallback() {
        assert_eq!(DEFAULT_DEVIATION_BPS,10_000);
        assert!(within(1,10_000,DEFAULT_DEVIATION_BPS));
        for bps in [1, 500, 1000, 2000, 10000] {
            let band = bps as u128;
            assert!(within(10000 + band, 10000, bps));
            assert!(!within(10001 + band, 10000, bps));
            if band < 10000 { assert!(within(10000 - band, 10000, bps)); }
            assert!(!within(9999 - band.min(9999), 10000, bps));
        }
        for invalid in [0, 10001, u16::MAX] {
            assert_eq!(effective_deviation_bps(invalid), 1000);
            assert!(within(110, 100, invalid));
            assert!(!within(111, 100, invalid));
        }
        assert!(!within(0, 100, 10000));
        assert!(!within(100, 0, 10000));
        assert!(within(u128::MAX, u128::MAX, 10000));
        assert!(within(u128::MAX - 1, u128::MAX, 1));
    }

    #[test]
    fn configured_threshold_controls_samples_and_average_independently() {
        for bps in [500, 2000] {
            let mut o = obs();
            for i in 0..5 { o.record(bps, 1000+i*60, i as u64+1, 1000, 1000, 1000, i as u128*60*1000).unwrap(); }
            let r = o.record(bps, 1300, 6, 1100, 1100, 1100, 300000).unwrap();
            assert_eq!(r.is_some(), bps == 2000);
            assert_eq!(o.breaker, bps == 500);
            if bps == 500 {
                // Changing the threshold does not clear a persisted breaker or skip warmup.
                for i in 1..5 { assert!(o.record(2000,1300+i*60,6+i as u64,1100,1100,1100,300000+i as u128*60*1100).unwrap().is_none()); assert!(o.breaker); }
                assert!(o.record(2000,1600,11,1100,1100,1100,630000).unwrap().is_some());
                assert!(!o.breaker);
            }
            let mut gradual = obs(); let mut integral=0;
            for i in 0..6 {
                if i>0 { integral += (1000+(i-1)*40) as u128*60; }
                let price=(1000+i*40) as u128;
                let r=gradual.record(bps,1000+i*60,i as u64+1,price,price,price,integral).unwrap();
                if i==5 { assert_eq!(r.is_some(),bps==2000); assert_eq!(gradual.breaker,bps==500); }
            }
        }
    }

    #[test]
    fn pool_and_payout_guards_share_threshold_and_reject_unset_prices() {
        let mut cfg: DonateConfig = unsafe { std::mem::zeroed() };
        cfg.price_ready=true; cfg.last_sync_ts=1000; cfg.pool_ratio_anchor=PRICE_SCALE;
        for bps in [500,2000] {
            cfg.max_sync_deviation_bps=bps;
            let base=10000+bps as u64;
            assert!(pool_guard(&cfg,base,10000,1000).is_ok());
            assert!(check_pool_deviation(&cfg,base as u128,10000).is_ok());
            assert!(pool_guard(&cfg,base+1,10000,1000).is_err());
            assert!(check_pool_deviation(&cfg,(base+1) as u128,10000).is_err());
            assert!(pool_guard(&cfg,base,10000,1121).is_err());
        }
        cfg.max_sync_deviation_bps=0;
        assert!(pool_guard(&cfg,11000,10000,1000).is_ok());
        assert!(pool_guard(&cfg,11001,10000,1000).is_err());
        cfg.pool_ratio_anchor=0;
        assert!(pool_guard(&cfg,10000,10000,1000).is_err());
        assert!(check_pool_deviation(&cfg,10000,10000).is_err());
    }

    fn obs() -> PriceObservation {
        PriceObservation::try_deserialize_unchecked(
            &mut vec![0u8; 8 + PriceObservation::INIT_SPACE].as_slice(),
        )
        .unwrap()
    }
    #[test]
    fn full_window_and_old_value_weight() {
        let mut o = obs();
        for i in 0..5 {
            assert!(o
                .record(DEFAULT_DEVIATION_BPS,
                    1000 + i * 60,
                    i as u64 + 1,
                    100,
                    100,
                    100,
                    (i * 60 * 100) as u128
                )
                .unwrap()
                .is_none());
        }
        let a = o.record(DEFAULT_DEVIATION_BPS, 1300, 6, 105, 105, 105, 30000).unwrap().unwrap();
        assert_eq!(a, [100, 100, 100]);
    }
    #[test]
    fn gap_and_breaker_require_rewarm() {
        let mut o = obs();
        for i in 0..31 {
            o.record(DEFAULT_DEVIATION_BPS,
                1000 + i * 60,
                i as u64 + 1,
                100,
                100,
                100,
                (i * 6000) as u128,
            )
            .unwrap();
        }
        assert!(o.record(DEFAULT_DEVIATION_BPS, 3000, 40, 100, 100, 100, 200000).unwrap().is_none());
        assert_eq!(o.count, 1);
        assert!(o.record(DEFAULT_DEVIATION_BPS, 3060, 41, 201, 100, 100, 206000).unwrap().is_none());
        assert!(o.breaker);
        assert!(o.record(DEFAULT_DEVIATION_BPS, 3120, 41, 200, 100, 100, 212000).is_err());
    }
    #[test]
    fn sustained_manipulation_is_a_documented_boundary() {
        let mut o = obs();
        for i in 0..31 {
            o.record(DEFAULT_DEVIATION_BPS,
                1000 + i * 60,
                i as u64 + 1,
                100,
                100,
                100,
                (i * 6000) as u128,
            )
            .unwrap();
        }
        assert!(o.record(DEFAULT_DEVIATION_BPS, 2860, 32, 201, 100, 100, 186000).unwrap().is_none());
        assert!(o.breaker);
        let mut result = None;
        for i in 1..=5 {
            result = o
                .record(DEFAULT_DEVIATION_BPS,
                    2860 + i * 60,
                    32 + i as u64,
                    201,
                    100,
                    100,
                    (186000 + i * 6000) as u128,
                )
                .unwrap();
            if i < 5 { assert!(result.is_none()); }
        }
        assert_eq!(result.unwrap()[0], 201); // A sustained false market price becomes the sampled price.
        assert!(!o.breaker);
    }
    #[test]
    fn five_minute_boundary_and_minimum_sample_count() {
        let mut o = obs();
        assert_eq!(WINDOW, 300);
        assert_eq!(MIN_SAMPLES, 6);
        for i in 0..5 {
            assert!(o.record(DEFAULT_DEVIATION_BPS, 1000 + i * 60, i as u64 + 1, 100, 100, 100, (i * 6000) as u128).unwrap().is_none());
        }
        assert!(o.record(DEFAULT_DEVIATION_BPS, 1299, 6, 100, 100, 100, 29900).is_err());
        assert_eq!(o.record(DEFAULT_DEVIATION_BPS, 1300, 6, 100, 100, 100, 30000).unwrap(), Some([100; 3]));
        let mut sparse = obs();
        for i in 0..5 {
            assert!(sparse.record(DEFAULT_DEVIATION_BPS, 1000 + i * 120, i as u64 + 1, 100, 100, 100, (i * 12000) as u128).unwrap().is_none());
        }
        assert_eq!(sparse.record(DEFAULT_DEVIATION_BPS, 1600, 6, 100, 100, 100, 60000).unwrap(), Some([100; 3]));
    }
    #[test]
    fn existing_full_history_uses_the_new_window_without_layout_change() {
        let mut o = obs();
        for i in 0..32 { o.record(DEFAULT_DEVIATION_BPS, 1000 + i * 60, i as u64 + 1, 100, 100, 100, (i * 6000) as u128).unwrap(); }
        let mut bytes = Vec::new();
        o.try_serialize(&mut bytes).unwrap();
        assert_eq!(bytes.len(), 8 + PriceObservation::INIT_SPACE);
        let mut restored = PriceObservation::try_deserialize(&mut bytes.as_slice()).unwrap();
        assert_eq!(restored.record(DEFAULT_DEVIATION_BPS, 2920, 33, 105, 105, 105, 192000).unwrap(), Some([100; 3]));
    }

    #[test]
    fn same_slot_cannot_create_weight_or_overwrite_a_sample() {
        let mut o = obs();
        o.record(DEFAULT_DEVIATION_BPS, 1000, 10, 100, 100, 100, 0).unwrap();
        assert!(o.record(DEFAULT_DEVIATION_BPS, 1060, 10, 200, 200, 200, 6000).is_err());
        assert_eq!(o.last_sol, 100);
        assert_eq!(o.cumulative_sol, 0);
    }

    #[test]
    fn dex_twap_remains_exact_across_counter_rollover() {
        let mut o = obs();
        let price = u64::MAX as u128 * PRICE_SCALE;
        let start = u128::MAX - price * 150;
        for i in 0..5 {
            assert!(o
                .record(DEFAULT_DEVIATION_BPS,
                    1000 + i * 60,
                    i as u64 + 1,
                    100,
                    100,
                    price,
                    start.wrapping_add(price * (i * 60) as u128)
                )
                .unwrap()
                .is_none());
        }
        assert_eq!(
            o.record(DEFAULT_DEVIATION_BPS, 1300, 6, 100, 100, price, start.wrapping_add(price * 300))
                .unwrap()
                .unwrap(),
            [100, 100, price]
        );
    }
}
