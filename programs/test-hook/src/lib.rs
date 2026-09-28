use solana_account_info::{next_account_info, AccountInfo};
use solana_cpi::invoke_signed;
use solana_program_error::{ProgramError, ProgramResult};
use solana_pubkey::Pubkey;
use solana_rent::Rent;
use solana_sysvar::Sysvar;
use spl_tlv_account_resolution::{account::ExtraAccountMeta, seeds::Seed, state::ExtraAccountMetaList};
use spl_transfer_hook_interface::{
    collect_extra_account_metas_signer_seeds, get_extra_account_metas_address_and_bump_seed,
    instruction::{ExecuteInstruction, TransferHookInstruction},
};

pub const COUNTER_SEED: &[u8] = b"counter";

pub fn counter_address(mint: &Pubkey, program_id: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[COUNTER_SEED, mint.as_ref()], program_id)
}

pub fn extra_metas(marker: &Pubkey) -> Vec<ExtraAccountMeta> {
    vec![
        ExtraAccountMeta::new_with_seeds(
            &[
                Seed::Literal { bytes: COUNTER_SEED.to_vec() },
                Seed::AccountKey { index: 1 },
            ],
            false,
            true,
        )
        .unwrap(),
        ExtraAccountMeta::new_with_pubkey(marker, false, false).unwrap(),
    ]
}

#[cfg(not(feature = "no-entrypoint"))]
solana_program_entrypoint::entrypoint!(process_instruction);

pub fn process_instruction(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    match TransferHookInstruction::unpack(data)? {
        TransferHookInstruction::Execute { .. } => execute(program_id, accounts),
        TransferHookInstruction::InitializeExtraAccountMetaList { extra_account_metas } => {
            initialize(program_id, accounts, &extra_account_metas)
        }
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

fn execute(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let _source = next_account_info(iter)?;
    let mint = next_account_info(iter)?;
    let _destination = next_account_info(iter)?;
    let _owner = next_account_info(iter)?;
    let _extra_metas = next_account_info(iter)?;
    let counter = next_account_info(iter)?;
    let marker = next_account_info(iter)?;
    let (expected, _) = counter_address(mint.key, program_id);
    if *counter.key != expected || counter.owner != program_id {
        return Err(ProgramError::InvalidAccountData);
    }
    if marker.is_writable || marker.is_signer || *marker.key == Pubkey::default() {
        return Err(ProgramError::InvalidAccountData);
    }
    let mut data = counter.try_borrow_mut_data()?;
    let next = u64::from_le_bytes(data[..8].try_into().unwrap()) + 1;
    data[..8].copy_from_slice(&next.to_le_bytes());
    Ok(())
}

fn initialize(program_id: &Pubkey, accounts: &[AccountInfo], metas: &[ExtraAccountMeta]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let extra_metas = next_account_info(iter)?;
    let mint = next_account_info(iter)?;
    let payer = next_account_info(iter)?;
    let _system_program = next_account_info(iter)?;
    let counter = next_account_info(iter)?;
    let (expected, bump) = get_extra_account_metas_address_and_bump_seed(mint.key, program_id);
    if *extra_metas.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }
    let rent = Rent::get()?;
    let size = ExtraAccountMetaList::size_of(metas.len())?;
    let bump_seed = [bump];
    let seeds = collect_extra_account_metas_signer_seeds(mint.key, &bump_seed);
    invoke_signed(
        &solana_system_interface::instruction::create_account(
            payer.key,
            extra_metas.key,
            rent.minimum_balance(size),
            size as u64,
            program_id,
        ),
        &[payer.clone(), extra_metas.clone()],
        &[&seeds],
    )?;
    ExtraAccountMetaList::init::<ExecuteInstruction>(&mut extra_metas.try_borrow_mut_data()?, metas)?;
    let (counter_key, counter_bump) = counter_address(mint.key, program_id);
    if *counter.key != counter_key {
        return Err(ProgramError::InvalidSeeds);
    }
    invoke_signed(
        &solana_system_interface::instruction::create_account(
            payer.key,
            counter.key,
            rent.minimum_balance(8),
            8,
            program_id,
        ),
        &[payer.clone(), counter.clone()],
        &[&[COUNTER_SEED, mint.key.as_ref(), &[counter_bump]]],
    )?;
    Ok(())
}
