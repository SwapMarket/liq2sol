//! Program tests from docs/test-plan.md section 4: the compiled SBF binary in LiteSVM, with the real Token program.
//! Build the binary first: `cargo build-sbf --manifest-path solana-escrow/Cargo.toml`.

use anchor_lang::{
    prelude::Pubkey, solana_program::instruction::Instruction, AccountDeserialize, InstructionData,
    ToAccountMetas,
};
use anchor_spl::associated_token::get_associated_token_address;
use litesvm::{types::FailedTransactionMetadata, LiteSVM};
use solana_account::Account;
use solana_escrow::{
    state::{Escrow, EscrowState, MakerVault, MAX_EXTENSION_SLOTS, RETENTION_SLOTS},
    EscrowError, ESCROW_SEED, ESCROW_VAULT_SEED, MAKER_VAULT_SEED, MAKER_VAULT_TOKEN_SEED,
    USDT_MINT,
};
use solana_keypair::Keypair;
use solana_last_restart_slot::LastRestartSlot;
use solana_message::{Message, VersionedMessage};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

const AMOUNT: u64 = 100_000_000;
const START_BALANCE: u64 = 1_000_000_000;
const START_SLOT: u64 = 1_000;
const DEADLINE: u64 = 2_000;
const fn nibble(c: u8) -> u8 {
    if c >= b'a' {
        c - b'a' + 10
    } else {
        c - b'0'
    }
}

const fn hex64(s: &str) -> [u8; 64] {
    let b = s.as_bytes();
    let mut out = [0u8; 64];
    let mut i = 0;
    while i < 64 {
        out[i] = nibble(b[i * 2]) * 16 + nibble(b[i * 2 + 1]);
        i += 1;
    }
    out
}

const fn hex32(s: &str) -> [u8; 32] {
    let b = s.as_bytes();
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = nibble(b[i * 2]) * 16 + nibble(b[i * 2 + 1]);
        i += 1;
    }
    out
}

// Test vectors computed independently of the program, by a small script using only the curve's definition.
// t = 0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20
const POINT_A: [u8; 64] = hex64("84bf7562262bbd6940085748f3be6afa52ae317155181ece31b66351ccffa4b08cc43d63b2859d469fee15f31c9edb5324266e6fd0407e87382d60fc4511acd8");
const S_A: [u8; 32] = hex32("4ce28f147e857ec4476456049bb053c134968e79e649f3a90e935310a8533273");
// t = fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210
const POINT_B: [u8; 64] = hex64("88e2ddeb04657dbd0edadf9c1f98da3b3895faa1f00527934dd35d17542ffe9b1e7640d7737e24e36d208effb77e86affe670a9a497aa7fb52bf4e687a17fff4");
const S_B: [u8; 32] = hex32("4c0b29f33d12e45ab6a60bf684540b7647c891483c9464672a83684d8fa02caf");
const PREIMAGE: [u8; 32] = S_A;
const HASH: [u8; 64] = POINT_A;

const TOKEN_FROZEN: u32 = 17;
const ANCHOR_HAS_ONE: u32 = 2001;
const ANCHOR_SEEDS: u32 = 2006;
const ANCHOR_ADDRESS: u32 = 2012;
const ANCHOR_TOKEN_OWNER: u32 = 2015;
const ANCHOR_INVALID_PROGRAM: u32 = 3008;
const ANCHOR_NOT_INITIALIZED: u32 = 3012;
const IX_DID_NOT_DESERIALIZE: u32 = 102;
const ALREADY_IN_USE: u32 = 0;

type TxResult = Result<u64, Box<FailedTransactionMetadata>>;

fn code(e: EscrowError) -> u32 {
    6000 + e as u32
}

#[track_caller]
fn assert_fails(result: TxResult, expected: u32) {
    let err = result.expect_err("transaction should fail");
    let text = format!("{:?}", err.err);
    assert!(
        text.contains(&format!("Custom({expected})")),
        "expected Custom({expected}), got {text}\n{:#?}",
        err.meta.logs
    );
}

struct Fx {
    svm: LiteSVM,
    initiator: Keypair,
    recipient: Keypair,
    other: Keypair,
}

fn token_data(mint: Pubkey, owner: Pubkey, amount: u64, frozen: bool) -> Vec<u8> {
    let mut d = vec![0u8; 165];
    d[0..32].copy_from_slice(mint.as_ref());
    d[32..64].copy_from_slice(owner.as_ref());
    d[64..72].copy_from_slice(&amount.to_le_bytes());
    d[108] = if frozen { 2 } else { 1 };
    d
}

fn mint_data() -> Vec<u8> {
    let mut d = vec![0u8; 82];
    d[36..44].copy_from_slice(&(10 * START_BALANCE).to_le_bytes());
    d[44] = 6;
    d[45] = 1;
    d
}

impl Fx {
    fn new() -> Self {
        let mut svm = LiteSVM::new();
        let bytes = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../target/deploy/solana_escrow.so"
        ));
        svm.add_program(solana_escrow::id(), bytes).unwrap();
        let mut fx = Fx {
            svm,
            initiator: Keypair::new(),
            recipient: Keypair::new(),
            other: Keypair::new(),
        };
        fx.warp(START_SLOT);
        for k in [
            fx.initiator.pubkey(),
            fx.recipient.pubkey(),
            fx.other.pubkey(),
        ] {
            fx.svm.airdrop(&k, 10_000_000_000).unwrap();
        }
        fx.put(USDT_MINT, anchor_spl::token::ID, mint_data());
        let (i, r) = (fx.initiator.pubkey(), fx.recipient.pubkey());
        fx.set_token(ata(&i), i, START_BALANCE, false);
        fx.set_token(ata(&r), r, 0, false);
        fx
    }

    fn put(&mut self, address: Pubkey, owner: Pubkey, data: Vec<u8>) {
        let lamports = self.svm.minimum_balance_for_rent_exemption(data.len());
        let account = Account {
            lamports,
            data,
            owner,
            executable: false,
            rent_epoch: 0,
        };
        self.svm.set_account(address, account).unwrap();
    }

    fn set_token(&mut self, address: Pubkey, owner: Pubkey, amount: u64, frozen: bool) {
        self.put(
            address,
            anchor_spl::token::ID,
            token_data(USDT_MINT, owner, amount, frozen),
        );
    }

    fn warp(&mut self, slot: u64) {
        self.svm.warp_to_slot(slot);
    }

    fn restart_at(&mut self, slot: u64) {
        self.svm.set_sysvar(&LastRestartSlot {
            last_restart_slot: slot,
        });
    }

    fn balance(&self, address: Pubkey) -> u64 {
        let d = self.svm.get_account(&address).unwrap().data;
        u64::from_le_bytes(d[64..72].try_into().unwrap())
    }

    fn exists(&self, address: Pubkey) -> bool {
        self.svm
            .get_account(&address)
            .is_some_and(|a| a.lamports > 0)
    }

    fn escrow(&self, address: Pubkey) -> Escrow {
        Escrow::try_deserialize(&mut &self.svm.get_account(&address).unwrap().data[..]).unwrap()
    }

    fn vault_state(&self, address: Pubkey) -> MakerVault {
        MakerVault::try_deserialize(&mut &self.svm.get_account(&address).unwrap().data[..]).unwrap()
    }

    fn send(&mut self, ix: Instruction, signers: &[&Keypair]) -> TxResult {
        self.svm.expire_blockhash();
        let message = Message::new_with_blockhash(
            &[ix],
            Some(&signers[0].pubkey()),
            &self.svm.latest_blockhash(),
        );
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(message), signers).unwrap();
        self.svm
            .send_transaction(tx)
            .map(|m| m.compute_units_consumed)
            .map_err(Box::new)
    }

    /// A short-leg style lock by the default initiator.
    fn lock_with(
        &mut self,
        hash: [u8; 64],
        recipient: Pubkey,
        amount: u64,
        deadline: u64,
        close_on_restart: bool,
    ) -> TxResult {
        let initiator = self.initiator.insecure_clone();
        let ix = lock_ix(
            &initiator,
            hash,
            recipient,
            amount,
            deadline,
            close_on_restart,
        );
        self.send(ix, &[&initiator])
    }

    fn lock(&mut self, close_on_restart: bool) -> Pubkey {
        let recipient = self.recipient.pubkey();
        self.lock_with(HASH, recipient, AMOUNT, DEADLINE, close_on_restart)
            .unwrap();
        escrow_pda(&self.initiator.pubkey(), &HASH)
    }

    fn claim_as(&mut self, payer: &Keypair, escrow: Pubkey, preimage: [u8; 32]) -> TxResult {
        let e = self.escrow(escrow);
        let ix = claim_ix(escrow, ata(&e.recipient), e.rent_payer, preimage);
        self.send(ix, &[payer])
    }

    fn claim(&mut self, escrow: Pubkey) -> TxResult {
        let payer = self.other.insecure_clone();
        self.claim_as(&payer, escrow, PREIMAGE)
    }

    fn refund(&mut self, escrow: Pubkey) -> TxResult {
        let e = self.escrow(escrow);
        let payer = self.other.insecure_clone();
        self.send(
            refund_ix(escrow, ata(&e.initiator), e.rent_payer),
            &[&payer],
        )
    }

    fn extend(&mut self, signer: &Keypair, escrow: Pubkey, new_deadline: u64) -> TxResult {
        let accounts = solana_escrow::accounts::Extend {
            recipient: signer.pubkey(),
            escrow,
        }
        .to_account_metas(None);
        let data = solana_escrow::instruction::Extend { new_deadline }.data();
        self.send(
            Instruction::new_with_bytes(solana_escrow::id(), &data, accounts),
            &[signer],
        )
    }

    fn release(&mut self, signer: &Keypair, escrow: Pubkey) -> TxResult {
        let e = self.escrow(escrow);
        let accounts = solana_escrow::accounts::Release {
            recipient: signer.pubkey(),
            escrow,
            vault: vault_pda(&escrow),
            initiator_token: ata(&e.initiator),
            rent_payer: e.rent_payer,
            token_program: anchor_spl::token::ID,
        }
        .to_account_metas(None);
        let data = solana_escrow::instruction::Release {}.data();
        self.send(
            Instruction::new_with_bytes(solana_escrow::id(), &data, accounts),
            &[signer],
        )
    }

    fn close(&mut self, signer: &Keypair, escrow: Pubkey) -> TxResult {
        let e = self.escrow(escrow);
        let accounts = solana_escrow::accounts::Close {
            closer: signer.pubkey(),
            escrow,
            rent_payer: e.rent_payer,
        }
        .to_account_metas(None);
        let data = solana_escrow::instruction::Close {}.data();
        self.send(
            Instruction::new_with_bytes(solana_escrow::id(), &data, accounts),
            &[signer],
        )
    }
}

fn ata(owner: &Pubkey) -> Pubkey {
    get_associated_token_address(owner, &USDT_MINT)
}

fn escrow_pda(initiator: &Pubkey, hash: &[u8; 64]) -> Pubkey {
    Pubkey::find_program_address(
        &[ESCROW_SEED, initiator.as_ref(), &hash[..32]],
        &solana_escrow::id(),
    )
    .0
}

fn vault_pda(escrow: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[ESCROW_VAULT_SEED, escrow.as_ref()], &solana_escrow::id()).0
}

fn lock_ix(
    initiator: &Keypair,
    payment_hash: [u8; 64],
    recipient: Pubkey,
    amount: u64,
    deadline_slot: u64,
    close_on_restart: bool,
) -> Instruction {
    let escrow = escrow_pda(&initiator.pubkey(), &payment_hash);
    let accounts = solana_escrow::accounts::Lock {
        initiator: initiator.pubkey(),
        escrow,
        vault: vault_pda(&escrow),
        initiator_token: ata(&initiator.pubkey()),
        mint: USDT_MINT,
        token_program: anchor_spl::token::ID,
        system_program: anchor_lang::system_program::ID,
    }
    .to_account_metas(None);
    let data = solana_escrow::instruction::Lock {
        lock_point: payment_hash,
        recipient,
        amount,
        deadline_slot,
        close_on_restart,
    }
    .data();
    Instruction::new_with_bytes(solana_escrow::id(), &data, accounts)
}

fn claim_ix(
    escrow: Pubkey,
    recipient_token: Pubkey,
    rent_payer: Pubkey,
    preimage: [u8; 32],
) -> Instruction {
    let accounts = solana_escrow::accounts::Claim {
        escrow,
        vault: vault_pda(&escrow),
        recipient_token,
        rent_payer,
        token_program: anchor_spl::token::ID,
    }
    .to_account_metas(None);
    let data = solana_escrow::instruction::Claim { revealed: preimage }.data();
    Instruction::new_with_bytes(solana_escrow::id(), &data, accounts)
}

fn refund_ix(escrow: Pubkey, initiator_token: Pubkey, rent_payer: Pubkey) -> Instruction {
    let accounts = solana_escrow::accounts::Refund {
        escrow,
        vault: vault_pda(&escrow),
        initiator_token,
        rent_payer,
        token_program: anchor_spl::token::ID,
    }
    .to_account_metas(None);
    let data = solana_escrow::instruction::Refund {}.data();
    Instruction::new_with_bytes(solana_escrow::id(), &data, accounts)
}

// ---- Lock ----

#[test]
fn lock_moves_funds_and_records_terms() {
    let mut fx = Fx::new();
    let escrow = fx.lock(true);
    let e = fx.escrow(escrow);
    assert_eq!(
        (e.amount, e.deadline_slot, e.created_slot),
        (AMOUNT, DEADLINE, START_SLOT)
    );
    assert_eq!(e.state, EscrowState::Open);
    assert!(e.close_on_restart && !e.extended);
    assert_eq!(e.recipient, fx.recipient.pubkey());
    assert_eq!(fx.balance(vault_pda(&escrow)), AMOUNT);
    assert_eq!(
        fx.balance(ata(&fx.initiator.pubkey())),
        START_BALANCE - AMOUNT
    );
}

#[test]
fn lock_rejects_bad_terms() {
    let mut fx = Fx::new();
    let r = fx.recipient.pubkey();
    let i = fx.initiator.pubkey();
    assert_fails(
        fx.lock_with(HASH, r, 0, DEADLINE, true),
        code(EscrowError::ZeroAmount),
    );
    assert_fails(
        fx.lock_with(HASH, r, AMOUNT, START_SLOT, true),
        code(EscrowError::DeadlinePassed),
    );
    assert_fails(
        fx.lock_with(HASH, r, AMOUNT, START_SLOT - 1, true),
        code(EscrowError::DeadlinePassed),
    );
    assert_fails(
        fx.lock_with(HASH, i, AMOUNT, DEADLINE, true),
        code(EscrowError::SelfSwap),
    );
}

#[test]
fn lock_rejects_wrong_mint_and_token_program() {
    let mut fx = Fx::new();
    let initiator = fx.initiator.insecure_clone();
    let r = fx.recipient.pubkey();

    let fake_mint = Pubkey::new_unique();
    fx.put(fake_mint, anchor_spl::token::ID, mint_data());
    // The attacker really holds the fake asset, so only the mint check can stop this.
    let fake_tokens = Pubkey::new_unique();
    fx.put(
        fake_tokens,
        anchor_spl::token::ID,
        token_data(fake_mint, initiator.pubkey(), AMOUNT, false),
    );
    let mut ix = lock_ix(&initiator, HASH, r, AMOUNT, DEADLINE, true);
    ix.accounts[3].pubkey = fake_tokens;
    ix.accounts[4].pubkey = fake_mint;
    assert_fails(fx.send(ix, &[&initiator]), ANCHOR_ADDRESS);

    // Token-2022 at the token program position.
    let mut ix = lock_ix(&initiator, HASH, r, AMOUNT, DEADLINE, true);
    ix.accounts[5].pubkey = anchor_spl::token_2022::ID;
    assert_fails(fx.send(ix, &[&initiator]), ANCHOR_INVALID_PROGRAM);
}

#[test]
fn lock_cannot_spend_someone_elses_tokens() {
    let mut fx = Fx::new();
    let attacker = fx.other.insecure_clone();
    let victim_ata = ata(&fx.initiator.pubkey());
    let mut ix = lock_ix(
        &attacker,
        HASH,
        fx.recipient.pubkey(),
        AMOUNT,
        DEADLINE,
        true,
    );
    ix.accounts[3].pubkey = victim_ata;
    assert_fails(fx.send(ix, &[&attacker]), ANCHOR_TOKEN_OWNER);
}

#[test]
fn lock_twice_with_the_same_hash_is_rejected_but_another_initiator_may_use_it() {
    let mut fx = Fx::new();
    fx.lock(true);
    let r = fx.recipient.pubkey();
    assert_fails(
        fx.lock_with(HASH, r, AMOUNT, DEADLINE, true),
        ALREADY_IN_USE,
    );

    let second = fx.other.insecure_clone();
    fx.set_token(ata(&second.pubkey()), second.pubkey(), AMOUNT, false);
    let ix = lock_ix(&second, HASH, r, AMOUNT, DEADLINE, true);
    fx.send(ix, &[&second]).unwrap();
    assert!(fx.exists(escrow_pda(&second.pubkey(), &HASH)));
}

// ---- Claim ----

#[test]
fn third_party_claim_pays_the_recipient_and_keeps_the_preimage_readable() {
    let mut fx = Fx::new();
    let escrow = fx.lock(true);
    let initiator_lamports = fx.svm.get_account(&fx.initiator.pubkey()).unwrap().lamports;
    let cu = fx.claim(escrow).unwrap();
    println!("claim used {cu} compute units");
    assert_eq!(fx.balance(ata(&fx.recipient.pubkey())), AMOUNT);
    assert!(!fx.exists(vault_pda(&escrow)), "vault closed");
    let e = fx.escrow(escrow);
    assert_eq!(e.state, EscrowState::Claimed);
    assert_eq!(e.revealed, PREIMAGE);
    assert_eq!(e.settled_slot, START_SLOT);
    assert!(
        fx.svm.get_account(&fx.initiator.pubkey()).unwrap().lamports > initiator_lamports,
        "vault rent returned"
    );
}

#[test]
fn wrong_preimage_is_rejected() {
    let mut fx = Fx::new();
    let escrow = fx.lock(true);
    let payer = fx.other.insecure_clone();
    assert_fails(
        fx.claim_as(&payer, escrow, [1; 32]),
        code(EscrowError::WrongSecret),
    );
}

#[test]
fn short_preimage_is_rejected_and_trailing_bytes_cannot_change_what_is_stored() {
    let mut fx = Fx::new();
    let escrow = fx.lock(true);
    let payer = fx.other.insecure_clone();
    let rent_payer = fx.initiator.pubkey();
    let base = claim_ix(escrow, ata(&fx.recipient.pubkey()), rent_payer, PREIMAGE);

    let mut short = base.clone();
    short.data.pop();
    assert_fails(fx.send(short, &[&payer]), IX_DID_NOT_DESERIALIZE);

    // Anchor ignores trailing bytes. They are outside the hashed value, so the stored preimage is still exact.
    let mut long = base;
    long.data.push(0xff);
    fx.send(long, &[&payer]).unwrap();
    assert_eq!(fx.escrow(escrow).revealed, PREIMAGE);
}

#[test]
fn claim_boundary_at_the_deadline() {
    let mut fx = Fx::new();
    let escrow = fx.lock(false);
    fx.warp(DEADLINE - 1);
    fx.claim(escrow).unwrap();

    let mut fx = Fx::new();
    let escrow = fx.lock(false);
    fx.warp(DEADLINE);
    assert_fails(fx.claim(escrow), code(EscrowError::ClaimClosed));
}

#[test]
fn claim_only_pays_the_recipients_canonical_account() {
    let mut fx = Fx::new();
    let escrow = fx.lock(true);
    let (payer, recipient, thief) = (
        fx.other.insecure_clone(),
        fx.recipient.pubkey(),
        fx.other.pubkey(),
    );

    // The thief's own account.
    fx.set_token(ata(&thief), thief, 0, false);
    let ix = claim_ix(escrow, ata(&thief), fx.initiator.pubkey(), PREIMAGE);
    assert_fails(
        fx.send(ix, &[&payer]),
        code(EscrowError::WrongRecipientAccount),
    );

    // Owned by the recipient, but not its canonical account.
    let other_account = Pubkey::new_unique();
    fx.set_token(other_account, recipient, 0, false);
    let ix = claim_ix(escrow, other_account, fx.initiator.pubkey(), PREIMAGE);
    assert_fails(
        fx.send(ix, &[&payer]),
        code(EscrowError::WrongRecipientAccount),
    );

    // The canonical address, reassigned to someone else.
    fx.set_token(ata(&recipient), thief, 0, false);
    let ix = claim_ix(escrow, ata(&recipient), fx.initiator.pubkey(), PREIMAGE);
    assert_fails(
        fx.send(ix, &[&payer]),
        code(EscrowError::WrongRecipientAccount),
    );
}

#[test]
fn claim_rejects_another_escrows_vault_and_wrong_rent_payer() {
    let mut fx = Fx::new();
    let escrow = fx.lock(true);
    let payer = fx.other.insecure_clone();
    let mut ix = claim_ix(
        escrow,
        ata(&fx.recipient.pubkey()),
        fx.initiator.pubkey(),
        PREIMAGE,
    );
    ix.accounts[1].pubkey = ata(&fx.initiator.pubkey());
    assert_fails(fx.send(ix, &[&payer]), ANCHOR_SEEDS);

    let ix = claim_ix(
        escrow,
        ata(&fx.recipient.pubkey()),
        fx.other.pubkey(),
        PREIMAGE,
    );
    assert_fails(fx.send(ix, &[&payer]), ANCHOR_ADDRESS);
}

#[test]
fn settled_escrows_cannot_settle_again() {
    let mut fx = Fx::new();
    let escrow = fx.lock(true);
    let vault = vault_pda(&escrow);
    fx.claim(escrow).unwrap();
    // The vault is gone, so every later settlement fails before it can move anything.
    assert!(!fx.exists(vault));
    assert_fails(fx.claim(escrow), ANCHOR_NOT_INITIALIZED);
    fx.warp(DEADLINE);
    assert_fails(fx.refund(escrow), ANCHOR_NOT_INITIALIZED);
    let recipient = fx.recipient.insecure_clone();
    assert_fails(fx.release(&recipient, escrow), ANCHOR_NOT_INITIALIZED);
    assert_eq!(fx.balance(ata(&fx.recipient.pubkey())), AMOUNT);

    let mut fx = Fx::new();
    let escrow = fx.lock(true);
    fx.warp(DEADLINE);
    fx.refund(escrow).unwrap();
    assert_fails(fx.refund(escrow), ANCHOR_NOT_INITIALIZED);
    assert_fails(fx.claim(escrow), ANCHOR_NOT_INITIALIZED);
    assert_eq!(fx.balance(ata(&fx.initiator.pubkey())), START_BALANCE);
}

// ---- Refund ----

#[test]
fn refund_boundary_at_the_deadline_and_anyone_can_send_it() {
    let mut fx = Fx::new();
    let escrow = fx.lock(false);
    fx.warp(DEADLINE - 1);
    assert_fails(fx.refund(escrow), code(EscrowError::RefundNotOpen));
    fx.warp(DEADLINE);
    fx.refund(escrow).unwrap();
    assert_eq!(fx.balance(ata(&fx.initiator.pubkey())), START_BALANCE);
    let e = fx.escrow(escrow);
    assert_eq!(e.state, EscrowState::Refunded);
    assert_eq!(e.revealed, [0; 32]);
    assert_eq!(e.settled_slot, DEADLINE);
}

#[test]
fn refund_cannot_go_to_another_account() {
    let mut fx = Fx::new();
    let escrow = fx.lock(false);
    fx.warp(DEADLINE);
    let (payer, thief) = (fx.other.insecure_clone(), fx.other.pubkey());
    fx.set_token(ata(&thief), thief, 0, false);
    let ix = refund_ix(escrow, ata(&thief), fx.initiator.pubkey());
    assert_fails(
        fx.send(ix, &[&payer]),
        code(EscrowError::WrongInitiatorAccount),
    );
}

// ---- Restart ----

#[test]
fn restart_after_creation_closes_a_short_leg_early() {
    let mut fx = Fx::new();
    let escrow = fx.lock(true);
    fx.warp(START_SLOT + 10);
    fx.restart_at(START_SLOT + 5);
    assert_fails(fx.claim(escrow), code(EscrowError::ClaimClosed));
    fx.refund(escrow).unwrap();
    assert_eq!(fx.balance(ata(&fx.initiator.pubkey())), START_BALANCE);
}

#[test]
fn restart_before_or_at_creation_has_no_effect() {
    for restart in [START_SLOT - 1, START_SLOT] {
        let mut fx = Fx::new();
        fx.restart_at(restart);
        let escrow = fx.lock(true);
        assert_fails(fx.refund(escrow), code(EscrowError::RefundNotOpen));
        fx.claim(escrow).unwrap();
    }
}

#[test]
fn restart_does_not_touch_a_long_leg() {
    let mut fx = Fx::new();
    let escrow = fx.lock(false);
    fx.warp(START_SLOT + 10);
    fx.restart_at(START_SLOT + 5);
    assert_fails(fx.refund(escrow), code(EscrowError::RefundNotOpen));
    fx.claim(escrow).unwrap();
}

// ---- Freezes ----

#[test]
fn frozen_accounts_fail_cleanly_and_work_after_a_thaw() {
    let mut fx = Fx::new();
    let escrow = fx.lock(true);
    let r = fx.recipient.pubkey();
    fx.set_token(ata(&r), r, 0, true);
    assert_fails(fx.claim(escrow), TOKEN_FROZEN);
    assert_eq!(fx.escrow(escrow).state, EscrowState::Open);
    assert_eq!(
        fx.escrow(escrow).revealed,
        [0; 32],
        "a failed claim leaves no state"
    );
    fx.set_token(ata(&r), r, 0, false);
    fx.claim(escrow).unwrap();

    let mut fx = Fx::new();
    let escrow = fx.lock(false);
    fx.warp(DEADLINE);
    let i = fx.initiator.pubkey();
    fx.set_token(ata(&i), i, START_BALANCE - AMOUNT, true);
    assert_fails(fx.refund(escrow), TOKEN_FROZEN);
    assert_eq!(fx.escrow(escrow).state, EscrowState::Open);
    fx.set_token(ata(&i), i, START_BALANCE - AMOUNT, false);
    fx.refund(escrow).unwrap();
}

#[test]
fn frozen_vault_blocks_both_paths_until_thawed() {
    let mut fx = Fx::new();
    let escrow = fx.lock(false);
    let vault = vault_pda(&escrow);
    fx.set_token(vault, escrow, AMOUNT, true);
    assert_fails(fx.claim(escrow), TOKEN_FROZEN);
    fx.warp(DEADLINE);
    assert_fails(fx.refund(escrow), TOKEN_FROZEN);
    assert_eq!(fx.escrow(escrow).state, EscrowState::Open);
    fx.set_token(vault, escrow, AMOUNT, false);
    fx.refund(escrow).unwrap();
}

// ---- Extend and release ----

#[test]
fn recipient_extends_once_and_the_new_deadline_governs() {
    let mut fx = Fx::new();
    let escrow = fx.lock(false);
    let recipient = fx.recipient.insecure_clone();
    fx.extend(&recipient, escrow, DEADLINE + MAX_EXTENSION_SLOTS)
        .unwrap();
    assert!(fx.escrow(escrow).extended);
    assert_fails(
        fx.extend(&recipient, escrow, DEADLINE + MAX_EXTENSION_SLOTS + 1),
        code(EscrowError::ExtensionNotAllowed),
    );
    fx.warp(DEADLINE);
    assert_fails(fx.refund(escrow), code(EscrowError::RefundNotOpen));
    fx.claim(escrow).unwrap();
}

#[test]
fn extend_rejects_everything_else() {
    let mut fx = Fx::new();
    let escrow = fx.lock(false);
    let (recipient, initiator) = (fx.recipient.insecure_clone(), fx.initiator.insecure_clone());
    assert_fails(fx.extend(&initiator, escrow, DEADLINE + 1), ANCHOR_HAS_ONE);
    assert_fails(
        fx.extend(&recipient, escrow, DEADLINE),
        code(EscrowError::ExtensionNotAllowed),
    );
    assert_fails(
        fx.extend(&recipient, escrow, DEADLINE - 1),
        code(EscrowError::ExtensionNotAllowed),
    );
    assert_fails(
        fx.extend(&recipient, escrow, DEADLINE + MAX_EXTENSION_SLOTS + 1),
        code(EscrowError::ExtensionNotAllowed),
    );
    fx.warp(DEADLINE);
    assert_fails(
        fx.extend(&recipient, escrow, DEADLINE + 1),
        code(EscrowError::ExtensionNotAllowed),
    );

    let mut fx = Fx::new();
    let short = fx.lock(true);
    let recipient = fx.recipient.insecure_clone();
    assert_fails(
        fx.extend(&recipient, short, DEADLINE + 1),
        code(EscrowError::ExtensionNotAllowed),
    );
}

#[test]
fn release_is_recipient_only_and_returns_funds_at_once() {
    let mut fx = Fx::new();
    let escrow = fx.lock(true);
    let (recipient, initiator) = (fx.recipient.insecure_clone(), fx.initiator.insecure_clone());
    assert_fails(fx.release(&initiator, escrow), ANCHOR_HAS_ONE);
    fx.release(&recipient, escrow).unwrap();
    assert_eq!(fx.balance(ata(&fx.initiator.pubkey())), START_BALANCE);
    assert_eq!(fx.escrow(escrow).state, EscrowState::Refunded);
}

// ---- Close ----

#[test]
fn close_rules() {
    let mut fx = Fx::new();
    let escrow = fx.lock(true);
    let (initiator, stranger) = (fx.initiator.insecure_clone(), fx.other.insecure_clone());
    assert_fails(
        fx.close(&initiator, escrow),
        code(EscrowError::CloseNotAllowed),
    );
    fx.claim(escrow).unwrap();
    assert_fails(
        fx.close(&stranger, escrow),
        code(EscrowError::CloseNotAllowed),
    );
    fx.warp(START_SLOT + RETENTION_SLOTS - 1);
    assert_fails(
        fx.close(&stranger, escrow),
        code(EscrowError::CloseNotAllowed),
    );
    assert_eq!(
        fx.escrow(escrow).revealed,
        PREIMAGE,
        "still readable during retention"
    );
    fx.warp(START_SLOT + RETENTION_SLOTS);
    fx.close(&stranger, escrow).unwrap();
    assert!(!fx.exists(escrow));

    let mut fx = Fx::new();
    let escrow = fx.lock(true);
    let initiator = fx.initiator.insecure_clone();
    fx.claim(escrow).unwrap();
    fx.close(&initiator, escrow).unwrap();
    assert!(!fx.exists(escrow));
}

// ---- Maker vault ----

struct VaultFx {
    fx: Fx,
    owner: Keypair,
    operator: Keypair,
    maker_vault: Pubkey,
    vault_token: Pubkey,
}

const WINDOW: u64 = 500;
const CAP: u64 = 2 * AMOUNT;

fn vault_fx() -> VaultFx {
    let mut fx = Fx::new();
    let (owner, operator) = (Keypair::new(), Keypair::new());
    fx.svm.airdrop(&owner.pubkey(), 10_000_000_000).unwrap();
    fx.svm.airdrop(&operator.pubkey(), 10_000_000_000).unwrap();
    let maker_vault = Pubkey::find_program_address(
        &[MAKER_VAULT_SEED, owner.pubkey().as_ref()],
        &solana_escrow::id(),
    )
    .0;
    let vault_token = Pubkey::find_program_address(
        &[MAKER_VAULT_TOKEN_SEED, maker_vault.as_ref()],
        &solana_escrow::id(),
    )
    .0;
    let accounts = solana_escrow::accounts::VaultInit {
        owner: owner.pubkey(),
        maker_vault,
        vault_token,
        mint: USDT_MINT,
        token_program: anchor_spl::token::ID,
        system_program: anchor_lang::system_program::ID,
    }
    .to_account_metas(None);
    let data = solana_escrow::instruction::VaultInit {
        operator: operator.pubkey(),
        window_slots: WINDOW,
        window_cap: CAP,
    }
    .data();
    fx.send(
        Instruction::new_with_bytes(solana_escrow::id(), &data, accounts),
        &[&owner],
    )
    .unwrap();

    let initiator = fx.initiator.insecure_clone();
    let accounts = solana_escrow::accounts::VaultDeposit {
        depositor: initiator.pubkey(),
        maker_vault,
        vault_token,
        from: ata(&initiator.pubkey()),
        token_program: anchor_spl::token::ID,
    }
    .to_account_metas(None);
    let data = solana_escrow::instruction::VaultDeposit {
        amount: START_BALANCE,
    }
    .data();
    fx.send(
        Instruction::new_with_bytes(solana_escrow::id(), &data, accounts),
        &[&initiator],
    )
    .unwrap();
    VaultFx {
        fx,
        owner,
        operator,
        maker_vault,
        vault_token,
    }
}

impl VaultFx {
    fn lock(&mut self, signer: &Keypair, hash: [u8; 64], amount: u64) -> (Pubkey, TxResult) {
        let escrow = escrow_pda(&self.maker_vault, &hash);
        let accounts = solana_escrow::accounts::LockFromVault {
            operator: signer.pubkey(),
            maker_vault: self.maker_vault,
            vault_token: self.vault_token,
            escrow,
            vault: vault_pda(&escrow),
            mint: USDT_MINT,
            token_program: anchor_spl::token::ID,
            system_program: anchor_lang::system_program::ID,
        }
        .to_account_metas(None);
        let data = solana_escrow::instruction::LockFromVault {
            lock_point: hash,
            recipient: self.fx.recipient.pubkey(),
            amount,
            deadline_slot: DEADLINE,
            close_on_restart: true,
        }
        .data();
        let result = self.fx.send(
            Instruction::new_with_bytes(solana_escrow::id(), &data, accounts),
            &[signer],
        );
        (escrow, result)
    }

    fn withdraw(&mut self, signer: &Keypair, to: Pubkey, amount: u64) -> TxResult {
        // The vault's PDA is derived from the real owner, so a wrong signer fails on seeds or has_one.
        let accounts = solana_escrow::accounts::VaultWithdraw {
            owner: signer.pubkey(),
            maker_vault: self.maker_vault,
            vault_token: self.vault_token,
            to,
            token_program: anchor_spl::token::ID,
        }
        .to_account_metas(None);
        let data = solana_escrow::instruction::VaultWithdraw { amount }.data();
        self.fx.send(
            Instruction::new_with_bytes(solana_escrow::id(), &data, accounts),
            &[signer],
        )
    }

    fn set_operator(&mut self, signer: &Keypair, operator: Pubkey) -> TxResult {
        let accounts = solana_escrow::accounts::VaultSetOperator {
            owner: signer.pubkey(),
            maker_vault: self.maker_vault,
        }
        .to_account_metas(None);
        let data = solana_escrow::instruction::VaultSetOperator {
            operator,
            window_slots: WINDOW,
            window_cap: CAP,
        }
        .data();
        self.fx.send(
            Instruction::new_with_bytes(solana_escrow::id(), &data, accounts),
            &[signer],
        )
    }
}

#[test]
fn vault_lock_claim_and_refund_flow() {
    let mut v = vault_fx();
    let operator = v.operator.insecure_clone();
    let (escrow, result) = v.lock(&operator, HASH, AMOUNT);
    result.unwrap();
    assert_eq!(v.fx.escrow(escrow).initiator, v.maker_vault);
    assert_eq!(v.fx.escrow(escrow).rent_payer, operator.pubkey());
    assert_eq!(v.fx.balance(v.vault_token), START_BALANCE - AMOUNT);
    v.fx.claim(escrow).unwrap();
    assert_eq!(v.fx.balance(ata(&v.fx.recipient.pubkey())), AMOUNT);

    // A refund returns to the vault's own token account, which is owned by the vault PDA.
    let mut v = vault_fx();
    let operator = v.operator.insecure_clone();
    let (escrow, result) = v.lock(&operator, HASH, AMOUNT);
    result.unwrap();
    v.fx.warp(DEADLINE);
    let payer = v.fx.other.insecure_clone();
    let ix = refund_ix(escrow, v.vault_token, operator.pubkey());
    v.fx.send(ix, &[&payer]).unwrap();
    assert_eq!(v.fx.balance(v.vault_token), START_BALANCE);
}

#[test]
fn window_cap_limits_a_stolen_hot_key() {
    let mut v = vault_fx();
    let operator = v.operator.insecure_clone();
    v.lock(&operator, [1; 64], AMOUNT).1.unwrap();
    v.lock(&operator, [2; 64], AMOUNT).1.unwrap();
    assert_fails(
        v.lock(&operator, [3; 64], 1).1,
        code(EscrowError::OverWindowCap),
    );
    assert_eq!(v.fx.balance(v.vault_token), START_BALANCE - CAP);
    let used = v.fx.vault_state(v.maker_vault);
    assert_eq!(used.window_used, CAP);

    v.fx.warp(START_SLOT + WINDOW);
    v.lock(&operator, [3; 64], AMOUNT).1.unwrap();
    assert_fails(
        v.lock(&operator, [4; 64], AMOUNT + 1).1,
        code(EscrowError::OverWindowCap),
    );
}

#[test]
fn hot_key_cannot_withdraw_or_rotate_and_old_key_stops_working() {
    let mut v = vault_fx();
    let (owner, operator, stranger) = (
        v.owner.insecure_clone(),
        v.operator.insecure_clone(),
        v.fx.other.insecure_clone(),
    );
    let thief_token = ata(&operator.pubkey());
    v.fx.set_token(thief_token, operator.pubkey(), 0, false);

    // The vault's address derives from the owner, so a wrong signer fails on the seeds.
    assert_fails(v.withdraw(&operator, thief_token, 1), ANCHOR_SEEDS);
    assert_fails(v.set_operator(&operator, operator.pubkey()), ANCHOR_SEEDS);
    assert_fails(v.lock(&stranger, [1; 64], 1).1, ANCHOR_HAS_ONE);

    let new_operator = Keypair::new();
    v.fx.svm
        .airdrop(&new_operator.pubkey(), 10_000_000_000)
        .unwrap();
    v.set_operator(&owner, new_operator.pubkey()).unwrap();
    assert_fails(v.lock(&operator, [1; 64], 1).1, ANCHOR_HAS_ONE);
    v.lock(&new_operator, [1; 64], 1).1.unwrap();

    let owner_token = ata(&owner.pubkey());
    v.fx.set_token(owner_token, owner.pubkey(), 0, false);
    v.withdraw(&owner, owner_token, 5).unwrap();
    assert_eq!(v.fx.balance(owner_token), 5);
}

// ---- Randomized invariants ----

/// Small deterministic generator, so a failure reproduces from its seed.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// S1, S2, S4, S5, S6: random operations in random order, with funds conserved and settlement final.
#[test]
fn invariants_hold_under_random_operations() {
    for seed in 1..=12u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut fx = Fx::new();
        let close_on_restart = rng.next().is_multiple_of(2);
        let escrow = fx.lock(close_on_restart);
        let vault = vault_pda(&escrow);
        let (i, r) = (ata(&fx.initiator.pubkey()), ata(&fx.recipient.pubkey()));
        let (recipient, stranger) = (fx.recipient.insecure_clone(), fx.other.insecure_clone());
        let mut slot = START_SLOT;
        let mut settled: Option<EscrowState> = None;

        for _ in 0..40 {
            let before = fx.escrow(escrow);
            let last_restart: u64 = fx.svm.get_sysvar::<LastRestartSlot>().last_restart_slot;
            let (could_claim, could_refund) = (
                before.can_claim(slot, last_restart),
                before.can_refund(slot, last_restart),
            );
            assert!(
                settled.is_some() || could_claim != could_refund,
                "S4 seed {seed}"
            );

            let op = rng.next() % 8;
            let result = match op {
                0 => {
                    slot += rng.next() % 400 + 1;
                    fx.warp(slot);
                    continue;
                }
                1 => {
                    fx.restart_at(slot.saturating_sub(rng.next() % 100));
                    continue;
                }
                2 => fx.claim(escrow),
                3 => fx.claim_as(&stranger, escrow, [9; 32]),
                4 => fx.refund(escrow),
                5 => fx.extend(&recipient, escrow, before.deadline_slot + rng.next() % 1000),
                6 => fx.extend(&stranger, escrow, before.deadline_slot + 1),
                _ => {
                    if rng.next().is_multiple_of(6) {
                        fx.release(&recipient, escrow)
                    } else {
                        fx.release(&stranger, escrow)
                    }
                }
            };

            if op == 2 && settled.is_none() {
                assert_eq!(
                    result.is_ok(),
                    could_claim,
                    "claim must follow can_claim, seed {seed}"
                );
            }
            if op == 4 && settled.is_none() {
                assert_eq!(
                    result.is_ok(),
                    could_refund,
                    "refund must follow can_refund, seed {seed}"
                );
            }

            let after = fx.escrow(escrow);
            if let Some(state) = settled {
                assert_eq!(after.state, state, "S5: settlement is final, seed {seed}");
                assert!(
                    result.is_err(),
                    "nothing succeeds after settlement, seed {seed}"
                );
            } else if after.state != EscrowState::Open {
                settled = Some(after.state);
            }

            // Funds are conserved and only ever in the three expected places.
            let vault_balance = if fx.exists(vault) {
                fx.balance(vault)
            } else {
                0
            };
            assert_eq!(
                fx.balance(i) + fx.balance(r) + vault_balance,
                START_BALANCE,
                "S2, seed {seed}"
            );
            match after.state {
                EscrowState::Open => assert_eq!(vault_balance, AMOUNT, "S1, seed {seed}"),
                EscrowState::Claimed => {
                    assert_eq!(fx.balance(r), AMOUNT, "seed {seed}");
                    assert_eq!(after.revealed, PREIMAGE, "S6, seed {seed}");
                    assert!(!fx.exists(vault));
                }
                EscrowState::Refunded => {
                    assert_eq!(fx.balance(i), START_BALANCE, "seed {seed}");
                    assert!(!fx.exists(vault));
                }
            }
        }
    }
}

// ---- Refund destination ----

/// Anyone can create a token account owned by the vault PDA, but nothing can withdraw from it, so a refund sent there
/// would be stuck for good.
#[test]
fn griefer_cannot_strand_a_vault_refund_in_a_look_alike_account() {
    let mut v = vault_fx();
    let operator = v.operator.insecure_clone();
    let (escrow, result) = v.lock(&operator, HASH, AMOUNT);
    result.unwrap();
    v.fx.warp(DEADLINE);
    let look_alike = Pubkey::new_unique();
    v.fx.set_token(look_alike, v.maker_vault, 0, false);
    let griefer = v.fx.other.insecure_clone();
    let ix = refund_ix(escrow, look_alike, operator.pubkey());
    assert!(
        v.fx.send(ix, &[&griefer]).is_err(),
        "refund must not reach a look-alike account"
    );
    assert_eq!(v.fx.balance(look_alike), 0);

    let ix = refund_ix(escrow, v.vault_token, operator.pubkey());
    v.fx.send(ix, &[&griefer]).unwrap();
    assert_eq!(v.fx.balance(v.vault_token), START_BALANCE);
}

#[test]
fn recipient_cannot_strand_a_vault_release_in_a_look_alike_account() {
    let mut v = vault_fx();
    let operator = v.operator.insecure_clone();
    let (escrow, result) = v.lock(&operator, HASH, AMOUNT);
    result.unwrap();
    let look_alike = Pubkey::new_unique();
    v.fx.set_token(look_alike, v.maker_vault, 0, false);
    let recipient = v.fx.recipient.insecure_clone();
    let accounts = solana_escrow::accounts::Release {
        recipient: recipient.pubkey(),
        escrow,
        vault: vault_pda(&escrow),
        initiator_token: look_alike,
        rent_payer: operator.pubkey(),
        token_program: anchor_spl::token::ID,
    }
    .to_account_metas(None);
    let data = solana_escrow::instruction::Release {}.data();
    let ix = Instruction::new_with_bytes(solana_escrow::id(), &data, accounts);
    assert!(v.fx.send(ix, &[&recipient]).is_err());
}

/// A wallet's refund goes to its canonical account, which can always be recreated if it was closed.
#[test]
fn wallet_refund_only_goes_to_the_canonical_account() {
    let mut fx = Fx::new();
    let escrow = fx.lock(false);
    fx.warp(DEADLINE);
    let i = fx.initiator.pubkey();
    let second = Pubkey::new_unique();
    fx.set_token(second, i, 0, false);
    let payer = fx.other.insecure_clone();
    let ix = refund_ix(escrow, second, i);
    assert!(fx.send(ix, &[&payer]).is_err());
    fx.refund(escrow).unwrap();
}

/// The counterparty computes the secret from this field, so it must be stored exactly. Both vectors are checked, with values
/// that aren't all zeros, so a missing write can't pass.
#[test]
fn claim_stores_the_exact_revealed_value() {
    for (point, s) in [(POINT_A, S_A), (POINT_B, S_B)] {
        let mut fx = Fx::new();
        let recipient = fx.recipient.pubkey();
        fx.lock_with(point, recipient, AMOUNT, DEADLINE, true)
            .unwrap();
        let escrow = escrow_pda(&fx.initiator.pubkey(), &point);
        assert_eq!(
            fx.escrow(escrow).revealed,
            [0; 32],
            "nothing to read before the claim"
        );
        let payer = fx.other.insecure_clone();
        fx.claim_as(&payer, escrow, s).unwrap();
        assert_eq!(fx.escrow(escrow).revealed, s);
        assert_eq!(fx.balance(ata(&recipient)), AMOUNT);
    }
}

/// `n - a`, big-endian: how a scalar or a y coordinate is negated.
fn negate(modulus: &[u8; 32], a: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut borrow = 0i16;
    for i in (0..32).rev() {
        let d = i16::from(modulus[i]) - i16::from(a[i]) - borrow;
        borrow = i16::from(d < 0);
        out[i] = d.rem_euclid(256) as u8;
    }
    out
}

const N: [u8; 32] = hex32("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141");
const P: [u8; 32] = hex32("fffffffffffffffffffffffffffffffffffffffffffffffffffffffefffffc2f");

/// The point check itself, through the real syscall.
#[test]
fn only_the_right_value_opens_the_point() {
    const N_MINUS_1: [u8; 32] =
        hex32("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364140");
    let wrong: [([u8; 32], &str); 5] = [
        ([0; 32], "zero"),
        (N, "the group order, out of range"),
        ([0xff; 32], "above the group order"),
        (S_B, "another point's value"),
        (N_MINUS_1, "a valid scalar for another point"),
    ];
    for (s, what) in wrong {
        let mut fx = Fx::new();
        let escrow = fx.lock(true);
        let payer = fx.other.insecure_clone();
        assert_fails(
            fx.claim_as(&payer, escrow, s),
            code(EscrowError::WrongSecret),
        );
        assert_eq!(fx.escrow(escrow).state, EscrowState::Open, "{what}");
    }
}

/// `-s` opens `-T` and not `T`. It also covers a high `s`, because one of `s` and `-s` is always high.
#[test]
fn the_sign_of_the_secret_matters() {
    let negative_s = negate(&N, &S_A);
    let mut negative_point = POINT_A;
    negative_point[32..].copy_from_slice(&negate(&P, &POINT_A[32..]));

    let mut fx = Fx::new();
    let escrow = fx.lock(true);
    let payer = fx.other.insecure_clone();
    assert_fails(
        fx.claim_as(&payer, escrow, negative_s),
        code(EscrowError::WrongSecret),
    );

    for (point, s) in [(POINT_A, S_A), (negative_point, negative_s)] {
        let mut fx = Fx::new();
        let recipient = fx.recipient.pubkey();
        fx.lock_with(point, recipient, AMOUNT, DEADLINE, true)
            .unwrap();
        let escrow = escrow_pda(&fx.initiator.pubkey(), &point);
        let payer = fx.other.insecure_clone();
        fx.claim_as(&payer, escrow, s).unwrap();
    }
}

/// An escrow locked to something that isn't a point can never be claimed, and still refunds.
#[test]
fn a_garbage_point_can_only_refund() {
    let mut fx = Fx::new();
    let recipient = fx.recipient.pubkey();
    let garbage = [7u8; 64];
    fx.lock_with(garbage, recipient, AMOUNT, DEADLINE, false)
        .unwrap();
    let escrow = escrow_pda(&fx.initiator.pubkey(), &garbage);
    let payer = fx.other.insecure_clone();
    assert_fails(
        fx.claim_as(&payer, escrow, S_A),
        code(EscrowError::WrongSecret),
    );
    fx.warp(DEADLINE);
    fx.refund(escrow).unwrap();
    assert_eq!(fx.balance(ata(&fx.initiator.pubkey())), START_BALANCE);
}
