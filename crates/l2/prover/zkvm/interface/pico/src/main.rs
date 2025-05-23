#![no_main]

use pico_sdk::io::{commit, read_as};

use ethrex_blockchain::{validate_block, validate_gas_used};
use ethrex_vm::backends::revm::{REVM, db::EvmState};
use zkvm_interface::{
    io::{ProgramInput, ProgramOutput},
    trie::{update_tries, verify_db},
};

pico_sdk::entrypoint!(main);

pub fn main() {
    let ProgramInput {
        block,
        parent_block_header,
        db,
    } = read_as();
    let mut state = EvmState::from(db.clone());
    let chain_config = state
        .chain_config()
        .expect("Failed to get chain config from state");

    // Validate the block
    validate_block(&block, &parent_block_header, &chain_config).expect("invalid block");

    // Tries used for validating initial and final state root
    let (mut state_trie, mut storage_tries) = db
        .get_tries()
        .expect("failed to build state and storage tries or state is not valid");

    // Validate the initial state
    let initial_state_hash = state_trie.hash_no_commit();
    if initial_state_hash != parent_block_header.state_root {
        panic!("invalid initial state trie");
    }
    if !verify_db(&db, &state_trie, &storage_tries).expect("failed to validate database") {
        panic!("invalid database")
    };

    let result = REVM::execute_block(&block, &mut state).expect("failed to execute block");
    let receipts = result.receipts;
    let account_updates = REVM::get_state_transitions(&mut state);
    validate_gas_used(&receipts, &block.header).expect("invalid gas used");

    // Output gas for measurement purposes
    // let cumulative_gas_used = receipts
    //     .last()
    //     .map(|last_receipt| last_receipt.cumulative_gas_used)
    //     .unwrap_or_default();
    // write(&cumulative_gas_used);

    // Update state trie
    update_tries(&mut state_trie, &mut storage_tries, &account_updates)
        .expect("failed to update state and storage tries");

    // Calculate final state root hash and check
    let final_state_hash = state_trie.hash_no_commit();
    if final_state_hash != block.header.state_root {
        panic!("invalid final state trie");
    }

    commit(&ProgramOutput {
        initial_state_hash,
        final_state_hash,
    });
}
