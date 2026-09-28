use anchor_lang::prelude::*;
use anchor_lang::solana_program::{instruction::AccountMeta, program::invoke_signed};
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token_interface::{spl_token_2022, Mint, TokenAccount, TokenInterface};
use spl_token_2022::extension::{
    transfer_fee::TransferFeeConfig, BaseStateWithExtensions, StateWithExtensions,
};

declare_id!("48s9SoST2ZA5AoW6ytBggQiExFFB16f1TJiWVauLUCbB");

pub const LOCK_SEED: &[u8] = b"lock";
pub const MAX_LOCK_SECONDS: i64 = 20 * 365 * 86_400;

#[program]
pub mod divi_lock {
    use super::*;

    pub fn initialize<'info>(
        ctx: Context<'info, Initialize<'info>>,
        unlock_ts: i64,
        amount: u64,
        max_fee: u64,
    ) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(unlock_ts > now, LockError::UnlockInPast);
        require!(unlock_ts <= now + MAX_LOCK_SECONDS, LockError::UnlockTooFar);
        require!(amount > 0, LockError::ZeroAmount);

        let lock = &mut ctx.accounts.lock;
        lock.beneficiary = ctx.accounts.beneficiary.key();
        lock.lock_mint = ctx.accounts.lock_mint.key();
        lock.vault = ctx.accounts.vault.key();
        lock.reward_mint = ctx.accounts.reward_mint.key();
        lock.reward_vault = ctx.accounts.reward_vault.key();
        lock.unlock_ts = unlock_ts;
        lock.bump = ctx.bumps.lock;

        let received = transfer(
            &ctx.accounts.token_program,
            &ctx.accounts.source,
            &ctx.accounts.lock_mint,
            &mut ctx.accounts.vault,
            ctx.accounts.payer.to_account_info(),
            ctx.remaining_accounts,
            amount,
            Some(max_fee),
            &[],
        )?;

        emit!(Locked {
            lock: ctx.accounts.lock.key(),
            beneficiary: ctx.accounts.lock.beneficiary,
            lock_mint: ctx.accounts.lock.lock_mint,
            vault: ctx.accounts.lock.vault,
            unlock_ts,
            amount,
            received,
        });
        Ok(())
    }

    pub fn deposit<'info>(
        ctx: Context<'info, Deposit<'info>>,
        amount: u64,
        max_fee: u64,
    ) -> Result<()> {
        require!(amount > 0, LockError::ZeroAmount);
        let received = transfer(
            &ctx.accounts.token_program,
            &ctx.accounts.source,
            &ctx.accounts.lock_mint,
            &mut ctx.accounts.vault,
            ctx.accounts.depositor.to_account_info(),
            ctx.remaining_accounts,
            amount,
            Some(max_fee),
            &[],
        )?;
        emit!(Deposited {
            lock: ctx.accounts.lock.key(),
            lock_mint: ctx.accounts.lock.lock_mint,
            amount,
            received,
        });
        Ok(())
    }

    pub fn extend(ctx: Context<Extend>, new_unlock_ts: i64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let lock = &mut ctx.accounts.lock;
        require!(new_unlock_ts > lock.unlock_ts, LockError::NotAnExtension);
        require!(new_unlock_ts <= now + MAX_LOCK_SECONDS, LockError::UnlockTooFar);
        lock.unlock_ts = new_unlock_ts;
        emit!(Extended {
            lock: lock.key(),
            unlock_ts: new_unlock_ts,
        });
        Ok(())
    }

    pub fn sweep<'info>(ctx: Context<'info, Sweep<'info>>, max_fee: u64) -> Result<()> {
        let lock = &ctx.accounts.lock;
        if ctx.accounts.mint.key() == lock.lock_mint {
            let now = Clock::get()?.unix_timestamp;
            require!(now >= lock.unlock_ts, LockError::StillLocked);
        }
        let amount = ctx.accounts.source.amount;
        require!(amount > 0, LockError::ZeroAmount);
        let seeds: &[&[u8]] = &[
            LOCK_SEED,
            lock.lock_mint.as_ref(),
            lock.beneficiary.as_ref(),
            &[lock.bump],
        ];
        let received = transfer(
            &ctx.accounts.token_program,
            &ctx.accounts.source,
            &ctx.accounts.mint,
            &mut ctx.accounts.destination,
            lock.to_account_info(),
            ctx.remaining_accounts,
            amount,
            Some(max_fee),
            &[seeds],
        )?;
        emit!(Swept {
            lock: lock.key(),
            mint: ctx.accounts.mint.key(),
            source: ctx.accounts.source.key(),
            amount,
            received,
        });
        Ok(())
    }

    pub fn withdraw<'info>(
        ctx: Context<'info, Withdraw<'info>>,
        amount: u64,
        max_fee: u64,
    ) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let lock = &ctx.accounts.lock;
        require!(now >= lock.unlock_ts, LockError::StillLocked);
        require!(amount > 0, LockError::ZeroAmount);
        let seeds: &[&[u8]] = &[
            LOCK_SEED,
            lock.lock_mint.as_ref(),
            lock.beneficiary.as_ref(),
            &[lock.bump],
        ];
        let received = transfer(
            &ctx.accounts.token_program,
            &ctx.accounts.vault,
            &ctx.accounts.lock_mint,
            &mut ctx.accounts.destination,
            lock.to_account_info(),
            ctx.remaining_accounts,
            amount,
            Some(max_fee),
            &[seeds],
        )?;
        emit!(Withdrawn {
            lock: lock.key(),
            lock_mint: lock.lock_mint,
            amount,
            received,
        });
        Ok(())
    }
}

fn expected_fee(mint: &InterfaceAccount<Mint>, amount: u64) -> Result<u64> {
    let info = mint.to_account_info();
    if *info.owner != spl_token_2022::ID {
        return Ok(0);
    }
    let data = info.try_borrow_data()?;
    let state = StateWithExtensions::<spl_token_2022::state::Mint>::unpack(&data)?;
    match state.get_extension::<TransferFeeConfig>() {
        Ok(config) => config
            .calculate_epoch_fee(Clock::get()?.epoch, amount)
            .ok_or_else(|| error!(LockError::FeeOverflow)),
        Err(_) => Ok(0),
    }
}

fn transfer<'info>(
    token_program: &Interface<'info, TokenInterface>,
    from: &InterfaceAccount<'info, TokenAccount>,
    mint: &InterfaceAccount<'info, Mint>,
    to: &mut InterfaceAccount<'info, TokenAccount>,
    authority: AccountInfo<'info>,
    extra: &[AccountInfo<'info>],
    amount: u64,
    max_fee: Option<u64>,
    signer_seeds: &[&[&[u8]]],
) -> Result<u64> {
    let mut ix = match max_fee {
        Some(max_fee) if *token_program.key == spl_token_2022::ID => {
            let fee = expected_fee(mint, amount)?;
            require!(fee <= max_fee, LockError::FeeTooHigh);
            spl_token_2022::extension::transfer_fee::instruction::transfer_checked_with_fee(
                token_program.key,
                &from.key(),
                &mint.key(),
                &to.key(),
                authority.key,
                &[],
                amount,
                mint.decimals,
                fee,
            )?
        }
        _ => spl_token_2022::instruction::transfer_checked(
            token_program.key,
            &from.key(),
            &mint.key(),
            &to.key(),
            authority.key,
            &[],
            amount,
            mint.decimals,
        )?,
    };
    let mut infos = vec![
        from.to_account_info(),
        mint.to_account_info(),
        to.to_account_info(),
        authority,
    ];
    for acc in extra {
        ix.accounts.push(AccountMeta {
            pubkey: *acc.key,
            is_signer: acc.is_signer,
            is_writable: acc.is_writable,
        });
        infos.push(acc.clone());
    }
    let before = to.amount;
    invoke_signed(&ix, &infos, signer_seeds)?;
    to.reload()?;
    Ok(to.amount.saturating_sub(before))
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub beneficiary: Signer<'info>,
    #[account(
        init,
        payer = payer,
        space = 8 + Lock::INIT_SPACE,
        seeds = [LOCK_SEED, lock_mint.key().as_ref(), beneficiary.key().as_ref()],
        bump,
    )]
    pub lock: Account<'info, Lock>,
    #[account(mint::token_program = token_program)]
    pub lock_mint: InterfaceAccount<'info, Mint>,
    #[account(
        mint::token_program = reward_token_program,
        constraint = reward_mint.key() != lock_mint.key() @ LockError::RewardIsLockMint,
    )]
    pub reward_mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        token::mint = lock_mint,
        token::authority = payer,
        token::token_program = token_program,
    )]
    pub source: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = lock_mint,
        associated_token::authority = lock,
        associated_token::token_program = token_program,
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = reward_mint,
        associated_token::authority = lock,
        associated_token::token_program = reward_token_program,
    )]
    pub reward_vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
    pub reward_token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Deposit<'info> {
    pub depositor: Signer<'info>,
    #[account(
        seeds = [LOCK_SEED, lock.lock_mint.as_ref(), lock.beneficiary.as_ref()],
        bump = lock.bump,
        has_one = lock_mint,
        has_one = vault,
    )]
    pub lock: Account<'info, Lock>,
    #[account(mint::token_program = token_program)]
    pub lock_mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        token::mint = lock_mint,
        token::authority = depositor,
        token::token_program = token_program,
    )]
    pub source: InterfaceAccount<'info, TokenAccount>,
    #[account(
        mut,
        token::mint = lock_mint,
        token::authority = lock,
        token::token_program = token_program,
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct Extend<'info> {
    pub beneficiary: Signer<'info>,
    #[account(
        mut,
        seeds = [LOCK_SEED, lock.lock_mint.as_ref(), lock.beneficiary.as_ref()],
        bump = lock.bump,
        has_one = beneficiary,
    )]
    pub lock: Account<'info, Lock>,
}

#[derive(Accounts)]
pub struct Sweep<'info> {
    #[account(mut)]
    pub beneficiary: Signer<'info>,
    #[account(
        seeds = [LOCK_SEED, lock.lock_mint.as_ref(), lock.beneficiary.as_ref()],
        bump = lock.bump,
        has_one = beneficiary,
    )]
    pub lock: Account<'info, Lock>,
    #[account(mint::token_program = token_program)]
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        token::mint = mint,
        token::authority = lock,
        token::token_program = token_program,
    )]
    pub source: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init_if_needed,
        payer = beneficiary,
        associated_token::mint = mint,
        associated_token::authority = beneficiary,
        associated_token::token_program = token_program,
    )]
    pub destination: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(mut)]
    pub beneficiary: Signer<'info>,
    #[account(
        seeds = [LOCK_SEED, lock.lock_mint.as_ref(), lock.beneficiary.as_ref()],
        bump = lock.bump,
        has_one = beneficiary,
        has_one = lock_mint,
        has_one = vault,
    )]
    pub lock: Account<'info, Lock>,
    #[account(mint::token_program = token_program)]
    pub lock_mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        token::mint = lock_mint,
        token::authority = lock,
        token::token_program = token_program,
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init_if_needed,
        payer = beneficiary,
        associated_token::mint = lock_mint,
        associated_token::authority = beneficiary,
        associated_token::token_program = token_program,
    )]
    pub destination: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[account]
#[derive(InitSpace)]
pub struct Lock {
    pub beneficiary: Pubkey,
    pub lock_mint: Pubkey,
    pub vault: Pubkey,
    pub reward_mint: Pubkey,
    pub reward_vault: Pubkey,
    pub unlock_ts: i64,
    pub bump: u8,
}

#[event]
pub struct Locked {
    pub lock: Pubkey,
    pub beneficiary: Pubkey,
    pub lock_mint: Pubkey,
    pub vault: Pubkey,
    pub unlock_ts: i64,
    pub amount: u64,
    pub received: u64,
}

#[event]
pub struct Deposited {
    pub lock: Pubkey,
    pub lock_mint: Pubkey,
    pub amount: u64,
    pub received: u64,
}

#[event]
pub struct Extended {
    pub lock: Pubkey,
    pub unlock_ts: i64,
}

#[event]
pub struct Swept {
    pub lock: Pubkey,
    pub mint: Pubkey,
    pub source: Pubkey,
    pub amount: u64,
    pub received: u64,
}

#[event]
pub struct Withdrawn {
    pub lock: Pubkey,
    pub lock_mint: Pubkey,
    pub amount: u64,
    pub received: u64,
}

#[error_code]
pub enum LockError {
    #[msg("unlock timestamp must be in the future")]
    UnlockInPast,
    #[msg("unlock timestamp is more than 20 years away")]
    UnlockTooFar,
    #[msg("amount must be greater than zero")]
    ZeroAmount,
    #[msg("new unlock timestamp must be later than the current one")]
    NotAnExtension,
    #[msg("tokens are still locked")]
    StillLocked,
    #[msg("reward mint must differ from the locked mint")]
    RewardIsLockMint,
    #[msg("transfer fee exceeds the accepted maximum")]
    FeeTooHigh,
    #[msg("transfer fee calculation overflowed")]
    FeeOverflow,
}
