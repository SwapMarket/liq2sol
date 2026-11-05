//! Solana side: instructions for the escrow program, plus devnet-only setup of the stand-in mint.

use anchor_lang::{prelude::Pubkey, AccountDeserialize, InstructionData, ToAccountMetas};
use anchor_spl::associated_token::get_associated_token_address;
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_escrow::{state::Escrow, ESCROW_SEED, ESCROW_VAULT_SEED, USDT_MINT};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    signature::{Keypair, Signature},
    signer::Signer,
    transaction::Transaction,
};

pub const MAINNET_GENESIS: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
const MINT_LEN: usize = 82;
const ATA_PROGRAM: Pubkey = anchor_spl::associated_token::ID;
const TOKEN_PROGRAM: Pubkey = anchor_spl::token::ID;

pub type Res<T> = Result<T, String>;

pub struct Sol {
    pub rpc: RpcClient,
}

pub fn ata(owner: &Pubkey) -> Pubkey {
    get_associated_token_address(owner, &USDT_MINT)
}

pub fn escrow_pda(initiator: &Pubkey, hash: &[u8; 32]) -> Pubkey {
    Pubkey::find_program_address(
        &[ESCROW_SEED, initiator.as_ref(), hash.as_ref()],
        &solana_escrow::id(),
    )
    .0
}

pub fn vault_pda(escrow: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[ESCROW_VAULT_SEED, escrow.as_ref()], &solana_escrow::id()).0
}

impl Sol {
    pub fn new(url: &str) -> Self {
        Self {
            rpc: RpcClient::new_with_commitment(url.to_string(), CommitmentConfig::finalized()),
        }
    }

    pub fn slot(&self) -> Res<u64> {
        self.rpc.get_slot().map_err(|e| format!("get_slot: {e}"))
    }

    pub fn genesis(&self) -> Res<String> {
        self.rpc
            .get_genesis_hash()
            .map(|h| h.to_string())
            .map_err(|e| e.to_string())
    }

    pub fn escrow(&self, address: &Pubkey) -> Res<Option<Escrow>> {
        match self
            .rpc
            .get_account_with_commitment(address, CommitmentConfig::finalized())
        {
            Ok(r) => match r.value {
                Some(a) => Escrow::try_deserialize(&mut &a.data[..])
                    .map(Some)
                    .map_err(|e| e.to_string()),
                None => Ok(None),
            },
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn token_balance(&self, address: &Pubkey) -> Res<u64> {
        match self
            .rpc
            .get_account_with_commitment(address, CommitmentConfig::finalized())
        {
            Ok(r) => Ok(r.value.map_or(0, |a| {
                u64::from_le_bytes(a.data[64..72].try_into().unwrap_or_default())
            })),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn lamports(&self, address: &Pubkey) -> Res<u64> {
        self.rpc.get_balance(address).map_err(|e| e.to_string())
    }

    /// A signed transaction with a fresh blockhash, not yet sent.
    pub fn build(
        &self,
        ixs: &[Instruction],
        payer: &Keypair,
        signers: &[&Keypair],
    ) -> Res<Transaction> {
        let hash = self.rpc.get_latest_blockhash().map_err(|e| e.to_string())?;
        let mut all: Vec<&Keypair> = vec![payer];
        all.extend_from_slice(signers);
        Ok(Transaction::new_signed_with_payer(
            ixs,
            Some(&payer.pubkey()),
            &all,
            hash,
        ))
    }

    /// Send, confirm at `finalized`, and refuse to send anything that fails simulation.
    pub fn send(
        &self,
        ixs: &[Instruction],
        payer: &Keypair,
        signers: &[&Keypair],
    ) -> Res<Signature> {
        let tx = self.build(ixs, payer, signers)?;
        self.simulate(&tx)?;
        self.rpc
            .send_and_confirm_transaction(&tx)
            .map_err(|e| e.to_string())
    }

    /// A failed claim would publish the preimage for nothing, so nothing is sent until this passes.
    fn simulate(&self, tx: &Transaction) -> Res<()> {
        let result = self
            .rpc
            .simulate_transaction(tx)
            .map_err(|e| e.to_string())?
            .value;
        match result.err {
            None => Ok(()),
            Some(e) => Err(format!("simulation failed: {e:?}\n{:#?}", result.logs)),
        }
    }

    /// Create the stand-in mint and an account for each owner, and mint to the first one. Devnet and local only.
    pub fn setup_mint(
        &self,
        authority: &Keypair,
        mint: &Keypair,
        holders: &[Pubkey],
        amount: u64,
    ) -> Res<()> {
        assert_eq!(
            mint.pubkey(),
            USDT_MINT,
            "the mint keypair must match the program's stand-in mint"
        );
        if self
            .rpc
            .get_account_with_commitment(&USDT_MINT, CommitmentConfig::finalized())
            .map_err(|e| e.to_string())?
            .value
            .is_none()
        {
            let rent = self
                .rpc
                .get_minimum_balance_for_rent_exemption(MINT_LEN)
                .map_err(|e| e.to_string())?;
            let create = anchor_lang::solana_program::system_instruction::create_account(
                &authority.pubkey(),
                &USDT_MINT,
                rent,
                MINT_LEN as u64,
                &TOKEN_PROGRAM,
            );
            let mut data = vec![20u8, 6];
            data.extend_from_slice(authority.pubkey().as_ref());
            // A freeze authority, as real USDT has.
            data.push(1);
            data.extend_from_slice(authority.pubkey().as_ref());
            let init = Instruction {
                program_id: TOKEN_PROGRAM,
                accounts: vec![AccountMeta::new(USDT_MINT, false)],
                data,
            };
            self.send(&[create, init], authority, &[mint])?;
        }
        for owner in holders {
            self.send(&[create_ata_ix(&authority.pubkey(), owner)], authority, &[])?;
        }
        if let Some(first) = holders.first() {
            if self.token_balance(&ata(first))? < amount {
                let mut data = vec![7u8];
                data.extend_from_slice(&amount.to_le_bytes());
                let mint_to = Instruction {
                    program_id: TOKEN_PROGRAM,
                    accounts: vec![
                        AccountMeta::new(USDT_MINT, false),
                        AccountMeta::new(ata(first), false),
                        AccountMeta::new_readonly(authority.pubkey(), true),
                    ],
                    data,
                };
                self.send(&[mint_to], authority, &[])?;
            }
        }
        Ok(())
    }

    pub fn top_up_token(&self, authority: &Keypair, owner: &Pubkey, amount: u64) -> Res<()> {
        self.send(&[create_ata_ix(&authority.pubkey(), owner)], authority, &[])?;
        if self.token_balance(&ata(owner))? >= amount {
            return Ok(());
        }
        let mut data = vec![7u8];
        data.extend_from_slice(&amount.to_le_bytes());
        let ix = Instruction {
            program_id: TOKEN_PROGRAM,
            accounts: vec![
                AccountMeta::new(USDT_MINT, false),
                AccountMeta::new(ata(owner), false),
                AccountMeta::new_readonly(authority.pubkey(), true),
            ],
            data,
        };
        self.send(&[ix], authority, &[]).map(|_| ())
    }

    pub fn lock(
        &self,
        initiator: &Keypair,
        payment_hash: [u8; 32],
        recipient: Pubkey,
        amount: u64,
        deadline_slot: u64,
        close_on_restart: bool,
    ) -> Res<Pubkey> {
        let ix = lock_ix(
            &initiator.pubkey(),
            payment_hash,
            recipient,
            amount,
            deadline_slot,
            close_on_restart,
        );
        self.send(&[ix], initiator, &[])?;
        Ok(escrow_pda(&initiator.pubkey(), &payment_hash))
    }

    pub fn claim(
        &self,
        payer: &Keypair,
        address: &Pubkey,
        escrow: &Escrow,
        preimage: [u8; 32],
    ) -> Res<Signature> {
        self.send(
            &claim_ixs(&payer.pubkey(), address, escrow, preimage),
            payer,
            &[],
        )
    }

    pub fn extend(
        &self,
        recipient: &Keypair,
        address: &Pubkey,
        new_deadline: u64,
    ) -> Res<Signature> {
        self.send(
            &[extend_ix(&recipient.pubkey(), address, new_deadline)],
            recipient,
            &[],
        )
    }

    /// Freeze (`freeze = true`) or thaw a token account, as Tether could. Only works on a mint with a freeze authority.
    pub fn set_frozen(
        &self,
        authority: &Keypair,
        account: &Pubkey,
        freeze: bool,
    ) -> Res<Signature> {
        let ix = Instruction {
            program_id: TOKEN_PROGRAM,
            accounts: vec![
                AccountMeta::new(*account, false),
                AccountMeta::new_readonly(USDT_MINT, false),
                AccountMeta::new_readonly(authority.pubkey(), true),
            ],
            data: vec![if freeze { 10 } else { 11 }],
        };
        self.send(&[ix], authority, &[])
    }

    pub fn refund(&self, payer: &Keypair, address: &Pubkey, escrow: &Escrow) -> Res<Signature> {
        self.send(&[refund_ix(address, escrow)], payer, &[])
    }
}

pub fn lock_ix(
    initiator: &Pubkey,
    payment_hash: [u8; 32],
    recipient: Pubkey,
    amount: u64,
    deadline_slot: u64,
    close_on_restart: bool,
) -> Instruction {
    let escrow = escrow_pda(initiator, &payment_hash);
    let accounts = solana_escrow::accounts::Lock {
        initiator: *initiator,
        escrow,
        vault: vault_pda(&escrow),
        initiator_token: ata(initiator),
        mint: USDT_MINT,
        token_program: TOKEN_PROGRAM,
        system_program: anchor_lang::system_program::ID,
    }
    .to_account_metas(None);
    let data = solana_escrow::instruction::Lock {
        payment_hash,
        recipient,
        amount,
        deadline_slot,
        close_on_restart,
    }
    .data();
    Instruction {
        program_id: solana_escrow::id(),
        accounts,
        data,
    }
}

/// A claim, preceded by creating the recipient's token account if it doesn't exist.
pub fn claim_ixs(
    payer: &Pubkey,
    address: &Pubkey,
    escrow: &Escrow,
    preimage: [u8; 32],
) -> Vec<Instruction> {
    let accounts = solana_escrow::accounts::Claim {
        escrow: *address,
        vault: vault_pda(address),
        recipient_token: ata(&escrow.recipient),
        rent_payer: escrow.rent_payer,
        token_program: TOKEN_PROGRAM,
    }
    .to_account_metas(None);
    let data = solana_escrow::instruction::Claim { preimage }.data();
    vec![
        create_ata_ix(payer, &escrow.recipient),
        Instruction {
            program_id: solana_escrow::id(),
            accounts,
            data,
        },
    ]
}

pub fn refund_ix(address: &Pubkey, escrow: &Escrow) -> Instruction {
    let accounts = solana_escrow::accounts::Refund {
        escrow: *address,
        vault: vault_pda(address),
        initiator_token: escrow.refund_token,
        rent_payer: escrow.rent_payer,
        token_program: TOKEN_PROGRAM,
    }
    .to_account_metas(None);
    let data = solana_escrow::instruction::Refund {}.data();
    Instruction {
        program_id: solana_escrow::id(),
        accounts,
        data,
    }
}

pub fn release_ix(recipient: &Pubkey, address: &Pubkey, escrow: &Escrow) -> Instruction {
    let accounts = solana_escrow::accounts::Release {
        recipient: *recipient,
        escrow: *address,
        vault: vault_pda(address),
        initiator_token: escrow.refund_token,
        rent_payer: escrow.rent_payer,
        token_program: TOKEN_PROGRAM,
    }
    .to_account_metas(None);
    let data = solana_escrow::instruction::Release {}.data();
    Instruction {
        program_id: solana_escrow::id(),
        accounts,
        data,
    }
}

pub fn extend_ix(recipient: &Pubkey, address: &Pubkey, new_deadline: u64) -> Instruction {
    let accounts = solana_escrow::accounts::Extend {
        recipient: *recipient,
        escrow: *address,
    }
    .to_account_metas(None);
    let data = solana_escrow::instruction::Extend { new_deadline }.data();
    Instruction {
        program_id: solana_escrow::id(),
        accounts,
        data,
    }
}

fn create_ata_ix(payer: &Pubkey, owner: &Pubkey) -> Instruction {
    Instruction {
        program_id: ATA_PROGRAM,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(ata(owner), false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(USDT_MINT, false),
            AccountMeta::new_readonly(anchor_lang::system_program::ID, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM, false),
        ],
        data: vec![1],
    }
}
