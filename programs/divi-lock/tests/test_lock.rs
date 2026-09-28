use {
    anchor_lang::{
        prelude::{Clock, Pubkey},
        solana_program::{
            instruction::{AccountMeta, Instruction},
            system_instruction, system_program,
        },
        AccountDeserialize, InstructionData, ToAccountMetas,
    },
    anchor_spl::{
        associated_token::{
            get_associated_token_address_with_program_id, spl_associated_token_account,
            ID as ATA_ID,
        },
        token_2022::{spl_token_2022, ID as TOKEN_2022_ID},
    },
    litesvm::{types::TransactionResult, LiteSVM},
    solana_keypair::Keypair,
    solana_message::{Message, VersionedMessage},
    solana_signer::Signer,
    solana_transaction::versioned::VersionedTransaction,
    spl_token_2022::{
        extension::{transfer_fee, BaseStateWithExtensions, ExtensionType, StateWithExtensions},
        state::{Account as TokenAccount, Mint},
    },
    std::fs,
};

const FEE_BPS: u16 = 300;
const MAX_FEE: u64 = 1_000_000_000_000_000;
const DECIMALS: u8 = 6;
const ONE: u64 = 1_000_000;
const START_TS: i64 = 1_800_000_000;
const UNLOCK_TS: i64 = START_TS + 86_400 * 180;
const PROGRAM_SO: &str = concat!(env!("CARGO_TARGET_TMPDIR"), "/../deploy/divi_lock.so");
const HOOK_SO: &str = concat!(env!("CARGO_TARGET_TMPDIR"), "/../deploy/test_hook.so");
const HOOK_SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../test-hook/src/lib.rs");

fn fee(amount: u64) -> u64 {
    ((amount as u128 * FEE_BPS as u128).div_ceil(10_000) as u64).min(MAX_FEE)
}

fn assert_fresh(so_path: &str, src_path: &str) {
    let so = fs::metadata(so_path).expect("run cargo build-sbf first").modified().unwrap();
    let src = fs::metadata(src_path).unwrap().modified().unwrap();
    assert!(so >= src, "{so_path} is older than {src_path}; run cargo build-sbf");
}

fn assert_program_is_fresh() {
    assert_fresh(PROGRAM_SO, concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"));
    assert_fresh(HOOK_SO, HOOK_SRC);
}

struct Env {
    svm: LiteSVM,
    payer: Keypair,
    beneficiary: Keypair,
    stranger: Keypair,
    lock_mint: Pubkey,
    reward_mint: Pubkey,
    lock: Pubkey,
    vault: Pubkey,
    reward_vault: Pubkey,
    payer_source: Pubkey,
}

impl Env {
    fn new() -> Self {
        assert_program_is_fresh();
        let program_id = divi_lock::id();
        let mut svm = LiteSVM::new();
        svm.add_program(program_id, &fs::read(PROGRAM_SO).unwrap()).unwrap();
        let payer = Keypair::new();
        let beneficiary = Keypair::new();
        let stranger = Keypair::new();
        for kp in [&payer, &beneficiary, &stranger] {
            svm.airdrop(&kp.pubkey(), 10_000_000_000).unwrap();
        }
        let lock_mint_kp = Keypair::new();
        let reward_mint_kp = Keypair::new();
        let lock_mint = lock_mint_kp.pubkey();
        let reward_mint = reward_mint_kp.pubkey();
        let (lock, _) = Pubkey::find_program_address(
            &[divi_lock::LOCK_SEED, lock_mint.as_ref(), beneficiary.pubkey().as_ref()],
            &program_id,
        );
        let vault = get_associated_token_address_with_program_id(&lock, &lock_mint, &TOKEN_2022_ID);
        let reward_vault =
            get_associated_token_address_with_program_id(&lock, &reward_mint, &TOKEN_2022_ID);
        let payer_source =
            get_associated_token_address_with_program_id(&payer.pubkey(), &lock_mint, &TOKEN_2022_ID);
        let mut env = Env {
            svm,
            payer,
            beneficiary,
            stranger,
            lock_mint,
            reward_mint,
            lock,
            vault,
            reward_vault,
            payer_source,
        };
        env.set_clock(START_TS);
        env.create_mint(&lock_mint_kp, true);
        env.create_mint(&reward_mint_kp, false);
        env.create_ata(&env.payer.pubkey(), &lock_mint);
        env.mint_to(&lock_mint, &payer_source, 2_000_000 * ONE);
        env
    }

    fn set_clock(&mut self, ts: i64) {
        let mut clock = self.svm.get_sysvar::<Clock>();
        clock.unix_timestamp = ts;
        self.svm.set_sysvar(&clock);
    }

    fn send(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> TransactionResult {
        self.svm.expire_blockhash();
        let blockhash = self.svm.latest_blockhash();
        let msg = Message::new_with_blockhash(ixs, Some(&signers[0].pubkey()), &blockhash);
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), signers).unwrap();
        self.svm.send_transaction(tx)
    }

    fn create_mint(&mut self, mint: &Keypair, with_fee: bool) {
        self.create_mint_with_hook(mint, with_fee, None)
    }

    fn create_mint_with_hook(&mut self, mint: &Keypair, with_fee: bool, hook: Option<Pubkey>) {
        let mut extensions = Vec::new();
        if with_fee {
            extensions.push(ExtensionType::TransferFeeConfig);
        }
        if hook.is_some() {
            extensions.push(ExtensionType::TransferHook);
        }
        let space = ExtensionType::try_calculate_account_len::<Mint>(&extensions).unwrap();
        let lamports = self.svm.minimum_balance_for_rent_exemption(space);
        let payer = self.payer.pubkey();
        let mut ixs = vec![system_instruction::create_account(
            &payer,
            &mint.pubkey(),
            lamports,
            space as u64,
            &TOKEN_2022_ID,
        )];
        if with_fee {
            ixs.push(
                transfer_fee::instruction::initialize_transfer_fee_config(
                    &TOKEN_2022_ID,
                    &mint.pubkey(),
                    Some(&payer),
                    Some(&payer),
                    FEE_BPS,
                    MAX_FEE,
                )
                .unwrap(),
            );
        }
        if let Some(hook) = hook {
            ixs.push(
                spl_token_2022::extension::transfer_hook::instruction::initialize(
                    &TOKEN_2022_ID,
                    &mint.pubkey(),
                    Some(payer),
                    Some(hook),
                )
                .unwrap(),
            );
        }
        ixs.push(
            spl_token_2022::instruction::initialize_mint2(
                &TOKEN_2022_ID,
                &mint.pubkey(),
                &payer,
                None,
                DECIMALS,
            )
            .unwrap(),
        );
        let payer_kp = self.payer.insecure_clone();
        self.send(&ixs, &[&payer_kp, mint]).unwrap();
    }

    fn counter_value(&self, counter: &Pubkey) -> u64 {
        let account = self.svm.get_account(counter).unwrap();
        u64::from_le_bytes(account.data[..8].try_into().unwrap())
    }

    fn set_fee(&mut self, bps: u16, max_fee: u64) {
        let ix = transfer_fee::instruction::set_transfer_fee(
            &TOKEN_2022_ID,
            &self.lock_mint,
            &self.payer.pubkey(),
            &[],
            bps,
            max_fee,
        )
        .unwrap();
        let payer_kp = self.payer.insecure_clone();
        self.send(&[ix], &[&payer_kp]).unwrap();
        let mut clock = self.svm.get_sysvar::<Clock>();
        clock.epoch += 2;
        self.svm.set_sysvar(&clock);
    }

    fn create_ata(&mut self, owner: &Pubkey, mint: &Pubkey) {
        let ix = spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            &self.payer.pubkey(),
            owner,
            mint,
            &TOKEN_2022_ID,
        );
        let payer_kp = self.payer.insecure_clone();
        self.send(&[ix], &[&payer_kp]).unwrap();
    }

    fn mint_to(&mut self, mint: &Pubkey, dest: &Pubkey, amount: u64) {
        let ix = spl_token_2022::instruction::mint_to(
            &TOKEN_2022_ID,
            mint,
            dest,
            &self.payer.pubkey(),
            &[],
            amount,
        )
        .unwrap();
        let payer_kp = self.payer.insecure_clone();
        self.send(&[ix], &[&payer_kp]).unwrap();
    }

    fn balance(&self, ata: &Pubkey) -> u64 {
        let account = self.svm.get_account(ata).expect("token account missing");
        StateWithExtensions::<TokenAccount>::unpack(&account.data)
            .unwrap()
            .base
            .amount
    }

    fn withheld(&self, ata: &Pubkey) -> u64 {
        let account = self.svm.get_account(ata).unwrap();
        let state = StateWithExtensions::<TokenAccount>::unpack(&account.data).unwrap();
        state
            .get_extension::<transfer_fee::TransferFeeAmount>()
            .map(|e| u64::from(e.withheld_amount))
            .unwrap_or(0)
    }

    fn lock_state(&self) -> divi_lock::Lock {
        let account = self.svm.get_account(&self.lock).unwrap();
        divi_lock::Lock::try_deserialize(&mut account.data.as_slice()).unwrap()
    }

    fn initialize_ix(&self, beneficiary: Pubkey, unlock_ts: i64, amount: u64, max_fee: u64) -> Instruction {
        let (lock, _) = Pubkey::find_program_address(
            &[divi_lock::LOCK_SEED, self.lock_mint.as_ref(), beneficiary.as_ref()],
            &divi_lock::id(),
        );
        Instruction::new_with_bytes(
            divi_lock::id(),
            &divi_lock::instruction::Initialize { unlock_ts, amount, max_fee }.data(),
            divi_lock::accounts::Initialize {
                payer: self.payer.pubkey(),
                beneficiary,
                lock,
                lock_mint: self.lock_mint,
                reward_mint: self.reward_mint,
                source: self.payer_source,
                vault: get_associated_token_address_with_program_id(&lock, &self.lock_mint, &TOKEN_2022_ID),
                reward_vault: get_associated_token_address_with_program_id(&lock, &self.reward_mint, &TOKEN_2022_ID),
                token_program: TOKEN_2022_ID,
                reward_token_program: TOKEN_2022_ID,
                associated_token_program: ATA_ID,
                system_program: system_program::ID,
            }
            .to_account_metas(None),
        )
    }

    fn initialize_with_fee(&mut self, unlock_ts: i64, amount: u64, max_fee: u64) -> TransactionResult {
        let ix = self.initialize_ix(self.beneficiary.pubkey(), unlock_ts, amount, max_fee);
        let payer_kp = self.payer.insecure_clone();
        let beneficiary_kp = self.beneficiary.insecure_clone();
        if payer_kp.pubkey() == beneficiary_kp.pubkey() {
            return self.send(&[ix], &[&payer_kp]);
        }
        self.send(&[ix], &[&payer_kp, &beneficiary_kp])
    }

    fn initialize(&mut self, unlock_ts: i64, amount: u64) -> TransactionResult {
        self.initialize_with_fee(unlock_ts, amount, fee(amount))
    }

    fn deposit_with_fee(&mut self, amount: u64, max_fee: u64) -> TransactionResult {
        let ix = Instruction::new_with_bytes(
            divi_lock::id(),
            &divi_lock::instruction::Deposit { amount, max_fee }.data(),
            divi_lock::accounts::Deposit {
                depositor: self.payer.pubkey(),
                lock: self.lock,
                lock_mint: self.lock_mint,
                source: self.payer_source,
                vault: self.vault,
                token_program: TOKEN_2022_ID,
            }
            .to_account_metas(None),
        );
        let payer_kp = self.payer.insecure_clone();
        self.send(&[ix], &[&payer_kp])
    }

    fn deposit(&mut self, amount: u64) -> TransactionResult {
        self.deposit_with_fee(amount, fee(amount))
    }

    fn extend(&mut self, signer: &Keypair, new_unlock_ts: i64) -> TransactionResult {
        let ix = Instruction::new_with_bytes(
            divi_lock::id(),
            &divi_lock::instruction::Extend { new_unlock_ts }.data(),
            divi_lock::accounts::Extend {
                beneficiary: signer.pubkey(),
                lock: self.lock,
            }
            .to_account_metas(None),
        );
        self.send(&[ix], &[signer])
    }

    fn sweep(&mut self, signer: &Keypair, mint: Pubkey, source: Pubkey) -> TransactionResult {
        self.sweep_with_extra(signer, mint, source, &[])
    }

    fn sweep_with_extra(
        &mut self,
        signer: &Keypair,
        mint: Pubkey,
        source: Pubkey,
        extra: &[Pubkey],
    ) -> TransactionResult {
        let metas: Vec<AccountMeta> = extra.iter().map(|k| AccountMeta::new_readonly(*k, false)).collect();
        self.sweep_with_metas(signer, mint, source, metas)
    }

    fn sweep_with_metas(
        &mut self,
        signer: &Keypair,
        mint: Pubkey,
        source: Pubkey,
        extra: Vec<AccountMeta>,
    ) -> TransactionResult {
        self.sweep_with_metas_and_fee(signer, mint, source, extra, TOKEN_2022_ID, u64::MAX)
    }

    fn sweep_with_metas_and_fee(
        &mut self,
        signer: &Keypair,
        mint: Pubkey,
        source: Pubkey,
        extra: Vec<AccountMeta>,
        token_program: Pubkey,
        max_fee: u64,
    ) -> TransactionResult {
        let destination =
            get_associated_token_address_with_program_id(&signer.pubkey(), &mint, &token_program);
        let mut metas = divi_lock::accounts::Sweep {
            beneficiary: signer.pubkey(),
            lock: self.lock,
            mint,
            source,
            destination,
            token_program,
            associated_token_program: ATA_ID,
            system_program: system_program::ID,
        }
        .to_account_metas(None);
        metas.extend(extra);
        let ix = Instruction::new_with_bytes(
            divi_lock::id(),
            &divi_lock::instruction::Sweep { max_fee }.data(),
            metas,
        );
        self.send(&[ix], &[signer])
    }

    fn withdraw_with_fee(&mut self, signer: &Keypair, amount: u64, max_fee: u64) -> TransactionResult {
        let destination = get_associated_token_address_with_program_id(
            &signer.pubkey(),
            &self.lock_mint,
            &TOKEN_2022_ID,
        );
        let ix = Instruction::new_with_bytes(
            divi_lock::id(),
            &divi_lock::instruction::Withdraw { amount, max_fee }.data(),
            divi_lock::accounts::Withdraw {
                beneficiary: signer.pubkey(),
                lock: self.lock,
                lock_mint: self.lock_mint,
                vault: self.vault,
                destination,
                token_program: TOKEN_2022_ID,
                associated_token_program: ATA_ID,
                system_program: system_program::ID,
            }
            .to_account_metas(None),
        );
        self.send(&[ix], &[signer])
    }

    fn withdraw(&mut self, signer: &Keypair, amount: u64) -> TransactionResult {
        self.withdraw_with_fee(signer, amount, fee(amount))
    }
}

fn assert_err(res: TransactionResult, code: &str) {
    let failed = res.expect_err("transaction unexpectedly succeeded");
    let logs = failed.meta.logs.join("\n");
    assert!(logs.contains(code), "expected {code}, got:\n{logs}");
}

#[test]
fn rejects_unlock_in_past() {
    let mut env = Env::new();
    assert_err(env.initialize(START_TS - 1, ONE), "UnlockInPast");
}

#[test]
fn rejects_unlock_too_far() {
    let mut env = Env::new();
    assert_err(
        env.initialize(START_TS + divi_lock::MAX_LOCK_SECONDS + 1, ONE),
        "UnlockTooFar",
    );
    env.initialize(START_TS + divi_lock::MAX_LOCK_SECONDS, ONE).unwrap();
}

#[test]
fn rejects_init_without_beneficiary_signature() {
    let env = Env::new();
    let ix = env.initialize_ix(env.stranger.pubkey(), UNLOCK_TS, ONE, fee(ONE));
    let payer_kp = env.payer.insecure_clone();
    let blockhash = env.svm.latest_blockhash();
    let msg = Message::new_with_blockhash(&[ix], Some(&payer_kp.pubkey()), &blockhash);
    assert!(VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[&payer_kp]).is_err());
}

#[test]
fn rejects_reward_mint_equal_to_lock_mint() {
    let mut env = Env::new();
    env.reward_mint = env.lock_mint;
    env.reward_vault = env.vault;
    let logs = env.initialize(UNLOCK_TS, ONE).expect_err("init succeeded").meta.logs.join("\n");
    assert!(logs.contains("ConstraintDuplicateMutableAccount"), "{logs}");
}

#[test]
fn locks_and_withholds_transfer_fee() {
    let mut env = Env::new();
    let amount = 1_000_000 * ONE;
    env.initialize(UNLOCK_TS, amount).unwrap();
    let lock = env.lock_state();
    assert_eq!(lock.beneficiary, env.beneficiary.pubkey());
    assert_eq!(lock.lock_mint, env.lock_mint);
    assert_eq!(lock.vault, env.vault);
    assert_eq!(lock.reward_mint, env.reward_mint);
    assert_eq!(lock.reward_vault, env.reward_vault);
    assert_eq!(lock.unlock_ts, UNLOCK_TS);
    assert_eq!(env.balance(&env.vault), amount - fee(amount));
    assert_eq!(env.withheld(&env.vault), fee(amount));
    assert_eq!(env.balance(&env.reward_vault), 0);
}

#[test]
fn init_succeeds_when_vault_ata_was_pre_created() {
    let mut env = Env::new();
    let (lock, lock_mint) = (env.lock, env.lock_mint);
    env.create_ata(&lock, &lock_mint);
    env.initialize(UNLOCK_TS, 1_000 * ONE).unwrap();
    assert_eq!(env.balance(&env.vault), 1_000 * ONE - fee(1_000 * ONE));
}

#[test]
fn rejects_fee_above_accepted_maximum() {
    let mut env = Env::new();
    let amount = 1_000 * ONE;
    assert_err(env.initialize_with_fee(UNLOCK_TS, amount, fee(amount) - 1), "FeeTooHigh");
    env.initialize_with_fee(UNLOCK_TS, amount, fee(amount)).unwrap();
    assert_err(env.deposit_with_fee(amount, 0), "FeeTooHigh");
    env.deposit_with_fee(amount, u64::MAX).unwrap();
}

#[test]
fn withdraw_refuses_a_raised_fee() {
    let mut env = Env::new();
    let amount = 1_000 * ONE;
    env.initialize(UNLOCK_TS, amount).unwrap();
    env.set_fee(10_000, u64::MAX);
    env.set_clock(UNLOCK_TS);
    let beneficiary = env.beneficiary.insecure_clone();
    let locked = env.balance(&env.vault);
    assert_err(env.withdraw_with_fee(&beneficiary, locked, fee(locked)), "FeeTooHigh");
    env.set_fee(FEE_BPS, MAX_FEE);
    env.withdraw_with_fee(&beneficiary, locked, fee(locked)).unwrap();
}

#[test]
fn accepts_top_up_deposit() {
    let mut env = Env::new();
    env.initialize(UNLOCK_TS, 1_000 * ONE).unwrap();
    let before = env.balance(&env.vault);
    env.deposit(500 * ONE).unwrap();
    assert_eq!(env.balance(&env.vault), before + 500 * ONE - fee(500 * ONE));
}

#[test]
fn deposit_rejects_foreign_vault() {
    let mut env = Env::new();
    env.initialize(UNLOCK_TS, 1_000 * ONE).unwrap();
    env.vault = env.payer_source;
    assert!(env.deposit(ONE).is_err());
}

#[test]
fn refuses_withdraw_before_unlock() {
    let mut env = Env::new();
    env.initialize(UNLOCK_TS, 1_000 * ONE).unwrap();
    env.set_clock(UNLOCK_TS - 1);
    let beneficiary = env.beneficiary.insecure_clone();
    assert_err(env.withdraw(&beneficiary, 1), "StillLocked");
}

#[test]
fn refuses_withdraw_by_stranger() {
    let mut env = Env::new();
    env.initialize(UNLOCK_TS, 1_000 * ONE).unwrap();
    env.set_clock(UNLOCK_TS + 1);
    let stranger = env.stranger.insecure_clone();
    assert_err(env.withdraw(&stranger, 1), "ConstraintHasOne");
}

#[test]
fn sweeps_rewards_while_locked() {
    let mut env = Env::new();
    env.initialize(UNLOCK_TS, 1_000 * ONE).unwrap();
    let reward = 250 * ONE;
    let (reward_mint, reward_vault) = (env.reward_mint, env.reward_vault);
    env.mint_to(&reward_mint, &reward_vault, reward);
    let beneficiary = env.beneficiary.insecure_clone();
    env.sweep(&beneficiary, reward_mint, reward_vault).unwrap();
    let dest = get_associated_token_address_with_program_id(
        &beneficiary.pubkey(),
        &reward_mint,
        &TOKEN_2022_ID,
    );
    assert_eq!(env.balance(&dest), reward);
    assert_eq!(env.balance(&reward_vault), 0);
    assert_eq!(env.balance(&env.vault), 1_000 * ONE - fee(1_000 * ONE));
}

#[test]
fn sweeps_unexpected_token_account_owned_by_lock() {
    let mut env = Env::new();
    env.initialize(UNLOCK_TS, 1_000 * ONE).unwrap();
    let other_mint_kp = Keypair::new();
    env.create_mint(&other_mint_kp, false);
    let other_mint = other_mint_kp.pubkey();
    let lock = env.lock;
    env.create_ata(&lock, &other_mint);
    let ata = get_associated_token_address_with_program_id(&lock, &other_mint, &TOKEN_2022_ID);
    env.mint_to(&other_mint, &ata, 7 * ONE);
    let beneficiary = env.beneficiary.insecure_clone();
    env.sweep(&beneficiary, other_mint, ata).unwrap();
    let dest = get_associated_token_address_with_program_id(
        &beneficiary.pubkey(),
        &other_mint,
        &TOKEN_2022_ID,
    );
    assert_eq!(env.balance(&dest), 7 * ONE);
}

#[test]
fn sweep_forwards_extra_accounts_to_token_program() {
    let mut env = Env::new();
    env.initialize(UNLOCK_TS, 1_000 * ONE).unwrap();
    let (reward_mint, reward_vault) = (env.reward_mint, env.reward_vault);
    env.mint_to(&reward_mint, &reward_vault, 3 * ONE);
    let beneficiary = env.beneficiary.insecure_clone();
    let extra = [Keypair::new().pubkey(), Keypair::new().pubkey()];
    env.sweep_with_extra(&beneficiary, reward_mint, reward_vault, &extra)
        .unwrap();
    assert_eq!(env.balance(&reward_vault), 0);
}

#[test]
fn sweeps_through_a_real_transfer_hook_with_writable_and_readonly_extras() {
    let mut env = Env::new();
    env.initialize(UNLOCK_TS, 1_000 * ONE).unwrap();
    let hooked_kp = Keypair::new();
    let hooked = hooked_kp.pubkey();
    let beneficiary = env.beneficiary.insecure_clone();
    let hook_program = Pubkey::new_unique();
    env.svm.add_program(hook_program, &fs::read(HOOK_SO).unwrap()).unwrap();
    env.create_mint_with_hook(&hooked_kp, false, Some(hook_program));
    let meta_list = spl_transfer_hook_interface::get_extra_account_metas_address(&hooked, &hook_program);
    let (counter, _) = test_hook::counter_address(&hooked, &hook_program);
    let mut init = spl_transfer_hook_interface::instruction::initialize_extra_account_meta_list(
        &hook_program,
        &meta_list,
        &hooked,
        &env.payer.pubkey(),
        &test_hook::extra_metas(&beneficiary.pubkey()),
    );
    init.accounts.push(AccountMeta::new(counter, false));
    let payer_kp = env.payer.insecure_clone();
    env.send(&[init], &[&payer_kp]).unwrap();
    let lock = env.lock;
    env.create_ata(&lock, &hooked);
    let source = get_associated_token_address_with_program_id(&lock, &hooked, &TOKEN_2022_ID);
    env.mint_to(&hooked, &source, 9 * ONE);

    assert!(env.sweep(&beneficiary, hooked, source).is_err());

    let extras = vec![
        AccountMeta::new(counter, false),
        AccountMeta::new_readonly(beneficiary.pubkey(), false),
        AccountMeta::new_readonly(hook_program, false),
        AccountMeta::new_readonly(meta_list, false),
    ];
    env.sweep_with_metas(&beneficiary, hooked, source, extras).unwrap();
    let dest = get_associated_token_address_with_program_id(&beneficiary.pubkey(), &hooked, &TOKEN_2022_ID);
    assert_eq!(env.balance(&dest), 9 * ONE);
    assert_eq!(env.counter_value(&counter), 1);
}

#[test]
fn sweep_of_lock_mint_only_after_unlock_and_fee_capped() {
    let mut env = Env::new();
    env.initialize(UNLOCK_TS, 1_000 * ONE).unwrap();
    let beneficiary = env.beneficiary.insecure_clone();
    let (lock_mint, vault) = (env.lock_mint, env.vault);
    assert_err(env.sweep(&beneficiary, lock_mint, vault), "StillLocked");
    env.set_clock(UNLOCK_TS);
    let locked = env.balance(&vault);
    assert_err(
        env.sweep_with_metas_and_fee(&beneficiary, lock_mint, vault, vec![], TOKEN_2022_ID, fee(locked) - 1),
        "FeeTooHigh",
    );
    env.sweep_with_metas_and_fee(&beneficiary, lock_mint, vault, vec![], TOKEN_2022_ID, fee(locked)).unwrap();
    let dest = get_associated_token_address_with_program_id(
        &beneficiary.pubkey(),
        &lock_mint,
        &TOKEN_2022_ID,
    );
    assert_eq!(env.balance(&dest), locked - fee(locked));
}

#[test]
fn refuses_sweep_by_stranger() {
    let mut env = Env::new();
    env.initialize(UNLOCK_TS, 1_000 * ONE).unwrap();
    let (reward_mint, reward_vault) = (env.reward_mint, env.reward_vault);
    env.mint_to(&reward_mint, &reward_vault, ONE);
    let stranger = env.stranger.insecure_clone();
    assert_err(env.sweep(&stranger, reward_mint, reward_vault), "ConstraintHasOne");
}

#[test]
fn refuses_sweep_of_account_not_owned_by_lock() {
    let mut env = Env::new();
    env.initialize(UNLOCK_TS, 1_000 * ONE).unwrap();
    let (reward_mint, stranger_pk) = (env.reward_mint, env.stranger.pubkey());
    env.create_ata(&stranger_pk, &reward_mint);
    let stranger_ata =
        get_associated_token_address_with_program_id(&stranger_pk, &reward_mint, &TOKEN_2022_ID);
    env.mint_to(&reward_mint, &stranger_ata, ONE);
    let beneficiary = env.beneficiary.insecure_clone();
    assert_err(env.sweep(&beneficiary, reward_mint, stranger_ata), "ConstraintTokenOwner");
}

#[test]
fn refuses_shortening_and_allows_extension() {
    let mut env = Env::new();
    env.initialize(UNLOCK_TS, 1_000 * ONE).unwrap();
    let beneficiary = env.beneficiary.insecure_clone();
    let stranger = env.stranger.insecure_clone();
    assert_err(env.extend(&beneficiary, UNLOCK_TS - 1), "NotAnExtension");
    assert_err(env.extend(&beneficiary, UNLOCK_TS), "NotAnExtension");
    assert_err(env.extend(&stranger, UNLOCK_TS + 1), "ConstraintHasOne");
    assert_err(
        env.extend(&beneficiary, START_TS + divi_lock::MAX_LOCK_SECONDS + 1),
        "UnlockTooFar",
    );
    env.extend(&beneficiary, UNLOCK_TS + 86_400).unwrap();
    assert_eq!(env.lock_state().unlock_ts, UNLOCK_TS + 86_400);
    env.set_clock(UNLOCK_TS + 1);
    assert_err(env.withdraw(&beneficiary, 1), "StillLocked");
}

#[test]
fn withdraws_after_unlock() {
    let mut env = Env::new();
    let amount = 1_000_000 * ONE;
    env.initialize(UNLOCK_TS, amount).unwrap();
    env.set_clock(UNLOCK_TS);
    let locked = env.balance(&env.vault);
    let beneficiary = env.beneficiary.insecure_clone();
    env.withdraw(&beneficiary, locked / 2).unwrap();
    env.withdraw(&beneficiary, locked - locked / 2).unwrap();
    let dest = get_associated_token_address_with_program_id(
        &beneficiary.pubkey(),
        &env.lock_mint,
        &TOKEN_2022_ID,
    );
    assert_eq!(
        env.balance(&dest),
        locked / 2 - fee(locked / 2) + (locked - locked / 2) - fee(locked - locked / 2)
    );
    assert_eq!(env.balance(&env.vault), 0);
    assert_err(env.withdraw(&beneficiary, 0), "ZeroAmount");
    assert!(env.withdraw(&beneficiary, 1).is_err());
}

#[test]
fn fee_rounds_up_on_indivisible_amounts() {
    let mut env = Env::new();
    let amount = ONE + 1;
    assert_eq!(fee(amount), 30_001);
    env.initialize(UNLOCK_TS, amount).unwrap();
    assert_eq!(env.balance(&env.vault), amount - 30_001);
    assert_eq!(env.withheld(&env.vault), 30_001);
    env.set_clock(UNLOCK_TS);
    let beneficiary = env.beneficiary.insecure_clone();
    let odd = 333_333;
    env.withdraw(&beneficiary, odd).unwrap();
    let dest = get_associated_token_address_with_program_id(&beneficiary.pubkey(), &env.lock_mint, &TOKEN_2022_ID);
    assert_eq!(env.balance(&dest), odd - fee(odd));
}

#[test]
fn init_with_payer_equal_to_beneficiary() {
    let mut env = Env::new();
    let beneficiary_pk = env.beneficiary.pubkey();
    let lock_mint = env.lock_mint;
    env.create_ata(&beneficiary_pk, &lock_mint);
    let source = get_associated_token_address_with_program_id(&beneficiary_pk, &lock_mint, &TOKEN_2022_ID);
    env.mint_to(&lock_mint, &source, 10 * ONE);
    env.payer = env.beneficiary.insecure_clone();
    env.payer_source = source;
    env.initialize(UNLOCK_TS, 10 * ONE).unwrap();
    assert_eq!(env.balance(&env.vault), 10 * ONE - fee(10 * ONE));
    assert_eq!(env.lock_state().beneficiary, beneficiary_pk);
}

#[test]
fn sweeps_classic_spl_token_rewards() {
    let mut env = Env::new();
    env.initialize(UNLOCK_TS, 1_000 * ONE).unwrap();
    let classic = anchor_spl::token::ID;
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let payer = env.payer.pubkey();
    use anchor_lang::solana_program::program_pack::Pack;
    let space = spl_token_2022::state::Mint::LEN;
    let lamports = env.svm.minimum_balance_for_rent_exemption(space);
    let ixs = vec![
        system_instruction::create_account(&payer, &mint, lamports, space as u64, &classic),
        spl_token_2022::instruction::initialize_mint2(&classic, &mint, &payer, None, DECIMALS).unwrap(),
    ];
    let payer_kp = env.payer.insecure_clone();
    env.send(&ixs, &[&payer_kp, &mint_kp]).unwrap();
    let lock = env.lock;
    let create = spl_associated_token_account::instruction::create_associated_token_account_idempotent(&payer, &lock, &mint, &classic);
    env.send(&[create], &[&payer_kp]).unwrap();
    let source = get_associated_token_address_with_program_id(&lock, &mint, &classic);
    let mint_ix = spl_token_2022::instruction::mint_to(&classic, &mint, &source, &payer, &[], 4 * ONE).unwrap();
    env.send(&[mint_ix], &[&payer_kp]).unwrap();
    let beneficiary = env.beneficiary.insecure_clone();
    env.sweep_with_metas_and_fee(&beneficiary, mint, source, vec![], classic, 0).unwrap();
    let dest = get_associated_token_address_with_program_id(&beneficiary.pubkey(), &mint, &classic);
    let account = env.svm.get_account(&dest).unwrap();
    let amount = u64::from_le_bytes(account.data[64..72].try_into().unwrap());
    assert_eq!(amount, 4 * ONE);
}

#[test]
fn rejects_duplicate_lock_for_same_pair() {
    let mut env = Env::new();
    env.initialize(UNLOCK_TS, 1_000 * ONE).unwrap();
    let res = env.initialize(UNLOCK_TS, 1_000 * ONE);
    assert!(res.is_err());
}

