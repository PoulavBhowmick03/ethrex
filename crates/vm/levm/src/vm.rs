use crate::{
    call_frame::{CacheBackup, CallFrame},
    constants::*,
    db::{cache, gen_db::GeneralizedDatabase},
    environment::Environment,
    errors::{ExecutionReport, InternalError, OpcodeResult, TxResult, VMError},
    hooks::{default_hook::DefaultHook, hook::Hook, l2_hook::L2Hook},
    precompiles::{
        execute_precompile, is_precompile, SIZE_PRECOMPILES_CANCUN, SIZE_PRECOMPILES_PRAGUE,
        SIZE_PRECOMPILES_PRE_CANCUN,
    },
    utils::*,
    TransientStorage,
};
use bytes::Bytes;
use ethrex_common::{
    types::{
        tx_fields::{AccessList, AuthorizationList},
        BlockHeader, ChainConfig, Fork, ForkBlobSchedule, Transaction, TxKind,
    },
    Address, H256, U256,
};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fmt::Debug,
    sync::Arc,
};
pub type Storage = HashMap<U256, H256>;

#[derive(Debug, Clone, Default)]
pub struct Substate {
    pub selfdestruct_set: HashSet<Address>,
    pub touched_accounts: HashSet<Address>,
    pub touched_storage_slots: HashMap<Address, BTreeSet<H256>>,
    pub created_accounts: HashSet<Address>,
}

/// Backup if sub-context is reverted. It consists of a copy of:
///   - Substate
///   - Gas Refunds
///   - Transient Storage
pub struct StateBackup {
    pub substate: Substate,
    pub refunded_gas: u64,
    pub transient_storage: TransientStorage,
}

impl StateBackup {
    pub fn new(
        substate: Substate,
        refunded_gas: u64,
        transient_storage: TransientStorage,
    ) -> StateBackup {
        StateBackup {
            substate,
            refunded_gas,
            transient_storage,
        }
    }
}

#[derive(Debug, Clone, Copy)]
/// This structs holds special configuration variables specific to the
/// EVM. In most cases, at least at the time of writing (February
/// 2025), you want to use the default blob_schedule values for the
/// specified Fork. The "intended" way to do this is by using the `EVMConfig::canonical_values(fork: Fork)` function.
///
/// However, that function should NOT be used IF you want to use a
/// custom ForkBlobSchedule, like it's described in
/// [EIP-7840](https://eips.ethereum.org/EIPS/eip-7840). For more
/// information read the EIP
pub struct EVMConfig {
    pub fork: Fork,
    pub blob_schedule: ForkBlobSchedule,
}

impl EVMConfig {
    pub fn new(fork: Fork, blob_schedule: ForkBlobSchedule) -> EVMConfig {
        EVMConfig {
            fork,
            blob_schedule,
        }
    }

    pub fn new_from_chain_config(chain_config: &ChainConfig, block_header: &BlockHeader) -> Self {
        let fork = chain_config.fork(block_header.timestamp);

        let blob_schedule = chain_config
            .get_fork_blob_schedule(block_header.timestamp)
            .unwrap_or_else(|| EVMConfig::canonical_values(fork));

        EVMConfig::new(fork, blob_schedule)
    }

    /// This function is used for running the EF tests. If you don't
    /// have acces to a EVMConfig (mainly in the form of a
    /// genesis.json file) you can use this function to get the
    /// "Default" ForkBlobSchedule for that specific Fork.
    /// NOTE: This function could potentially be expanded to include
    /// other types of "default"s.
    pub fn canonical_values(fork: Fork) -> ForkBlobSchedule {
        let max_blobs_per_block: u64 = Self::max_blobs_per_block(fork);
        let target: u64 = Self::get_target_blob_gas_per_block_(fork);
        let base_fee_update_fraction: u64 = Self::get_blob_base_fee_update_fraction_value(fork);

        ForkBlobSchedule {
            target,
            max: max_blobs_per_block,
            base_fee_update_fraction,
        }
    }

    /// After EIP-7691 the maximum number of blob hashes changed. For more
    /// information see
    /// [EIP-7691](https://eips.ethereum.org/EIPS/eip-7691#specification).
    const fn max_blobs_per_block(fork: Fork) -> u64 {
        match fork {
            Fork::Prague => MAX_BLOB_COUNT_ELECTRA,
            Fork::Osaka => MAX_BLOB_COUNT_ELECTRA,
            _ => MAX_BLOB_COUNT,
        }
    }

    /// According to [EIP-7691](https://eips.ethereum.org/EIPS/eip-7691#specification):
    ///
    /// "These changes imply that get_base_fee_per_blob_gas and
    /// calc_excess_blob_gas functions defined in EIP-4844 use the new
    /// values for the first block of the fork (and for all subsequent
    /// blocks)."
    const fn get_blob_base_fee_update_fraction_value(fork: Fork) -> u64 {
        match fork {
            Fork::Prague | Fork::Osaka => BLOB_BASE_FEE_UPDATE_FRACTION_PRAGUE,
            _ => BLOB_BASE_FEE_UPDATE_FRACTION,
        }
    }

    /// According to [EIP-7691](https://eips.ethereum.org/EIPS/eip-7691#specification):
    const fn get_target_blob_gas_per_block_(fork: Fork) -> u64 {
        match fork {
            Fork::Prague | Fork::Osaka => TARGET_BLOB_GAS_PER_BLOCK_PECTRA,
            _ => TARGET_BLOB_GAS_PER_BLOCK,
        }
    }
}

impl Default for EVMConfig {
    /// The default EVMConfig depends on the default Fork.
    fn default() -> Self {
        let fork = core::default::Default::default();
        EVMConfig {
            fork,
            blob_schedule: Self::canonical_values(fork),
        }
    }
}

pub struct VM<'a> {
    pub call_frames: Vec<CallFrame>,
    pub env: Environment,
    /// Information that is acted upon immediately following the
    /// transaction.
    pub accrued_substate: Substate,
    pub db: &'a mut GeneralizedDatabase,
    pub tx_kind: TxKind,
    pub access_list: AccessList,
    pub authorization_list: Option<AuthorizationList>,
    pub hooks: Vec<Arc<dyn Hook>>,
    pub return_data: Vec<RetData>,
    pub backups: Vec<StateBackup>,
}

pub struct RetData {
    pub is_create: bool,
    pub ret_offset: U256,
    pub ret_size: usize,
    pub should_transfer_value: bool,
    pub to: Address,
    pub msg_sender: Address,
    pub value: U256,
    pub max_message_call_gas: u64,
}

impl<'a> VM<'a> {
    pub fn new(
        env: Environment,
        db: &'a mut GeneralizedDatabase,
        tx: &Transaction,
    ) -> Result<Self, VMError> {
        // Add sender and recipient (in the case of a Call) to cache [https://www.evm.codes/about#access_list]
        let mut default_touched_accounts = HashSet::from_iter([env.origin].iter().cloned());

        // [EIP-3651] - Add coinbase to cache if the spec is SHANGHAI or higher
        if env.config.fork >= Fork::Shanghai {
            default_touched_accounts.insert(env.coinbase);
        }

        let mut default_touched_storage_slots: HashMap<Address, BTreeSet<H256>> = HashMap::new();

        // Add access lists contents to cache
        for (address, keys) in tx.access_list() {
            default_touched_accounts.insert(address);
            let mut warm_slots = BTreeSet::new();
            for slot in keys {
                warm_slots.insert(slot);
            }
            default_touched_storage_slots.insert(address, warm_slots);
        }

        // Add precompiled contracts addresses to cache.
        let max_precompile_address = match env.config.fork {
            spec if spec >= Fork::Prague => SIZE_PRECOMPILES_PRAGUE,
            spec if spec >= Fork::Cancun => SIZE_PRECOMPILES_CANCUN,
            spec if spec < Fork::Cancun => SIZE_PRECOMPILES_PRE_CANCUN,
            _ => return Err(VMError::Internal(InternalError::InvalidSpecId)),
        };
        for i in 1..=max_precompile_address {
            default_touched_accounts.insert(Address::from_low_u64_be(i));
        }

        // When instantiating a new vm the current value of the storage slots are actually the original values because it is a new transaction
        for account in db.cache.values_mut() {
            for storage_slot in account.storage.values_mut() {
                storage_slot.original_value = storage_slot.current_value;
            }
        }

        let hooks: Vec<Arc<dyn Hook>> = match tx {
            Transaction::PrivilegedL2Transaction(privileged_tx) => vec![Arc::new(L2Hook {
                recipient: privileged_tx.recipient,
            })],
            _ => vec![Arc::new(DefaultHook)],
        };

        match tx.to() {
            TxKind::Call(address_to) => {
                default_touched_accounts.insert(address_to);

                let mut substate = Substate {
                    selfdestruct_set: HashSet::new(),
                    touched_accounts: default_touched_accounts,
                    touched_storage_slots: default_touched_storage_slots,
                    created_accounts: HashSet::new(),
                };

                let (_is_delegation, _eip7702_gas_consumed, _code_address, bytecode) =
                    eip7702_get_code(db, &mut substate, address_to)?;

                let initial_call_frame = CallFrame::new(
                    env.origin,
                    address_to,
                    address_to,
                    bytecode,
                    tx.value(),
                    tx.data().clone(),
                    false,
                    env.gas_limit,
                    0,
                    0,
                    false,
                );

                Ok(Self {
                    call_frames: vec![initial_call_frame],
                    env,
                    accrued_substate: substate,
                    db,
                    tx_kind: TxKind::Call(address_to),
                    access_list: tx.access_list(),
                    authorization_list: tx.authorization_list(),
                    hooks,
                    return_data: vec![],
                    backups: vec![],
                })
            }
            TxKind::Create => {
                let sender_nonce = db.get_account(env.origin)?.info.nonce;
                let new_contract_address = calculate_create_address(env.origin, sender_nonce)
                    .map_err(|_| VMError::Internal(InternalError::CouldNotComputeCreateAddress))?;

                default_touched_accounts.insert(new_contract_address);

                let initial_call_frame = CallFrame::new(
                    env.origin,
                    new_contract_address,
                    new_contract_address,
                    Bytes::new(), // Bytecode is assigned after passing validations.
                    tx.value(),
                    tx.data().clone(), // Calldata is removed after passing validations.
                    false,
                    env.gas_limit,
                    0,
                    0,
                    false,
                );

                let substate = Substate {
                    selfdestruct_set: HashSet::new(),
                    touched_accounts: default_touched_accounts,
                    touched_storage_slots: default_touched_storage_slots,
                    created_accounts: HashSet::from([new_contract_address]),
                };

                Ok(Self {
                    call_frames: vec![initial_call_frame],
                    env,
                    accrued_substate: substate,
                    db,
                    tx_kind: TxKind::Create,
                    access_list: tx.access_list(),
                    authorization_list: tx.authorization_list(),
                    hooks,
                    return_data: vec![],
                    backups: vec![],
                })
            }
        }
    }

    pub fn run_execution(&mut self) -> Result<ExecutionReport, VMError> {
        let fork = self.env.config.fork;

        if is_precompile(&self.current_call_frame()?.code_address, fork) {
            let mut current_call_frame = self
                .call_frames
                .pop()
                .ok_or(VMError::Internal(InternalError::CouldNotPopCallframe))?;
            let precompile_result = execute_precompile(&mut current_call_frame, fork);
            let backup = self
                .backups
                .pop()
                .ok_or(VMError::Internal(InternalError::CouldNotPopCallframe))?;
            let report =
                self.handle_precompile_result(precompile_result, backup, &mut current_call_frame)?;
            self.handle_return(&current_call_frame, &report)?;
            self.current_call_frame_mut()?.increment_pc_by(1)?;
            return Ok(report);
        }

        loop {
            let opcode = self.current_call_frame()?.next_opcode();

            let op_result = self.handle_current_opcode(opcode);

            match op_result {
                Ok(OpcodeResult::Continue { pc_increment }) => self
                    .current_call_frame_mut()?
                    .increment_pc_by(pc_increment)?,
                Ok(OpcodeResult::Halt) => {
                    let mut current_call_frame = self
                        .call_frames
                        .pop()
                        .ok_or(VMError::Internal(InternalError::CouldNotPopCallframe))?;
                    let report = self.handle_opcode_result(&mut current_call_frame)?;
                    if self.handle_return(&current_call_frame, &report)? {
                        self.current_call_frame_mut()?.increment_pc_by(1)?;
                    } else {
                        return Ok(report);
                    }
                }
                Err(error) => {
                    let mut current_call_frame = self
                        .call_frames
                        .pop()
                        .ok_or(VMError::Internal(InternalError::CouldNotPopCallframe))?;
                    let report = self.handle_opcode_error(error, &mut current_call_frame)?;
                    if self.handle_return(&current_call_frame, &report)? {
                        self.current_call_frame_mut()?.increment_pc_by(1)?;
                    } else {
                        return Ok(report);
                    }
                }
            }
        }
    }

    pub fn restore_state(
        &mut self,
        backup: StateBackup,
        call_frame_backup: CacheBackup,
    ) -> Result<(), VMError> {
        self.restore_cache_state(call_frame_backup)?;
        self.accrued_substate = backup.substate;
        self.env.refunded_gas = backup.refunded_gas;
        self.env.transient_storage = backup.transient_storage;
        Ok(())
    }

    pub fn is_create(&self) -> bool {
        matches!(self.tx_kind, TxKind::Create)
    }

    /// Executes without making changes to the cache.
    pub fn stateless_execute(&mut self) -> Result<ExecutionReport, VMError> {
        let cache_backup = self.db.cache.clone();
        let report = self.execute()?;
        // Restore the cache to its original state
        self.db.cache = cache_backup;
        Ok(report)
    }

    /// Main function for executing an external transaction
    pub fn execute(&mut self) -> Result<ExecutionReport, VMError> {
        if let Err(e) = self.prepare_execution() {
            // We need to do a cleanup of the cache so that it doesn't interfere with next transaction's execution
            self.restore_cache_state(self.current_call_frame()?.cache_backup.clone())?;
            return Err(e);
        }

        // Here we clear the cache backup because if prepare_execution succeeded we don't want to
        // revert the changes it made.
        // Even if the transaction reverts we want to apply these kind of changes!
        // These are: Incrementing sender nonce, transferring value to a delegate account, decreasing sender account balance
        self.current_call_frame_mut()?.cache_backup = HashMap::new();

        // In CREATE type transactions:
        //  Add created contract to cache, reverting transaction if the address is already occupied
        if self.is_create() {
            let new_contract_address = self.current_call_frame()?.to;
            let new_account = self.get_account_mut(new_contract_address)?;

            if new_account.has_code_or_nonce() {
                return self.handle_create_non_empty_account();
            }

            self.increase_account_balance(
                new_contract_address,
                self.current_call_frame()?.msg_value,
            )?;

            // https://eips.ethereum.org/EIPS/eip-161
            if self.env.config.fork >= Fork::SpuriousDragon {
                self.increment_account_nonce(new_contract_address)?;
            };
        }

        // Backup of Substate, Gas Refunds and Transient Storage if sub-context is reverted
        let backup = StateBackup::new(
            self.accrued_substate.clone(),
            self.env.refunded_gas,
            self.env.transient_storage.clone(),
        );
        self.backups.push(backup);

        let mut report = self.run_execution()?;

        self.finalize_execution(&mut report)?;
        Ok(report)
    }

    pub fn current_call_frame_mut(&mut self) -> Result<&mut CallFrame, VMError> {
        self.call_frames.last_mut().ok_or(VMError::Internal(
            InternalError::CouldNotAccessLastCallframe,
        ))
    }

    pub fn current_call_frame(&self) -> Result<&CallFrame, VMError> {
        self.call_frames.last().ok_or(VMError::Internal(
            InternalError::CouldNotAccessLastCallframe,
        ))
    }

    fn handle_create_non_empty_account(&mut self) -> Result<ExecutionReport, VMError> {
        let mut report = ExecutionReport {
            result: TxResult::Revert(VMError::AddressAlreadyOccupied),
            gas_used: self.env.gas_limit,
            gas_refunded: 0,
            logs: vec![],
            output: Bytes::new(),
        };

        self.finalize_execution(&mut report)?;

        Ok(report)
    }

    fn prepare_execution(&mut self) -> Result<(), VMError> {
        // NOTE: ATTOW the default hook is created in VM::new(), so
        // (in theory) _at least_ the default prepare execution should
        // run
        for hook in self.hooks.clone() {
            hook.prepare_execution(self)?;
        }

        Ok(())
    }

    fn finalize_execution(&mut self, report: &mut ExecutionReport) -> Result<(), VMError> {
        // NOTE: ATTOW the default hook is created in VM::new(), so
        // (in theory) _at least_ the default finalize execution should
        // run
        for hook in self.hooks.clone() {
            hook.finalize_execution(self, report)?;
        }

        Ok(())
    }

    /// Restores the cache state to the state before changes made during a callframe.
    fn restore_cache_state(&mut self, call_frame_backup: CacheBackup) -> Result<(), VMError> {
        for (address, account_opt) in call_frame_backup {
            if let Some(account) = account_opt {
                // restore the account to the state before the call
                cache::insert_account(&mut self.db.cache, address, account.clone());
            } else {
                // remove from cache if it wasn't there before
                cache::remove_account(&mut self.db.cache, &address);
            }
        }
        Ok(())
    }
}
