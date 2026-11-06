//! Against the built program: \`cargo build-sbf --manifest-path devnet-faucet/Cargo.toml && cargo test\`.
use anchor_lang::{AccountDeserialize, InstructionData, ToAccountMetas};
use anchor_spl::associated_token::get_associated_token_address as ata;
use anchor_spl::token::TokenAccount;
use devnet_faucet::{accounts, instruction, CLAIM_SEED, DRIP_UNITS, MINT, VAULT_SEED};
use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use litesvm::LiteSVM;
use solana_account::Account;
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

fn mint_data() -> Vec<u8> {
    let mut d = vec![0u8; 82];
    d[36..44].copy_from_slice(&u64::MAX.to_le_bytes());
    d[44] = 6;
    d[45] = 1;
    d
}

fn token_data(mint: Pubkey, owner: Pubkey, amount: u64) -> Vec<u8> {
    let mut d = vec![0u8; 165];
    d[0..32].copy_from_slice(mint.as_ref());
    d[32..64].copy_from_slice(owner.as_ref());
    d[64..72].copy_from_slice(&amount.to_le_bytes());
    d[108] = 1;
    d
}

struct Fx {
    svm: LiteSVM,
    payer: Keypair,
}

impl Fx {
    fn new() -> Self {
        let mut svm = LiteSVM::new();
        svm.add_program(devnet_faucet::id(), include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../target/deploy/devnet_faucet.so"))).unwrap();
        let payer = Keypair::new();
        svm.airdrop(&payer.pubkey(), 10_000_000_000).unwrap();
        let mut fx = Fx { svm, payer };
        fx.put(MINT, anchor_spl::token::ID, mint_data());
        fx
    }

    fn put(&mut self, address: Pubkey, owner: Pubkey, data: Vec<u8>) {
        let lamports = self.svm.minimum_balance_for_rent_exemption(data.len());
        self.svm.set_account(address, Account { lamports, data, owner, executable: false, rent_epoch: 0 }).unwrap();
    }

    fn send(&mut self, ix: Instruction, signer: &Keypair) -> std::result::Result<(), String> {
        self.svm.expire_blockhash();
        let message = Message::new_with_blockhash(&[ix], Some(&signer.pubkey()), &self.svm.latest_blockhash());
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[signer]).unwrap();
        self.svm.send_transaction(tx).map(|_| ()).map_err(|e| format!("{:?}", e.err))
    }

    fn vault(&self) -> Pubkey {
        Pubkey::find_program_address(&[VAULT_SEED], &devnet_faucet::id()).0
    }

    fn init(&mut self, amount: u64) {
        let ix = Instruction {
            program_id: devnet_faucet::id(),
            accounts: accounts::Init {
                payer: self.payer.pubkey(),
                mint: MINT,
                vault: self.vault(),
                token_program: anchor_spl::token::ID,
                system_program: anchor_lang::system_program::ID,
            }
            .to_account_metas(None),
            data: instruction::Init {}.data(),
        };
        let payer = self.payer.insecure_clone();
        self.send(ix, &payer).unwrap();
        let v = self.vault();
        self.put(v, anchor_spl::token::ID, token_data(MINT, v, amount));
    }

    fn wallet(&mut self) -> Keypair {
        let w = Keypair::new();
        self.svm.airdrop(&w.pubkey(), 1_000_000_000).unwrap();
        self.put(ata(&w.pubkey(), &MINT), anchor_spl::token::ID, token_data(MINT, w.pubkey(), 0));
        w
    }

    fn drip_ix(&self, claimant: &Pubkey, to: Pubkey) -> Instruction {
        Instruction {
            program_id: devnet_faucet::id(),
            accounts: accounts::Drip {
                claimant: *claimant,
                claim: Pubkey::find_program_address(&[CLAIM_SEED, claimant.as_ref()], &devnet_faucet::id()).0,
                vault: self.vault(),
                to,
                token_program: anchor_spl::token::ID,
                system_program: anchor_lang::system_program::ID,
            }
            .to_account_metas(None),
            data: instruction::Drip {}.data(),
        }
    }

    fn drip(&mut self, w: &Keypair) -> std::result::Result<(), String> {
        let ix = self.drip_ix(&w.pubkey(), ata(&w.pubkey(), &MINT));
        self.send(ix, w)
    }

    fn balance(&self, address: Pubkey) -> u64 {
        let a = self.svm.get_account(&address).unwrap();
        TokenAccount::try_deserialize(&mut a.data.as_slice()).unwrap().amount
    }
}

#[test]
fn a_wallet_gets_its_tokens_once() {
    let mut fx = Fx::new();
    fx.init(3 * DRIP_UNITS);
    let w = fx.wallet();
    fx.drip(&w).unwrap();
    assert_eq!(fx.balance(ata(&w.pubkey(), &MINT)), DRIP_UNITS);
    assert_eq!(fx.balance(fx.vault()), 2 * DRIP_UNITS);
    assert!(fx.drip(&w).is_err(), "a second claim");
    assert_eq!(fx.balance(fx.vault()), 2 * DRIP_UNITS);
    let other = fx.wallet();
    fx.drip(&other).unwrap();
}

#[test]
fn an_empty_faucet_says_so() {
    let mut fx = Fx::new();
    fx.init(DRIP_UNITS - 1);
    let w = fx.wallet();
    let err = fx.drip(&w).unwrap_err();
    assert!(err.contains("Custom(6000)"), "{err}");
}

#[test]
fn tokens_only_go_to_the_claimants_own_account() {
    let mut fx = Fx::new();
    fx.init(3 * DRIP_UNITS);
    let w = fx.wallet();
    let thief = fx.wallet();
    let ix = fx.drip_ix(&thief.pubkey(), ata(&w.pubkey(), &MINT));
    assert!(fx.send(ix, &thief).is_err(), "someone else's account");
    assert_eq!(fx.balance(fx.vault()), 3 * DRIP_UNITS);
}

#[test]
fn nobody_can_create_a_claim_for_another_wallet() {
    let mut fx = Fx::new();
    fx.init(3 * DRIP_UNITS);
    let victim = fx.wallet();
    let attacker = fx.wallet();
    let mut ix = fx.drip_ix(&attacker.pubkey(), ata(&attacker.pubkey(), &MINT));
    ix.accounts[1].pubkey = Pubkey::find_program_address(&[CLAIM_SEED, victim.pubkey().as_ref()], &devnet_faucet::id()).0;
    assert!(fx.send(ix, &attacker).is_err());
    fx.drip(&victim).unwrap();
}

#[test]
fn the_vault_cannot_be_created_twice() {
    let mut fx = Fx::new();
    fx.init(DRIP_UNITS);
    let ix = Instruction {
        program_id: devnet_faucet::id(),
        accounts: accounts::Init {
            payer: fx.payer.pubkey(),
            mint: MINT,
            vault: fx.vault(),
            token_program: anchor_spl::token::ID,
            system_program: anchor_lang::system_program::ID,
        }
        .to_account_metas(None),
        data: instruction::Init {}.data(),
    };
    let payer = fx.payer.insecure_clone();
    assert!(fx.send(ix, &payer).is_err());
}
