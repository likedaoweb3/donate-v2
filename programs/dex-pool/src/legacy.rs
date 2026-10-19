//! Fixed Locker exit authorized by the deployed program's upgrade authority.
use crate::*;
use anchor_lang::solana_program::pubkey;
use anchor_spl::associated_token::get_associated_token_address;
use anchor_spl::token_interface::{transfer_checked, TransferChecked};
pub const DEPLOYMENT: Pubkey = pubkey!("FJF6YMp72V8NHUniRZxweeZd5CyyTNYY96dUKuaG2kmw");
pub const LOCKER: Pubkey = pubkey!("9c18DLHnXrQC6MAVHpig9kpAE8YX7mZYhB6LtVT84RW1");
pub const POOL: Pubkey = pubkey!("2A9fNRik2FogYNcnCJw6seYL7Xpjx6zRYAzcM7AZsxZA");
pub const LIKE: Pubkey = pubkey!("CJMihkPYswa3k6az9SUbepjKnkJQ6KWpGG5p9qW9n7NV");
pub const LIKER: Pubkey = pubkey!("8UqYNLpm5fzG8LZXVaje6nf6gEdvB6WeLY1rLzoAnoc6");
pub const LP: Pubkey = pubkey!("7rmNkUJg8uZwmPeyN29QdnGjPvc49FmsT5LwKTJSrF5N");
// The existing reserved remove_enabled field becomes the migration freeze flag
// for this fixed pool only. No serialized account layout changes.
pub fn frozen(pool: &Account<Pool>) -> bool { pool.key() == POOL && !pool.remove_enabled }
#[derive(Accounts)]
pub struct FreezeLegacyPool<'info> {
    #[account(address=DEPLOYMENT)] pub deployment: Signer<'info>,
    #[account(address=crate::ID, constraint=program.programdata_address()? == Some(program_data.key()))]
    pub program: Program<'info, crate::program::DexPool>,
    #[account(constraint=program_data.upgrade_authority_address == Some(deployment.key()))]
    pub program_data: Box<Account<'info, ProgramData>>,
    #[account(mut,address=POOL)] pub pool: Box<Account<'info, Pool>>,
}
pub fn freeze(ctx: Context<FreezeLegacyPool>) -> Result<()> {
    let p=&mut ctx.accounts.pool;
    p.buy_enabled=false; p.sell_enabled=false; p.add_enabled=false; p.remove_enabled=false;
    p.operator=Pubkey::default(); p.burn_bps=0;
    p.last_burn_ts=Clock::get()?.unix_timestamp;
    Ok(())
}
#[derive(Accounts)]
pub struct ReleaseLockedLegacyLiquidity<'info> {
    #[account(address=DEPLOYMENT)] pub deployment: Signer<'info>,
    #[account(address=crate::ID, constraint=program.programdata_address()? == Some(program_data.key()))]
    pub program: Program<'info, crate::program::DexPool>,
    #[account(constraint=program_data.upgrade_authority_address == Some(deployment.key()))]
    pub program_data: Box<Account<'info, ProgramData>>,
    #[account(seeds=[b"vault_authority"],bump,seeds::program=LOCKER)]
    pub locker_authority: Signer<'info>,
    #[account(mut,address=POOL,has_one=base_mint,has_one=quote_mint,has_one=lp_mint,has_one=base_vault,has_one=quote_vault)]
    pub pool: Box<Account<'info, Pool>>,
    #[account(address=LIKE,owner=anchor_spl::token::ID)] pub base_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(address=LIKER,owner=anchor_spl::token::ID)] pub quote_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut,address=LP,owner=anchor_spl::token::ID)] pub lp_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut,token::mint=base_mint,token::authority=pool,token::token_program=token_program)]
    pub base_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut,token::mint=quote_mint,token::authority=pool,token::token_program=token_program)]
    pub quote_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut,seeds=[b"vault",LP.as_ref()],bump,seeds::program=LOCKER,
        token::mint=lp_mint,token::authority=locker_authority,token::token_program=token_program)]
    pub locker_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut,address=get_associated_token_address(&DEPLOYMENT,&LIKE),token::mint=base_mint,token::authority=deployment,token::token_program=token_program)]
    pub deployment_base: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut,address=get_associated_token_address(&DEPLOYMENT,&LIKER),token::mint=quote_mint,token::authority=deployment,token::token_program=token_program)]
    pub deployment_quote: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(address=anchor_spl::token::ID)] pub token_program: Interface<'info, TokenInterface>,
}
pub fn release(ctx: Context<ReleaseLockedLegacyLiquidity>, min_base:u64,min_quote:u64) -> Result<()> {
    let a=ctx.accounts;
    require!(frozen(&a.pool) && a.pool.burn_bps==0 && !a.pool.buy_enabled && !a.pool.sell_enabled
        && a.pool.operator==Pubkey::default(),DexError::Unauthorized);
    let lp=a.locker_vault.amount; let ledger=a.pool.total_lp;
    require!(lp>0 && ledger>0,DexError::ZeroLiquidity);
    require!(a.lp_mint.supply<=ledger,DexError::LpAccountingBroken);
    let base=u64::try_from(a.base_vault.amount as u128 * lp as u128 / ledger as u128).map_err(|_|DexError::MathOverflow)?;
    let quote=u64::try_from(a.quote_vault.amount as u128 * lp as u128 / ledger as u128).map_err(|_|DexError::MathOverflow)?;
    require!(base>=min_base && quote>=min_quote,DexError::SlippageExceeded);
    require!(base>0 || quote>0,DexError::ZeroAmount);
    let remaining=ledger.checked_sub(lp).ok_or(DexError::MathOverflow)?;
    require!(remaining>=MINIMUM_LIQUIDITY,DexError::ZeroLiquidity);
    burn(CpiContext::new(a.token_program.to_account_info(),Burn{mint:a.lp_mint.to_account_info(),
        from:a.locker_vault.to_account_info(),authority:a.locker_authority.to_account_info()}),lp)?;
    let bump=[a.pool.bump]; let base_key=a.base_mint.key(); let quote_key=a.quote_mint.key();
    let seeds:&[&[&[u8]]]=&[&[b"pool",base_key.as_ref(),quote_key.as_ref(),&bump]];
    for (from,to,mint,amount,decimals) in [(a.base_vault.to_account_info(),a.deployment_base.to_account_info(),a.base_mint.to_account_info(),base,a.base_mint.decimals),
        (a.quote_vault.to_account_info(),a.deployment_quote.to_account_info(),a.quote_mint.to_account_info(),quote,a.quote_mint.decimals)] {
        transfer_checked(CpiContext::new_with_signer(a.token_program.to_account_info(),TransferChecked{
            from,to,mint,authority:a.pool.to_account_info()},seeds),amount,decimals)?;
    }
    a.pool.total_lp=remaining; a.base_vault.reload()?; a.quote_vault.reload()?;
    a.pool.observe_reserves(a.base_vault.amount,a.quote_vault.amount,Clock::get()?.unix_timestamp)?;
    Ok(())
}
