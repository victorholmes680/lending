use core::{borrow, time};
use std::f32::consts::E;

use anchor_lang::{prelude::*, solana_program::clock};
use anchor_spl::token_interface::Mint;
use pyth_solana_receiver_sdk::{
    cpi,
    price_update::{get_feed_id_from_hex, PriceUpdateV2},
};

use crate::{
    constant::{MAX_AGE, SOL_USD_FEED_ID, USDC_USD_FEED_ID},
    state::{Bank, User},
};
use crate::{error::ErrorCode, instructions::deposit};

#[derive(Accounts)]
pub struct Borrow<'info> {
    #[account(mut)]
    pub signer: Signer<'info>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        seeds = [mint.key().as_ref()],
        bump,
    )]
    pub bank: Account<'info, Bank>,

    #[account(
        mut,
        seeds = [b"treasury", mint.key().as_ref()],
        bump,
    )]
    pub bank_token_account: InterfaceAccount<'info, anchor_spl::token_interface::TokenAccount>,
    #[account(
        mut,
        seeds = [signer.key().as_ref()],
        bump,
    )]
    pub user_account: Account<'info, crate::state::User>,
    #[account(
        init_if_needed,
        payer = signer,
        associated_token::mint = mint,
        associated_token::authority = signer,
        associated_token::token_program = token_program,
    )]
    pub user_token_account: InterfaceAccount<'info, anchor_spl::token_interface::TokenAccount>,

    pub price_update: Account<'info, PriceUpdateV2>,

    pub token_program: Interface<'info, anchor_spl::token_interface::TokenInterface>,

    pub system_program: Program<'info, System>,
    pub associated_token_program: Program<'info, anchor_spl::associated_token::AssociatedToken>,
}

pub fn process_borrow(ctx: Context<Borrow>, amount: u64) -> Result<()> {
    let bank = &mut ctx.accounts.bank;
    let user = &mut ctx.accounts.user_account;

    let price_update = &ctx.accounts.price_update;

    let total_collateral: u64;

    match ctx.accounts.mint.to_account_info().key() {
        key if key == user.usdc_address => {
            let sol_feed_id = get_feed_id_from_hex(SOL_USD_FEED_ID)?;
            let sol_price =
                price_update.get_price_no_older_than(&Clock::get()?, MAX_AGE, &sol_feed_id)?;
            let new_value = calculate_accrued_interest(
                user.deposited_sol,
                bank.interest_rate,
                user.last_update,
            )?;
            total_collateral = sol_price.price as u64 * new_value;
        }
        _ => {
            let usdc_feed_id = get_feed_id_from_hex(USDC_USD_FEED_ID)?;
            let usdc_price =
                price_update.get_price_no_older_than(&Clock::get()?, MAX_AGE, &usdc_feed_id)?;
            let new_value = calculate_accrued_interest(
                user.deposited_usdc,
                bank.interest_rate,
                user.last_update,
            )?;
            total_collateral = usdc_price.price as u64 * new_value;
        }
    }

    let borrowable_amount = total_collateral
        .checked_mul(bank.liquidation_threshold)
        .unwrap();

    if borrowable_amount < amount {
        return Err(ErrorCode::OverBorrowableAmount.into());
    }

    let transfer_cpi_accounts = anchor_spl::token_interface::TransferChecked {
        from: ctx.accounts.bank_token_account.to_account_info(),
        to: ctx.accounts.user_token_account.to_account_info(),
        authority: ctx.accounts.bank_token_account.to_account_info(),
        mint: ctx.accounts.mint.to_account_info(),
    };

    let cpi_program = ctx.accounts.token_program.to_account_info();
    let mint_key = ctx.accounts.mint.key();
    let signer_seeds: &[&[&[u8]]] = &[&[
        b"treasury",
        mint_key.as_ref(),
        &[ctx.bumps.bank_token_account],
    ]];
    let cpi_ctx = CpiContext::new(cpi_program, transfer_cpi_accounts).with_signer(signer_seeds);
    let decimals = ctx.accounts.mint.decimals;
    anchor_spl::token_interface::transfer_checked(cpi_ctx, amount, decimals)?;

    if bank.total_borrows == 0 {
        bank.total_borrows = amount;
        bank.total_borrow_shares = amount;
    }

    let borrow_ratio = amount.checked_div(bank.total_borrows).unwrap();
    let user_shares = bank.total_borrow_shares.checked_mul(borrow_ratio).unwrap();

    match ctx.accounts.mint.to_account_info().key() {
        key if key == user.usdc_address => {
            user.borrowed_usdc += amount;
            user.borrowed_usdc_shares += user_shares;
        }
        _ => {
            user.borrowed_sol += amount;
            user.borrowed_sol_shares += user_shares;
        }
    }

    Ok(())
}

fn calculate_accrued_interest(
    deposited: u64,
    interest_rate: u64,
    last_updated: i64,
) -> Result<u64> {
    let current_time = Clock::get()?.unix_timestamp;
    let time_diff = current_time - last_updated;
    let new_value =
        (deposited as f64 * E.powf(interest_rate as f32 * time_diff as f32) as f64) as u64;
    Ok(new_value)
}
