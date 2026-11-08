//! Solana side: instructions for the escrow program, plus devnet-only setup of the stand-in mint.

use std::str::FromStr;

use anchor_lang::{prelude::Pubkey, AccountDeserialize, InstructionData, ToAccountMetas};
use anchor_spl::associated_token::get_associated_token_address;
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_escrow::{
    state::Escrow, ESCROW_SEED, ESCROW_VAULT_SEED, MAKER_VAULT_SEED, MAKER_VAULT_TOKEN_SEED, USDT_MINT,
};
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

/// Solana's compute budget program, which sets a transaction's compute limit and its priority fee.
pub const COMPUTE_BUDGET: Pubkey = Pubkey::from_str_const("ComputeBudget111111111111111111111111111111");

/// The page's choice too: the 75th percentile of recent fees, kept between a floor and a cap, in micro-lamports per unit.
pub fn priority_price(recent: &[u64], floor: u64, cap: u64) -> u64 {
    let mut fees = recent.to_vec();
    fees.sort_unstable();
    let p75 = fees.get(fees.len() * 3 / 4).copied().unwrap_or(0);
    p75.clamp(floor, cap.max(floor))
}

/// 30% above what a simulation used, for state that changes before it lands. Solana's default when nothing was reported.
pub fn compute_limit(used: Option<u64>) -> u32 {
    used.map_or(200_000, |u| (u.saturating_mul(13) / 10).saturating_add(2_000).min(1_400_000) as u32)
}

/// Set the compute limit, then the price per unit.
pub fn budget_ixs(limit: u32, micro_lamports: u64) -> [Instruction; 2] {
    let mut set_limit = vec![2];
    set_limit.extend_from_slice(&limit.to_le_bytes());
    let mut set_price = vec![3];
    set_price.extend_from_slice(&micro_lamports.to_le_bytes());
    [
        Instruction { program_id: COMPUTE_BUDGET, accounts: vec![], data: set_limit },
        Instruction { program_id: COMPUTE_BUDGET, accounts: vec![], data: set_price },
    ]
}

/// The most a transaction's priority fee can cost, from its own compute budget instructions, in lamports, rounded up.
pub fn priority_lamports(tx: &Transaction) -> u64 {
    let (mut limit, mut price) = (200_000u128, 0u128);
    for ix in &tx.message.instructions {
        if tx.message.account_keys.get(ix.program_id_index as usize) != Some(&COMPUTE_BUDGET) {
            continue;
        }
        match ix.data.split_first() {
            Some((2, rest)) if rest.len() == 4 => limit = u32::from_le_bytes(rest.try_into().unwrap_or_default()).into(),
            Some((3, rest)) if rest.len() == 8 => price = u64::from_le_bytes(rest.try_into().unwrap_or_default()).into(),
            _ => {}
        }
    }
    (limit * price).div_ceil(1_000_000) as u64
}

pub struct Sol {
    pub rpc: RpcClient,
}

pub fn ata(owner: &Pubkey) -> Pubkey {
    get_associated_token_address(owner, &USDT_MINT)
}

pub fn escrow_pda(initiator: &Pubkey, lock_point: &[u8; 64]) -> Pubkey {
    Pubkey::find_program_address(&[ESCROW_SEED, initiator.as_ref(), &lock_point[..32]], &solana_escrow::id()).0
}

pub fn vault_pda(escrow: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[ESCROW_VAULT_SEED, escrow.as_ref()], &solana_escrow::id()).0
}

impl Sol {
    pub fn new(url: &str) -> Self {
        Self { rpc: RpcClient::new_with_commitment(url.to_string(), CommitmentConfig::finalized()) }
    }

    pub fn slot(&self) -> Res<u64> {
        self.rpc.get_slot().map_err(|e| format!("get_slot: {e}"))
    }

    pub fn genesis(&self) -> Res<String> {
        self.rpc.get_genesis_hash().map(|h| h.to_string()).map_err(|e| e.to_string())
    }

    /// What an escrow account's rent comes to, from the cluster's current rate.
    pub fn escrow_rent(&self) -> Res<u64> {
        self.rpc
            .get_minimum_balance_for_rent_exemption(8 + <Escrow as anchor_lang::Space>::INIT_SPACE)
            .map_err(|e| e.to_string())
    }

    pub fn escrow(&self, address: &Pubkey) -> Res<Option<Escrow>> {
        match self.rpc.get_account_with_commitment(address, CommitmentConfig::finalized()) {
            Ok(r) => match r.value {
                Some(a) => Escrow::try_deserialize(&mut &a.data[..]).map(Some).map_err(|e| e.to_string()),
                None => Ok(None),
            },
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn token_balance(&self, address: &Pubkey) -> Res<u64> {
        match self.rpc.get_account_with_commitment(address, CommitmentConfig::finalized()) {
            Ok(r) => Ok(r.value.map_or(0, |a| u64::from_le_bytes(a.data[64..72].try_into().unwrap_or_default()))),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn lamports(&self, address: &Pubkey) -> Res<u64> {
        self.rpc.get_balance(address).map_err(|e| e.to_string())
    }

    /// A signed transaction with a fresh blockhash, not yet sent.
    pub fn build(&self, ixs: &[Instruction], payer: &Keypair, signers: &[&Keypair]) -> Res<Transaction> {
        let hash = self.rpc.get_latest_blockhash().map_err(|e| e.to_string())?;
        let mut all: Vec<&Keypair> = vec![payer];
        all.extend_from_slice(signers);
        Ok(Transaction::new_signed_with_payer(ixs, Some(&payer.pubkey()), &all, hash))
    }

    /// With a compute limit just above what a simulation used, and a priority fee from what recently landed on the same
    /// accounts, between `floor` and `cap` micro-lamports per unit. Fails if the simulation does, so nothing doomed is signed.
    pub fn build_prioritized(&self, ixs: &[Instruction], payer: &Keypair, signers: &[&Keypair], floor: u64, cap: u64) -> Res<Transaction> {
        let probe = self.build(ixs, payer, signers)?;
        let sim = self.rpc.simulate_transaction(&probe).map_err(|e| e.to_string())?.value;
        if let Some(e) = sim.err {
            return Err(format!("simulation failed: {e:?}\n{:#?}", sim.logs));
        }
        let mut accounts: Vec<Pubkey> = ixs.iter().flat_map(|ix| ix.accounts.iter().filter(|m| m.is_writable).map(|m| m.pubkey)).collect();
        accounts.sort();
        accounts.dedup();
        // Without recent fees, the floor still applies.
        let recent: Vec<u64> = self
            .rpc
            .get_recent_prioritization_fees(&accounts)
            .map(|fees| fees.iter().map(|f| f.prioritization_fee).collect())
            .unwrap_or_default();
        let mut all = budget_ixs(compute_limit(sim.units_consumed), priority_price(&recent, floor, cap)).to_vec();
        all.extend_from_slice(ixs);
        self.build(&all, payer, signers)
    }

    /// Send, confirm at `finalized`, and refuse to send anything that fails simulation.
    pub fn send(&self, ixs: &[Instruction], payer: &Keypair, signers: &[&Keypair]) -> Res<Signature> {
        let tx = self.build(ixs, payer, signers)?;
        self.simulate(&tx)?;
        self.rpc.send_and_confirm_transaction(&tx).map_err(|e| e.to_string())
    }

    /// A failed claim would publish the secret for nothing, so nothing is sent until this passes.
    fn simulate(&self, tx: &Transaction) -> Res<()> {
        let result = self.rpc.simulate_transaction(tx).map_err(|e| e.to_string())?.value;
        match result.err {
            None => Ok(()),
            Some(e) => Err(format!("simulation failed: {e:?}\n{:#?}", result.logs)),
        }
    }

    /// Create the stand-in mint and an account for each owner, and mint to the first one. Devnet and local only.
    /// Create the stand-in mint if it isn't there, which needs its keypair, then give each holder a token account and the
    /// first one `amount`. Without the keypair, the mint must already exist, as on a validator started with it preloaded.
    pub fn setup_mint(&self, authority: &Keypair, mint: Option<&Keypair>, holders: &[Pubkey], amount: u64) -> Res<()> {
        if self
            .rpc
            .get_account_with_commitment(&USDT_MINT, CommitmentConfig::finalized())
            .map_err(|e| e.to_string())?
            .value
            .is_none()
        {
            let mint = mint.ok_or("the stand-in mint isn't there, and creating it needs its keypair")?;
            assert_eq!(mint.pubkey(), USDT_MINT, "the mint keypair must match the program's stand-in mint");
            let rent = self.rpc.get_minimum_balance_for_rent_exemption(MINT_LEN).map_err(|e| e.to_string())?;
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
            let init =
                Instruction { program_id: TOKEN_PROGRAM, accounts: vec![AccountMeta::new(USDT_MINT, false)], data };
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

    /// Mint stand-in tokens straight into a token account, such as the faucet's vault.
    pub fn mint_to(&self, authority: &Keypair, account: &Pubkey, amount: u64) -> Res<()> {
        let mut data = vec![7u8];
        data.extend_from_slice(&amount.to_le_bytes());
        let mint_to = Instruction {
            program_id: TOKEN_PROGRAM,
            accounts: vec![
                AccountMeta::new(USDT_MINT, false),
                AccountMeta::new(*account, false),
                AccountMeta::new_readonly(authority.pubkey(), true),
            ],
            data,
        };
        self.send(&[mint_to], authority, &[]).map(|_| ())
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
        lock_point: [u8; 64],
        recipient: Pubkey,
        amount: u64,
        deadline_slot: u64,
        close_on_restart: bool,
    ) -> Res<Pubkey> {
        let ix = lock_ix(&initiator.pubkey(), lock_point, recipient, amount, deadline_slot, close_on_restart);
        self.send(&[ix], initiator, &[])?;
        Ok(escrow_pda(&initiator.pubkey(), &lock_point))
    }

    pub fn claim(&self, payer: &Keypair, address: &Pubkey, escrow: &Escrow, revealed: [u8; 32]) -> Res<Signature> {
        self.send(&claim_ixs(&payer.pubkey(), address, escrow, revealed), payer, &[])
    }

    pub fn extend(&self, recipient: &Keypair, address: &Pubkey, new_deadline: u64) -> Res<Signature> {
        self.send(&[extend_ix(&recipient.pubkey(), address, new_deadline)], recipient, &[])
    }

    /// Freeze (`freeze = true`) or thaw a token account, as Tether could. Only works on a mint with a freeze authority.
    pub fn set_frozen(&self, authority: &Keypair, account: &Pubkey, freeze: bool) -> Res<Signature> {
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
    lock_point: [u8; 64],
    recipient: Pubkey,
    amount: u64,
    deadline_slot: u64,
    close_on_restart: bool,
) -> Instruction {
    let escrow = escrow_pda(initiator, &lock_point);
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
    let data =
        solana_escrow::instruction::Lock { lock_point, recipient, amount, deadline_slot, close_on_restart }.data();
    Instruction { program_id: solana_escrow::id(), accounts, data }
}

/// A claim, preceded by creating the recipient's token account if it doesn't exist.
pub fn claim_ixs(payer: &Pubkey, address: &Pubkey, escrow: &Escrow, revealed: [u8; 32]) -> Vec<Instruction> {
    let accounts = solana_escrow::accounts::Claim {
        escrow: *address,
        vault: vault_pda(address),
        recipient_token: ata(&escrow.recipient),
        rent_payer: escrow.rent_payer,
        token_program: TOKEN_PROGRAM,
    }
    .to_account_metas(None);
    let data = solana_escrow::instruction::Claim { revealed }.data();
    vec![create_ata_ix(payer, &escrow.recipient), Instruction { program_id: solana_escrow::id(), accounts, data }]
}

pub fn maker_vault_pda(owner: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[MAKER_VAULT_SEED, owner.as_ref()], &solana_escrow::id()).0
}

pub fn maker_vault_token_pda(maker_vault: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[MAKER_VAULT_TOKEN_SEED, maker_vault.as_ref()], &solana_escrow::id()).0
}

/// Create a maker vault. Signed by the cold owner, never by the daemon's host.
pub fn vault_init_ix(owner: &Pubkey, operator: Pubkey, window_slots: u64, window_cap: u64) -> Instruction {
    let maker_vault = maker_vault_pda(owner);
    let accounts = solana_escrow::accounts::VaultInit {
        owner: *owner,
        maker_vault,
        vault_token: maker_vault_token_pda(&maker_vault),
        mint: USDT_MINT,
        token_program: TOKEN_PROGRAM,
        system_program: anchor_lang::system_program::ID,
    }
    .to_account_metas(None);
    let data = solana_escrow::instruction::VaultInit { operator, window_slots, window_cap }.data();
    Instruction { program_id: solana_escrow::id(), accounts, data }
}

/// Move tokens from the depositor's canonical account into a vault.
pub fn vault_deposit_ix(depositor: &Pubkey, vault_owner: &Pubkey, amount: u64) -> Instruction {
    let maker_vault = maker_vault_pda(vault_owner);
    let accounts = solana_escrow::accounts::VaultDeposit {
        depositor: *depositor,
        maker_vault,
        vault_token: maker_vault_token_pda(&maker_vault),
        from: ata(depositor),
        token_program: TOKEN_PROGRAM,
    }
    .to_account_metas(None);
    let data = solana_escrow::instruction::VaultDeposit { amount }.data();
    Instruction { program_id: solana_escrow::id(), accounts, data }
}

pub fn lock_from_vault_ix(
    operator: &Pubkey,
    vault_owner: &Pubkey,
    lock_point: [u8; 64],
    recipient: Pubkey,
    amount: u64,
    deadline_slot: u64,
    close_on_restart: bool,
) -> Instruction {
    let maker_vault = maker_vault_pda(vault_owner);
    let escrow = escrow_pda(&maker_vault, &lock_point);
    let accounts = solana_escrow::accounts::LockFromVault {
        operator: *operator,
        maker_vault,
        vault_token: maker_vault_token_pda(&maker_vault),
        escrow,
        vault: vault_pda(&escrow),
        mint: USDT_MINT,
        token_program: TOKEN_PROGRAM,
        system_program: anchor_lang::system_program::ID,
    }
    .to_account_metas(None);
    let data =
        solana_escrow::instruction::LockFromVault { lock_point, recipient, amount, deadline_slot, close_on_restart }
            .data();
    Instruction { program_id: solana_escrow::id(), accounts, data }
}

/// Whether a token account exists and isn't frozen.
impl Sol {
    /// The latest hard-fork slot, as the program itself sees it.
    pub fn last_restart_slot(&self) -> Res<u64> {
        let sysvar = Pubkey::from_str("SysvarLastRestartS1ot1111111111111111111111").map_err(|e| e.to_string())?;
        let account =
            self.rpc.get_account_with_commitment(&sysvar, CommitmentConfig::finalized()).map_err(|e| e.to_string())?;
        let data = account.value.ok_or("no LastRestartSlot sysvar")?.data;
        Ok(u64::from_le_bytes(data.get(..8).ok_or("short sysvar")?.try_into().map_err(|_| "short sysvar")?))
    }

    /// Send without waiting. The engine sees the result at `finalized` on a later tick, so the loop never blocks.
    pub fn send_signed(&self, tx: &Transaction) -> Res<Signature> {
        self.rpc.send_transaction(tx).map_err(|e| e.to_string())
    }

    pub fn token_usable(&self, address: &Pubkey) -> Res<bool> {
        match self.rpc.get_account_with_commitment(address, CommitmentConfig::finalized()) {
            Ok(r) => Ok(r.value.is_some_and(|a| a.data.get(108) == Some(&1))),
            Err(e) => Err(e.to_string()),
        }
    }
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
    Instruction { program_id: solana_escrow::id(), accounts, data }
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
    Instruction { program_id: solana_escrow::id(), accounts, data }
}

/// Close a settled escrow and return its rent to whoever paid it.
pub fn close_ix(closer: &Pubkey, address: &Pubkey, escrow: &Escrow) -> Instruction {
    let accounts =
        solana_escrow::accounts::Close { closer: *closer, escrow: *address, rent_payer: escrow.rent_payer }.to_account_metas(None);
    let data = solana_escrow::instruction::Close {}.data();
    Instruction { program_id: solana_escrow::id(), accounts, data }
}

pub fn extend_ix(recipient: &Pubkey, address: &Pubkey, new_deadline: u64) -> Instruction {
    let accounts = solana_escrow::accounts::Extend { recipient: *recipient, escrow: *address }.to_account_metas(None);
    let data = solana_escrow::instruction::Extend { new_deadline }.data();
    Instruction { program_id: solana_escrow::id(), accounts, data }
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
