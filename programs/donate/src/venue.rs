//! Read-only view over the legacy AMM and Raydium pool ABIs.
use crate::*;
use anchor_lang::Owners;
use std::ops::Deref;
pub const CPMM: Pubkey = pubkey!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");
pub const CLMM: Pubkey = pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");
pub const EXISTING_CLMM_POOL: Pubkey = pubkey!("FngGnPrkFfxBjbZSCTtJu6H2KCh4iwABf83dRJ9yFfQa");
pub const LEGACY_LIKER: Pubkey = pubkey!("8UqYNLpm5fzG8LZXVaje6nf6gEdvB6WeLY1rLzoAnoc6");
pub const LEGACY_LP: Pubkey = pubkey!("7rmNkUJg8uZwmPeyN29QdnGjPvc49FmsT5LwKTJSrF5N");
pub const LIKE: Pubkey = pubkey!("CJMihkPYswa3k6az9SUbepjKnkJQ6KWpGG5p9qW9n7NV");
// Keep the legacy account ABI; after CPMM binding its LP destination is the official
// incinerator's classic SPL ATA. Shares are priced by pool.lp_supply, never mint supply.
pub fn lp_sink_owner(pool: &VenuePool) -> Pubkey {
    if pool.cpmm.is_some() || pool.clmm.is_some() { INCINERATOR }
    else { Pubkey::find_program_address(&[b"vault_authority"], &LP_LOCKER_ID).0 }
}
pub fn lp_sink(pool: &VenuePool, mint: Pubkey) -> Pubkey {
    if pool.cpmm.is_some() || pool.clmm.is_some() {
        anchor_spl::associated_token::get_associated_token_address(&INCINERATOR, &mint)
    } else { Pubkey::find_program_address(&[b"vault", mint.as_ref()], &LP_LOCKER_ID).0 }
}
fn disc(name: &str) -> [u8; 8] {
    anchor_lang::solana_program::hash::hash(format!("account:{name}").as_bytes()).to_bytes()[..8]
        .try_into()
        .unwrap()
}
fn key(b: &[u8], o: usize) -> Pubkey {
    Pubkey::new_from_array(b[o..o + 32].try_into().unwrap())
}
fn u64at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn i32at(b: &[u8], o: usize) -> i32 {
    i32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
// Position boundaries and swap traversal are different account sets. Optional
// swap arrays follow the fixed venue/position prefix, before referral accounts.
fn tick_start(a: &AccountInfo) -> Result<Option<i32>> {
    if *a.owner != CLMM { return Ok(None); }
    let data = a.try_borrow_data()?;
    if data.len() < 8 || data[..8] != disc("TickArrayState") { return Ok(None); }
    require!(data.len() == 10240 && key(&data, 8) == EXISTING_CLMM_POOL, DonateError::BadVaultAccount);
    let start = i32at(&data, 40);
    require_keys_eq!(a.key(), Pubkey::find_program_address(
        &[b"tick_array", EXISTING_CLMM_POOL.as_ref(), &start.to_be_bytes()], &CLMM).0,
        DonateError::BadVaultAccount);
    require!(a.is_writable, DonateError::BadVaultAccount);
    Ok(Some(start))
}
fn after_swap_arrays(remaining: &[AccountInfo], mut n: usize) -> Result<usize> {
    let first = n;
    while n < remaining.len() && tick_start(&remaining[n])?.is_some() {
        n += 1;
        require!(n - first <= 4, DonateError::BadVaultAccount);
    }
    Ok(n)
}
#[derive(Clone)]
pub struct CpmmState {
    pub config: Pubkey,
    pub creator: Pubkey,
    pub observation: Pubkey,
    pub base_is_zero: bool,
    pub base_fees: u64,
    pub quote_fees: u64,
    pub creator_enabled: bool,
    pub creator_fee_on: u8,
    pub open_time: u64,
}
#[derive(Clone)]
pub struct ClmmState {
    pub config: Pubkey,
    pub creator: Pubkey,
    pub observation: Pubkey,
    pub sqrt_price_x64: u128,
    pub liquidity: u128,
    pub tick_current: i32,
    pub tick_spacing: u16,
}
#[derive(Clone)]
pub struct VenuePool {
    pub legacy: dex_pool::Pool,
    pub cpmm: Option<CpmmState>,
    pub clmm: Option<ClmmState>,
}
impl Deref for VenuePool {
    type Target = dex_pool::Pool;
    fn deref(&self) -> &Self::Target {
        &self.legacy
    }
}
static VENUE_OWNERS: [Pubkey; 3] = [dex_pool::ID, CPMM, CLMM];
impl Owners for VenuePool {
    fn owners() -> &'static [Pubkey] {
        &VENUE_OWNERS
    }
}
impl Discriminator for VenuePool {
    const DISCRIMINATOR: &'static [u8] = dex_pool::Pool::DISCRIMINATOR;
}
impl AccountSerialize for VenuePool {
    fn try_serialize<W: std::io::Write>(&self, w: &mut W) -> Result<()> {
        self.legacy.try_serialize(w)
    }
}
impl AccountDeserialize for VenuePool {
    fn try_deserialize(buf: &mut &[u8]) -> Result<Self> {
        if buf.starts_with(dex_pool::Pool::DISCRIMINATOR) {
            return Ok(Self {
                legacy: dex_pool::Pool::try_deserialize(buf)?,
                cpmm: None,
                clmm: None,
            });
        }
        let b = *buf;
        if b.len() == 1544 && b[..8] == disc("PoolState") {
            require!(
                key(b, 73) == LEGACY_LIKER && key(b, 105) == LIKE
                    && b[233] == 9 && b[234] == 9 && b[389] & 4 == 0,
                DonateError::WrongMint
            );
            let zero = [0u8; 8 + dex_pool::Pool::INIT_SPACE];
            let mut p = dex_pool::Pool::try_deserialize_unchecked(&mut &zero[..])?;
            p.authority = key(b, 41);
            p.base_mint = LIKE;
            p.quote_mint = LEGACY_LIKER;
            p.base_vault = key(b, 169);
            p.quote_vault = key(b, 137);
            // Legacy account ABI retains these fields. A CLMM position is an NFT,
            // never a fungible LP share, and cannot be priced using total_lp.
            p.lp_mint = LEGACY_LP;
            p.day_window_secs = 86400;
            p.burn_bps = 0;
            p.buy_enabled = true;
            p.sell_enabled = true;
            return Ok(Self {
                legacy: p,
                cpmm: None,
                clmm: Some(ClmmState {
                    config: key(b, 9),
                    creator: key(b, 41),
                    observation: key(b, 201),
                    sqrt_price_x64: u128::from_le_bytes(b[253..269].try_into().unwrap()),
                    liquidity: u128::from_le_bytes(b[237..253].try_into().unwrap()),
                    tick_current: i32::from_le_bytes(b[269..273].try_into().unwrap()),
                    tick_spacing: u16::from_le_bytes(b[235..237].try_into().unwrap()),
                }),
            });
        }
        require!(
            b.len() == 637 && b[..8] == disc("PoolState"),
            DonateError::BadVaultAccount
        );
        let m0 = key(b, 168);
        let m1 = key(b, 200);
        require!(m0 == LIKE || m1 == LIKE, DonateError::WrongMint);
        require!(
            key(b, 232) == anchor_spl::token::ID
                && key(b, 264) == anchor_spl::token::ID
                && b[331] == 9
                && b[332] == 9,
            DonateError::BadDecimals
        );
        let zero = m0 == LIKE;
        let f0 = u64at(b, 341)
            .checked_add(u64at(b, 357))
            .and_then(|v| v.checked_add(u64at(b, 397)))
            .ok_or(DonateError::MathOverflow)?;
        let f1 = u64at(b, 349)
            .checked_add(u64at(b, 365))
            .and_then(|v| v.checked_add(u64at(b, 405)))
            .ok_or(DonateError::MathOverflow)?;
        let empty_pool = [0u8; 8 + dex_pool::Pool::INIT_SPACE];
        let mut p = dex_pool::Pool::try_deserialize_unchecked(&mut &empty_pool[..])?;
        p.authority = key(b, 40);
        p.base_mint = LIKE;
        p.quote_mint = if zero { m1 } else { m0 };
        p.base_vault = key(b, if zero { 72 } else { 104 });
        p.quote_vault = key(b, if zero { 104 } else { 72 });
        p.lp_mint = key(b, 136);
        p.total_lp = u64at(b, 333);
        p.buy_enabled = b[329] & 4 == 0;
        p.sell_enabled = p.buy_enabled;
        p.day_window_secs = 86400;
        p.burn_bps = 0;
        Ok(Self {
            legacy: p,
            cpmm: Some(CpmmState {
                config: key(b, 8),
                creator: key(b, 40),
                observation: key(b, 296),
                base_is_zero: zero,
                base_fees: if zero { f0 } else { f1 },
                quote_fees: if zero { f1 } else { f0 },
                creator_enabled: b[390] == 1,
                creator_fee_on: b[389],
                open_time: u64at(b, 373),
            }),
            clmm: None,
        })
    }
    fn try_deserialize_unchecked(buf: &mut &[u8]) -> Result<Self> {
        Self::try_deserialize(buf)
    }
}
#[cfg(feature = "idl-build")]
impl anchor_lang::IdlBuild for VenuePool {
    fn create_type() -> Option<anchor_lang::idl::types::IdlTypeDef> {
        <dex_pool::Pool as anchor_lang::IdlBuild>::create_type()
    }
    fn get_full_path() -> String {
        <dex_pool::Pool as anchor_lang::IdlBuild>::get_full_path()
    }
    fn insert_types(
        t: &mut std::collections::BTreeMap<String, anchor_lang::idl::types::IdlTypeDef>,
    ) {
        <dex_pool::Pool as anchor_lang::IdlBuild>::insert_types(t)
    }
}
impl VenuePool {
    pub fn canonical_pool(&self) -> Pubkey {
        if let Some(c) = &self.clmm {
            return Pubkey::find_program_address(
                &[b"pool", c.config.as_ref(), LEGACY_LIKER.as_ref(), LIKE.as_ref()], &CLMM,
            ).0;
        }
        let c = self.cpmm.as_ref().unwrap();
        let (m0, m1) = if c.base_is_zero {
            (self.base_mint, self.quote_mint)
        } else {
            (self.quote_mint, self.base_mint)
        };
        Pubkey::find_program_address(
            &[b"pool", c.config.as_ref(), m0.as_ref(), m1.as_ref()],
            &CPMM,
        )
        .0
    }
    pub fn program(&self) -> Pubkey {
        if self.clmm.is_some() { CLMM }
        else if self.cpmm.is_some() {
            CPMM
        } else {
            dex_pool::ID
        }
    }
    pub fn reserves(&self, base: u64, quote: u64) -> Result<(u64, u64)> {
        if let Some(c) = &self.clmm {
            // A CLMM vault ratio is not a price: inactive/out-of-range deposits
            // and accrued fees skew it. Expose a fixed-depth virtual ratio from
            // sqrtPriceX64 to the legacy price guards and conservative spot quote.
            let s = c.sqrt_price_x64 >> 32;
            let ratio = s.checked_mul(s).and_then(|n| n.checked_mul(PRICE_SCALE))
                .ok_or(DonateError::MathOverflow)? / (1u128 << 64);
            return Ok((u64::try_from(ratio).map_err(|_| error!(DonateError::MathOverflow))?, PRICE_SCALE as u64));
        }
        if let Some(c) = &self.cpmm {
            Ok((
                base.checked_sub(c.base_fees)
                    .ok_or(DonateError::EmptyPool)?,
                quote
                    .checked_sub(c.quote_fees)
                    .ok_or(DonateError::EmptyPool)?,
            ))
        } else {
            Ok((base, quote))
        }
    }
    pub fn extras<'a, 'info>(
        &self,
        remaining: &'a [AccountInfo<'info>],
    ) -> Result<&'a [AccountInfo<'info>]> {
        if self.clmm.is_some() {
            require!(remaining.len() >= 8, DonateError::BadVaultAccount);
            Ok(&remaining[..8])
        } else if self.cpmm.is_some() {
            require!(remaining.len() >= 5, DonateError::BadVaultAccount);
            Ok(&remaining[..5])
        } else {
            Ok(&remaining[..0])
        }
    }
    pub fn ancestors<'a, 'info>(
        &self,
        remaining: &'a [AccountInfo<'info>],
    ) -> Result<&'a [AccountInfo<'info>]> {
        let n = self.extras(remaining)?.len();
        if self.clmm.is_some() {
            require!(remaining.len()>n, DonateError::BadTreasuryBurnAccount);
            let n = after_swap_arrays(remaining, n)?;
            require!(n < remaining.len(), DonateError::BadTreasuryBurnAccount);
            Ok(&remaining[n..remaining.len()-1])
        } else { Ok(&remaining[n..]) }
    }
    pub fn donation_ancestors<'a, 'info>(
        &self,
        remaining: &'a [AccountInfo<'info>],
    ) -> Result<&'a [AccountInfo<'info>]> {
        if self.clmm.is_some() {
            require!(remaining.len() >= 20, DonateError::BadVaultAccount);
            let n = after_swap_arrays(remaining, 19)?;
            require!(n < remaining.len(), DonateError::BadTreasuryBurnAccount);
            Ok(&remaining[n..remaining.len()-1])
        } else {
            self.ancestors(remaining)
        }
    }
    pub fn fee(&self, remaining: &[AccountInfo]) -> Result<u64> {
        if let Some(c) = &self.clmm {
            let e = self.extras(remaining)?;
            require_keys_eq!(e[0].key(), c.config, DonateError::BadVaultAccount);
            require_keys_eq!(*e[0].owner, CLMM, DonateError::BadVaultAccount);
            require_keys_eq!(e[1].key(), c.observation, DonateError::BadVaultAccount);
            require_keys_eq!(*e[1].owner, CLMM, DonateError::BadVaultAccount);
            let observation = e[1].try_borrow_data()?;
            require!(observation.len() == 4483 && observation[..8] == disc("ObservationState")
                && key(&observation, 19) == EXISTING_CLMM_POOL, DonateError::BadVaultAccount);
            require_keys_eq!(
                e[2].key(),
                Pubkey::find_program_address(&[b"pool_tick_array_bitmap_extension", EXISTING_CLMM_POOL.as_ref()], &CLMM).0,
                DonateError::BadVaultAccount
            );
            require_keys_eq!(*e[2].owner, CLMM, DonateError::BadVaultAccount);
            let bitmap = e[2].try_borrow_data()?;
            require!(bitmap.len() >= 40 && bitmap[..8] == disc("TickArrayBitmapExtension")
                && key(&bitmap, 8) == EXISTING_CLMM_POOL, DonateError::BadVaultAccount);
            require_keys_eq!(*e[3].owner, crate::ID, DonateError::BadVaultAccount);
            let migration = crate::raydium_migration::RaydiumMigration::try_deserialize(&mut &e[3].try_borrow_data()?[..])?;
            require!(migration.bound && migration.target_pool == EXISTING_CLMM_POOL
                && migration.new_mint == LEGACY_LIKER, DonateError::BadVaultAccount);
            require_keys_eq!(e[3].key(), Pubkey::find_program_address(
                &[b"raydium-migration", migration.config.as_ref()], &crate::ID).0,
                DonateError::BadVaultAccount);
            for tick_array in [&e[4], &e[5]] {
                require_keys_eq!(*tick_array.owner, CLMM, DonateError::BadVaultAccount);
                let data = tick_array.try_borrow_data()?;
                require!(data.len() == 10240 && data[..8] == disc("TickArrayState")
                    && key(&data, 8) == EXISTING_CLMM_POOL, DonateError::BadVaultAccount);
                let start = i32at(&data, 40);
                require_keys_eq!(tick_array.key(), Pubkey::find_program_address(
                    &[b"tick_array", EXISTING_CLMM_POOL.as_ref(), &start.to_be_bytes()], &CLMM).0,
                    DonateError::BadVaultAccount);
            }
            require!(e[6].key() == anchor_spl::token_2022::ID && e[6].executable
                && e[7].key() == pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr") && e[7].executable,
                DonateError::BadVaultAccount);
            let b = e[0].try_borrow_data()?;
            require!(b.len() == 117 && b[..8] == disc("AmmConfig"), DonateError::BadVaultAccount);
            require!(u16::from_le_bytes(b[51..53].try_into().unwrap()) == c.tick_spacing,
                DonateError::BadVaultAccount);
            let fee = u32::from_le_bytes(b[47..51].try_into().unwrap()) as u64;
            require!(fee < 1_000_000, DonateError::BadRatios);
            return Ok(fee);
        }
        let c = self.cpmm.as_ref().ok_or(DonateError::BadVaultAccount)?;
        let extras = self.extras(remaining)?;
        let auth = Pubkey::find_program_address(&[b"vault_and_lp_mint_auth_seed"], &CPMM).0;
        require_keys_eq!(extras[0].key(), auth, DonateError::BadVaultAccount);
        require_keys_eq!(extras[1].key(), c.config, DonateError::BadVaultAccount);
        require_keys_eq!(*extras[1].owner, CPMM, DonateError::BadVaultAccount);
        require_keys_eq!(extras[2].key(), c.observation, DonateError::BadVaultAccount);
        require_keys_eq!(*extras[2].owner, CPMM, DonateError::BadVaultAccount);
        require!(
            extras[4].key() == anchor_spl::token_2022::ID && extras[4].executable,
            DonateError::BadVaultAccount
        );
        let migration_ai = &extras[3];
        require_keys_eq!(*migration_ai.owner, crate::ID, DonateError::BadVaultAccount);
        let migration = crate::raydium_migration::RaydiumMigration::try_deserialize(
            &mut &migration_ai.try_borrow_data()?[..],
        )?;
        require_keys_eq!(
            migration_ai.key(),
            Pubkey::find_program_address(
                &[b"raydium-migration", migration.config.as_ref()],
                &crate::ID
            )
            .0,
            DonateError::BadVaultAccount
        );
        if migration.bound {
            require!(
                migration.target_pool == self.canonical_pool()
                    && migration.new_mint == self.quote_mint
                    && migration.fee_recipient == c.creator,
                DonateError::BadVaultAccount
            );
        }
        let observation = extras[2].try_borrow_data()?;
        require!(
            observation.len() == 4075
                && observation[..8] == disc("ObservationState")
                && key(&observation, 11) == self.canonical_pool(),
            DonateError::BadVaultAccount
        );
        let b = extras[1].try_borrow_data()?;
        require!(
            b.len() == 236 && b[..8] == disc("AmmConfig"),
            DonateError::BadVaultAccount
        );
        require!(
            c.creator_enabled && c.creator_fee_on == 0 && u64at(&b, 108) == 5000,
            DonateError::BadRatios
        );
        let f = u64at(&b, 12)
            .checked_add(5000)
            .ok_or(DonateError::MathOverflow)?;
        require!(f < 1_000_000, DonateError::BadRatios);
        Ok(f)
    }
    pub fn integral(
        &self,
        pool: Pubkey,
        base: u64,
        quote: u64,
        remaining: &[AccountInfo],
        now: i64,
    ) -> Result<u128> {
        if let Some(c) = &self.cpmm {
            self.fee(remaining)?;
            let b = remaining[2].try_borrow_data()?;
            require!(
                b.len() == 4075
                    && b[..8] == disc("ObservationState")
                    && b[8] == 1
                    && key(&b, 11) == pool,
                DonateError::PriceUnavailable
            );
            let index = u16::from_le_bytes(b[9..11].try_into().unwrap()) as usize;
            require!(index < 100, DonateError::BadVaultAccount);
            let o = 43 + index * 40;
            let last = u64at(&b, 4043);
            require!(
                last > 0 && now >= last as i64,
                DonateError::PriceUnavailable
            );
            let offset = o + if c.base_is_zero { 24 } else { 8 };
            let cumulative = u128::from_le_bytes(b[offset..offset + 16].try_into().unwrap());
            let (base, quote) = self.reserves(base, quote)?;
            require!(quote > 0, DonateError::EmptyPool);
            // Q32 counter remains in its native domain. Oracle converts its window delta, avoiding
            // multiplication of the lifetime counter and preserving wraparound subtraction.
            let delta = (((base as u128) << 32) / (quote as u128))
                .checked_mul((now as u64 - last) as u128)
                .ok_or(DonateError::MathOverflow)?;
            Ok(cumulative.wrapping_add(delta))
        } else {
            self.legacy.price_integral(now)
        }
    }
}
pub fn read(pool: &AccountInfo) -> Result<VenuePool> {
    require!(
        VenuePool::owners().contains(pool.owner),
        DonateError::BadVaultAccount
    );
    let view = VenuePool::try_deserialize(&mut &pool.try_borrow_data()?[..])?;
    if view.cpmm.is_some() || view.clmm.is_some() {
        require_keys_eq!(
            pool.key(),
            view.canonical_pool(),
            DonateError::BadVaultAccount
        );
    }
    Ok(view)
}
pub fn checked_program(program: &AccountInfo, pool: &VenuePool) -> Result<()> {
    require!(
        program.executable && program.key() == pool.program(),
        DonateError::BadVaultAccount
    );
    Ok(())
}
pub fn quote(input: u64, ri: u64, ro: u64, fee: u64) -> Result<u64> {
    require!(ri > 0 && ro > 0 && fee < 1_000_000, DonateError::EmptyPool);
    let f = (input as u128 * fee as u128 + 999_999) / 1_000_000;
    let n = (input as u128)
        .checked_sub(f)
        .ok_or(DonateError::ZeroAmount)?;
    u64::try_from(n * ro as u128 / (ri as u128 + n)).map_err(|_| error!(DonateError::MathOverflow))
}
#[allow(clippy::too_many_arguments)]
pub fn swap<'info>(
    program: AccountInfo<'info>,
    user: AccountInfo<'info>,
    pool: AccountInfo<'info>,
    base_mint: AccountInfo<'info>,
    quote_mint: AccountInfo<'info>,
    base_vault: AccountInfo<'info>,
    quote_vault: AccountInfo<'info>,
    user_base: AccountInfo<'info>,
    user_quote: AccountInfo<'info>,
    token: AccountInfo<'info>,
    input: u64,
    min_out: u64,
    side: u8,
    signer: &[&[&[u8]]],
    remaining: &[AccountInfo<'info>],
) -> Result<()> {
    let p = read(&pool)?;
    checked_program(&program, &p)?;
    if p.cpmm.is_none() && p.clmm.is_none() {
        return dex_pool::cpi::swap(
            CpiContext::new_with_signer(
                program,
                dex_pool::cpi::accounts::Swap {
                    user,
                    pool,
                    base_mint,
                    quote_mint,
                    base_vault,
                    quote_vault,
                    user_base,
                    user_quote,
                    token_program: token,
                },
                signer,
            ),
            input,
            min_out,
            side,
        );
    }
    p.fee(remaining)?;
    let e = p.extras(remaining)?;
    if p.clmm.is_some() {
        let (source, dest, iv, ov, im, om) = if side == 1 {
            (user_base, user_quote, base_vault, quote_vault, base_mint, quote_mint)
        } else {
            require!(side == 0, DonateError::BadRatios);
            (user_quote, user_base, quote_vault, base_vault, quote_mint, base_mint)
        };
        // Re-read the live tick: an earlier CPI in this transaction may have
        // moved the pool since Anchor deserialized VenuePool. Raydium validates
        // the bitmap/first array and every crossing; missing routes fail closed.
        let current = {
            let b = pool.try_borrow_data()?;
            let width = i32::from(p.clmm.as_ref().unwrap().tick_spacing) * 60;
            require!(width > 0, DonateError::BadVaultAccount);
            i32at(&b, 269).div_euclid(width) * width
        };
        let mut ticks = Vec::with_capacity(6);
        for a in remaining {
            if let Some(start) = tick_start(a)? {
                if (side == 1 && start >= current || side == 0 && start <= current)
                    && !ticks.iter().any(|(s, _)| *s == start) {
                    require!(ticks.len() < 6, DonateError::BadVaultAccount);
                    ticks.push((start, a.clone()));
                }
            }
        }
        ticks.sort_unstable_by_key(|(start, _)| *start);
        if side == 0 { ticks.reverse(); }
        require!(!ticks.is_empty(), DonateError::BadVaultAccount);
        let before_src = token_amount(&source)?;
        let before_dest = token_amount(&dest)?;
        let protected = [source.clone(), dest.clone(), iv.clone(), ov.clone()];
        let permissions = protected.iter().map(token_shape).collect::<Result<Vec<_>>>()?;
        let mut infos = Vec::with_capacity(14 + ticks.len());
        infos.extend([user, e[0].clone(), pool, source.clone(), dest.clone(), iv,
            ov, e[1].clone(), token.clone(), e[6].clone(), e[7].clone(), im, om,
            e[2].clone()]);
        infos.extend(ticks.into_iter().map(|(_, a)| a));
        let mut data = anchor_lang::solana_program::hash::hash(b"global:swap_v2").to_bytes()[..8].to_vec();
        data.extend(input.to_le_bytes());
        data.extend(min_out.to_le_bytes());
        data.extend(0u128.to_le_bytes());
        data.push(1); // exact input
        invoke_raw(&data, &infos, &program, signer, &[0])?;
        for (account, before) in protected.iter().zip(permissions) {
            require!(token_shape(account)? == before, DonateError::EscrowPermissionsChanged);
        }
        require!(before_src.checked_sub(token_amount(&source)?) == Some(input), DonateError::ExternalSwapDebit);
        require!(token_amount(&dest)?.checked_sub(before_dest).is_some_and(|n| n >= min_out), DonateError::ExternalSwapOutput);
        return Ok(());
    }
    let (source, dest, iv, ov, im, om) = if side == 1 {
        (
            user_base,
            user_quote,
            base_vault,
            quote_vault,
            base_mint,
            quote_mint,
        )
    } else {
        require!(side == 0, DonateError::BadRatios);
        (
            user_quote,
            user_base,
            quote_vault,
            base_vault,
            quote_mint,
            base_mint,
        )
    };
    let before_src = token_amount(&source)?;
    let before_dest = token_amount(&dest)?;
    let protected = [source.clone(), dest.clone(), iv.clone(), ov.clone()];
    let permissions = protected
        .iter()
        .map(token_shape)
        .collect::<Result<Vec<_>>>()?;
    let infos = vec![
        user,
        e[0].clone(),
        e[1].clone(),
        pool,
        source.clone(),
        dest.clone(),
        iv,
        ov,
        token.clone(),
        token,
        im,
        om,
        e[2].clone(),
    ];
    invoke(
        "swap_base_input",
        &infos,
        &program,
        &[input, min_out],
        signer,
        &[0],
    )?;
    for (account, before) in protected.iter().zip(permissions) {
        require!(
            token_shape(account)? == before,
            DonateError::EscrowPermissionsChanged
        );
    }
    require!(
        before_src.checked_sub(token_amount(&source)?) == Some(input),
        DonateError::ExternalSwapDebit
    );
    require!(
        token_amount(&dest)?
            .checked_sub(before_dest)
            .is_some_and(|n| n >= min_out),
        DonateError::ExternalSwapOutput
    );
    Ok(())
}
fn token_shape(a: &AccountInfo) -> Result<[u8; 32]> {
    let b = a.try_borrow_data()?;
    require!(
        *a.owner == anchor_spl::token::ID && b.len() == 165,
        DonateError::BadVaultAccount
    );
    // SPL Revoke clears delegateOption and delegatedAmount, but leaves the
    // inactive delegate pubkey bytes untouched. Those bytes grant no authority.
    if b[72..76] == [0; 4] {
        Ok(anchor_lang::solana_program::hash::hashv(&[
            &b[..64], &b[72..76], &[0; 32], &b[108..121], &[0; 8], &b[129..],
        ]).to_bytes())
    } else {
        Ok(anchor_lang::solana_program::hash::hashv(&[&b[..64], &b[72..]]).to_bytes())
    }
}
pub fn token_amount(a: &AccountInfo) -> Result<u64> {
    let b = a.try_borrow_data()?;
    require!(
        *a.owner == anchor_spl::token::ID && b.len() == 165,
        DonateError::BadVaultAccount
    );
    Ok(u64at(&b, 64))
}
fn invoke<'info>(
    name: &str,
    infos: &[AccountInfo<'info>],
    program: &AccountInfo<'info>,
    args: &[u64],
    signer: &[&[&[u8]]],
    signers: &[usize],
) -> Result<()> {
    let mut data = anchor_lang::solana_program::hash::hash(format!("global:{name}").as_bytes())
        .to_bytes()[..8]
        .to_vec();
    for n in args {
        data.extend(n.to_le_bytes());
    }
    invoke_raw(&data, infos, program, signer, signers)
}
fn invoke_raw<'info>(
    data: &[u8],
    infos: &[AccountInfo<'info>],
    program: &AccountInfo<'info>,
    signer: &[&[&[u8]]],
    signers: &[usize],
) -> Result<()> {
    let metas = infos
        .iter()
        .enumerate()
        .map(|(i, a)| AccountMeta {
            pubkey: a.key(),
            is_signer: signers.contains(&i),
            is_writable: a.is_writable,
        })
        .collect();
    let ix = Instruction {
        program_id: program.key(),
        accounts: metas,
        data: data.to_vec(),
    };
    // Solana uses a bump allocator: growing an exactly-sized clone retains the
    // old allocation for the entire instruction. Reserve the program slot once,
    // leaving room for full referral chains across repeated CLMM CPIs.
    let mut all = Vec::with_capacity(infos.len() + 1);
    all.extend_from_slice(infos);
    all.push(program.clone());
    invoke_signed(&ix, &all, signer).map_err(Into::into)
}
pub fn liquidity_target(base: u64, quote: u64, rb: u64, rq: u64, total: u64) -> Result<u64> {
    require!(rb > 0 && rq > 0 && total > 0, DonateError::EmptyPool);
    u64::try_from(
        (base as u128 * total as u128 / rb as u128).min(quote as u128 * total as u128 / rq as u128),
    )
    .map_err(|_| error!(DonateError::MathOverflow))
}
#[allow(clippy::too_many_arguments)]
pub fn add<'info>(
    program: AccountInfo<'info>,
    accounts: dex_pool::cpi::accounts::AddLiquidity<'info>,
    base: u64,
    quote_max: u64,
    min_lp: u64,
    signer: &[&[&[u8]]],
    remaining: &[AccountInfo<'info>],
) -> Result<u64> {
    let p = read(&accounts.pool)?;
    checked_program(&program, &p)?;
    let source = accounts.user_base.clone();
    let before = token_amount(&source)?;
    if p.clmm.is_some() {
        p.fee(remaining)?;
        require!(min_lp == 0, DonateError::BadRatios);
        require!(remaining.len() >= 19, DonateError::BadVaultAccount);
        let e = &remaining[..19];
        let payer = &e[8];
        let nft_mint = &e[9];
        let nft_ata = &e[10];
        let metadata = &e[11];
        let personal = &e[12];
        let protocol = &e[13];
        require!(payer.is_signer && payer.is_writable && nft_mint.is_signer && nft_mint.is_writable,
            DonateError::BadVaultAccount);
        require_keys_eq!(nft_ata.key(), anchor_spl::associated_token::get_associated_token_address(&INCINERATOR, &nft_mint.key()), DonateError::BadVaultAccount);
        require_keys_eq!(personal.key(), Pubkey::find_program_address(&[b"position", nft_mint.key().as_ref()], &CLMM).0, DonateError::BadVaultAccount);
        require_keys_eq!(protocol.key(), Pubkey::find_program_address(&[b"position", accounts.pool.key().as_ref(), &(-443520i32).to_le_bytes(), &(443520i32).to_le_bytes()], &CLMM).0, DonateError::BadVaultAccount);
        require_keys_eq!(e[14].key(), anchor_lang::solana_program::sysvar::rent::ID, DonateError::BadVaultAccount);
        require_keys_eq!(e[15].key(), anchor_lang::system_program::ID, DonateError::BadVaultAccount);
        require_keys_eq!(e[16].key(), anchor_spl::associated_token::ID, DonateError::BadVaultAccount);
        require_keys_eq!(e[17].key(), INCINERATOR, DonateError::BadVaultAccount);
        require_keys_eq!(e[18].key(), pubkey!("metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s"), DonateError::BadVaultAccount);
        let before_quote = token_amount(&accounts.user_quote)?;
        let base_shape = token_shape(&accounts.user_base)?;
        let quote_shape = token_shape(&accounts.user_quote)?;
        // Raydium's payer signs its token transfers. Grant only this instruction's
        // two exact maxima, then revoke before returning to Donate.
        for (source, amount) in [(&accounts.user_base, base), (&accounts.user_quote, quote_max)] {
            anchor_spl::token::approve(CpiContext::new_with_signer(
                accounts.token_program.clone(),
                anchor_spl::token::Approve { to: source.clone(), delegate: payer.clone(), authority: accounts.user.clone() },
                signer,
            ), amount)?;
        }
        let infos = vec![payer.clone(), e[17].clone(), nft_mint.clone(), nft_ata.clone(),
            metadata.clone(), accounts.pool.clone(), protocol.clone(), e[4].clone(), e[5].clone(),
            personal.clone(), accounts.user_quote.clone(), accounts.user_base.clone(),
            accounts.quote_vault.clone(), accounts.base_vault.clone(), e[14].clone(), e[15].clone(),
            accounts.token_program.clone(), e[16].clone(), e[18].clone(), e[6].clone(),
            accounts.quote_mint.clone(), accounts.base_mint.clone(), e[2].clone()];
        let mut data = anchor_lang::solana_program::hash::hash(b"global:open_position_v2").to_bytes()[..8].to_vec();
        for n in [-443520i32, 443520, -446400, 439200] { data.extend(n.to_le_bytes()); }
        data.extend(0u128.to_le_bytes()); // liquidity chosen from the LIKE leg
        data.extend(quote_max.to_le_bytes()); // mint A: LIKER
        data.extend(base.to_le_bytes()); // mint B: LIKE
        data.extend([0u8, 1u8, 0u8]); // no metadata, Some(baseFlag=false)
        invoke_raw(&data, &infos, &program, signer, &[0, 2])?;
        for source in [&accounts.user_base, &accounts.user_quote] {
            anchor_spl::token::revoke(CpiContext::new_with_signer(
                accounts.token_program.clone(),
                anchor_spl::token::Revoke { source: source.clone(), authority: accounts.user.clone() },
                signer,
            ))?;
        }
        require!(token_shape(&accounts.user_base)? == base_shape && token_shape(&accounts.user_quote)? == quote_shape,
            DonateError::EscrowPermissionsChanged);
        require!(before_quote.checked_sub(token_amount(&accounts.user_quote)?).is_some_and(|n| n > 0 && n <= quote_max),
            DonateError::ExternalSwapDebit);
        let nft = token_amount(nft_ata)?;
        require!(nft == 1, DonateError::ExternalSwapOutput);
    } else if p.cpmm.is_none() {
        dex_pool::cpi::add_liquidity(
            CpiContext::new_with_signer(program, accounts, signer),
            base,
            quote_max,
            min_lp,
        )?;
    } else {
        p.fee(remaining)?;
        let e = p.extras(remaining)?;
        let (rb, rq) = p.reserves(
            token_amount(&accounts.base_vault)?,
            token_amount(&accounts.quote_vault)?,
        )?;
        let lp = liquidity_target(base, quote_max, rb, rq, p.total_lp)?;
        require!(lp > 0 && lp >= min_lp, DonateError::ExternalSwapOutput);
        let c = p.cpmm.as_ref().unwrap();
        let (u0, u1, v0, v1, m0, m1, max0, max1) = if c.base_is_zero {
            (
                accounts.user_base.clone(),
                accounts.user_quote.clone(),
                accounts.base_vault.clone(),
                accounts.quote_vault.clone(),
                accounts.base_mint.clone(),
                accounts.quote_mint.clone(),
                base,
                quote_max,
            )
        } else {
            (
                accounts.user_quote.clone(),
                accounts.user_base.clone(),
                accounts.quote_vault.clone(),
                accounts.base_vault.clone(),
                accounts.quote_mint.clone(),
                accounts.base_mint.clone(),
                quote_max,
                base,
            )
        };
        let before_lp = token_amount(&accounts.user_lp)?;
        let before_q = token_amount(&accounts.user_quote)?;
        let protected = [
            accounts.user_base.clone(),
            accounts.user_quote.clone(),
            accounts.user_lp.clone(),
            accounts.base_vault.clone(),
            accounts.quote_vault.clone(),
        ];
        let permissions = protected
            .iter()
            .map(token_shape)
            .collect::<Result<Vec<_>>>()?;
        let mint_before = {
            let b = accounts.lp_mint.try_borrow_data()?;
            u64at(&b, 36)
        };
        let expected_base =
            ((lp as u128 * rb as u128 + p.total_lp as u128 - 1) / p.total_lp as u128) as u64;
        let expected_quote =
            ((lp as u128 * rq as u128 + p.total_lp as u128 - 1) / p.total_lp as u128) as u64;
        let infos = vec![
            accounts.user.clone(),
            e[0].clone(),
            accounts.pool.clone(),
            accounts.user_lp.clone(),
            u0,
            u1,
            v0,
            v1,
            accounts.token_program.clone(),
            e[4].clone(),
            m0,
            m1,
            accounts.lp_mint.clone(),
        ];
        invoke("deposit", &infos, &program, &[lp, max0, max1], signer, &[0])?;
        for (account, before) in protected.iter().zip(permissions) {
            require!(
                token_shape(account)? == before,
                DonateError::EscrowPermissionsChanged
            );
        }
        let mint_after = {
            let b = accounts.lp_mint.try_borrow_data()?;
            u64at(&b, 36)
        };
        require!(
            token_amount(&accounts.user_lp)?.checked_sub(before_lp) == Some(lp)
                && before_q.checked_sub(token_amount(&accounts.user_quote)?)
                    == Some(expected_quote)
                && before.checked_sub(token_amount(&source)?) == Some(expected_base)
                && mint_after.checked_sub(mint_before) == Some(lp)
                && read(&accounts.pool)?.total_lp.checked_sub(p.total_lp) == Some(lp),
            DonateError::ExternalSwapOutput
        );
    }
    let used = before
        .checked_sub(token_amount(&source)?)
        .ok_or(DonateError::ExternalSwapDebit)?;
    require!(used <= base, DonateError::ExternalSwapDebit);
    Ok(used)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fee_ceiling() {
        assert_eq!(quote(10000, 1000000, 1000000, 7500).unwrap(), 9827);
        assert_eq!(quote(1, 1000000, 1000000, 7500).unwrap(), 0);
    }
    #[test]
    fn liquidity_target_rejects_overflow_and_empty_ledger() {
        assert!(liquidity_target(u64::MAX, u64::MAX, 1, 1, u64::MAX).is_err());
        assert!(liquidity_target(1, 1, 1, 1, 0).is_err());
        assert_eq!(liquidity_target(50, 100, 100, 200, 1000).unwrap(), 500);
    }
    #[test]
    fn owners_and_malformed() {
        assert_eq!(VenuePool::owners(), &[dex_pool::ID, CPMM, CLMM]);
        assert!(VenuePool::try_deserialize(&mut &[0; 637][..]).is_err());
    }
}
