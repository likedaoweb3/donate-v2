//! Postconditions for the external fixed-input swap. The callee receives a PDA signer,
//! so successful CPI return alone is not evidence that it respected the input budget.
use crate::DonateError;
use anchor_lang::prelude::*;
use anchor_lang::solana_program::{program_option::COption, program_pack::Pack};
use anchor_spl::token::spl_token::state::{Account as SplAccount, AccountState};

pub struct Snapshot {
    data: Box<[u8; SplAccount::LEN]>,
    amount: u64,
    lamports: u64,
}

impl Snapshot {
    pub fn capture(
        account: &AccountInfo,
        mint: &Pubkey,
        authority: &Pubkey,
        native: bool,
    ) -> Result<Self> {
        require_keys_eq!(
            *account.owner,
            anchor_spl::token::ID,
            DonateError::EscrowPermissionsChanged
        );
        let data = account.try_borrow_data()?;
        require!(
            data.len() == SplAccount::LEN && !account.executable,
            DonateError::EscrowPermissionsChanged
        );
        let token = SplAccount::unpack(&data)?;
        require!(
            token.mint == *mint
                && token.owner == *authority
                && token.state == AccountState::Initialized
                && token.delegate == COption::None
                && token.delegated_amount == 0
                && token.close_authority == COption::None
                && token.is_native.is_some() == native,
            DonateError::EscrowPermissionsChanged
        );
        let mut saved = Box::new([0u8; SplAccount::LEN]);
        saved.copy_from_slice(&data);
        Ok(Self {
            data: saved,
            amount: token.amount,
            lamports: account.lamports(),
        })
    }

    fn amount_after(&self, account: &AccountInfo) -> Result<u64> {
        require_keys_eq!(
            *account.owner,
            anchor_spl::token::ID,
            DonateError::EscrowPermissionsChanged
        );
        let data = account.try_borrow_data()?;
        // Classic SPL Account only permits the amount at 64..72 to change in a swap.
        // Comparing all other bytes covers mint, authority, delegate, state, native rent
        // reserve, delegated amount and close authority (including option padding).
        require!(
            !account.executable
                && data.len() == SplAccount::LEN
                && data[..64] == self.data[..64]
                && data[72..] == self.data[72..],
            DonateError::EscrowPermissionsChanged
        );
        Ok(u64::from_le_bytes(data[64..72].try_into().unwrap()))
    }
}

pub fn verify_swap(
    source_before: &Snapshot,
    destination_before: &Snapshot,
    source: &AccountInfo,
    destination: &AccountInfo,
    input: u64,
    min_output: u64,
) -> Result<u64> {
    let source_amount = source_before.amount_after(source)?;
    let destination_amount = destination_before.amount_after(destination)?;
    require!(
        source_before.amount.checked_sub(source_amount) == Some(input)
            && source_before.lamports.checked_sub(source.lamports()) == Some(input),
        DonateError::ExternalSwapDebit
    );
    require!(
        destination.lamports() == destination_before.lamports,
        DonateError::EscrowPermissionsChanged
    );
    let received = destination_amount
        .checked_sub(destination_before.amount)
        .ok_or(DonateError::ExternalSwapOutput)?;
    require!(received >= min_output, DonateError::ExternalSwapOutput);
    Ok(received)
}
