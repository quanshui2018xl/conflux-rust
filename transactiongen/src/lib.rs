// Copyright 2019 Conflux Foundation. All rights reserved.
// Conflux is free software and distributed under GNU General Public License.
// See http://www.gnu.org/licenses/

extern crate cfx_bytes as bytes;
extern crate core;
extern crate keylib;
extern crate network;
extern crate parking_lot;
extern crate primitives;
extern crate rand;
extern crate secret_store;
#[macro_use]
extern crate log;

use crate::bytes::Bytes;
use cfx_types::{Address, H256, H512, U256, U512};
use cfxcore::{
    executive::contract_address,
    state::State,
    statedb::StateDb,
    storage::{StorageManager, StorageManagerTrait},
    vm::CreateContractAddress,
    SharedConsensusGraph, SharedTransactionPool,
};
use hex::*;
use keylib::{public_to_address, Generator, KeyPair, Random};
use network::Error;
use parking_lot::RwLock;
use primitives::{
    transaction::Action, Account, SignedTransaction, Transaction,
};
use rand::prelude::*;
use rlp::Encodable;
use secret_store::{SecretStore, SharedSecretStore};
use std::{
    collections::HashMap,
    sync::Arc,
    thread,
    time::{self, Instant},
};
use time::Duration;

pub mod propagate;

enum TransGenState {
    Start,
    Stop,
}

pub struct TransactionGeneratorConfig {
    pub generate_tx: bool,
    pub period: time::Duration,
    pub account_count: usize,
}

impl TransactionGeneratorConfig {
    pub fn new(
        generate_tx: bool, period_ms: u64, account_count: usize,
    ) -> Self {
        TransactionGeneratorConfig {
            generate_tx,
            period: time::Duration::from_micros(period_ms),
            account_count,
        }
    }
}

pub struct TransactionGenerator {
    pub consensus: SharedConsensusGraph,
    pub storage_manager: Arc<StorageManager>,
    txpool: SharedTransactionPool,
    secret_store: SharedSecretStore,
    state: RwLock<TransGenState>,
    key_pair: Option<KeyPair>,
}

pub type SharedTransactionGenerator = Arc<TransactionGenerator>;

impl TransactionGenerator {
    pub fn new(
        consensus: SharedConsensusGraph, storage_manager: Arc<StorageManager>,
        txpool: SharedTransactionPool, secret_store: SharedSecretStore,
        key_pair: Option<KeyPair>,
    ) -> Self
    {
        TransactionGenerator {
            consensus,
            storage_manager,
            txpool,
            secret_store,
            state: RwLock::new(TransGenState::Start),
            key_pair,
        }
    }

    pub fn stop(&self) { *self.state.write() = TransGenState::Stop; }

    pub fn get_best_state_at(&self, block_hash: &H256) -> State {
        State::new(
            StateDb::new(
                self.storage_manager
                    .get_state_at(block_hash.clone())
                    .unwrap(),
            ),
            0.into(),
            Default::default(),
        )
    }

    pub fn get_best_state(&self) -> State {
        self.get_best_state_at(&self.consensus.best_state_block_hash())
    }

    pub fn generate_transaction(&self) -> SignedTransaction {
        // Generate new address with 10% probability
        let is_send_to_new_address = (rand::thread_rng().gen_range(0, 10) == 0)
            || (self.secret_store.count() < 10);
        let receiver_address = match is_send_to_new_address {
            false => {
                let account_count = self.secret_store.count();
                let index: usize = random::<usize>() % account_count;
                let kp = self.secret_store.get_keypair(index);
                public_to_address(kp.public())
            }
            true => {
                let kp = Random.generate().expect("Fail to generate KeyPair.");
                self.secret_store.insert(kp.clone());
                public_to_address(kp.public())
            }
        };

        let account_count = self.secret_store.count();
        let sender_index: usize = random::<usize>() % account_count;
        let sender_kp = self.secret_store.get_keypair(sender_index);
        let sender_address = public_to_address(sender_kp.public());

        let state = self.get_best_state();

        debug!(
            "account_count:{} sender_addr:{:?} epoch_id:{:?}",
            account_count,
            sender_address,
            self.consensus.best_state_block_hash()
        );
        let sender_balance = state.balance(&sender_address).unwrap_or(0.into());

        let sender_nonce = state.nonce(&sender_address).unwrap_or(0.into());

        let mut balance_to_transfer: U256 = 0.into();
        if sender_balance > 0.into() {
            balance_to_transfer = U256::from(
                U512::from(H512::random()) % U512::from(sender_balance),
            );
        }

        let tx = Transaction {
            nonce: sender_nonce,
            gas_price: U256::from(100u64),
            gas: U256::from(1u64),
            value: balance_to_transfer,
            action: Action::Call(receiver_address),
            data: Bytes::new(),
        };
        let r = tx.sign(sender_kp.secret());
        r
    }

    pub fn generate_transactions(
        txgen: Arc<TransactionGenerator>, tx_config: TransactionGeneratorConfig,
    ) -> Result<(), Error> {
        let account_count = tx_config.account_count;
        let mut nonce_map: HashMap<Address, U256> = HashMap::new();
        let mut balance_map: HashMap<Address, U256> = HashMap::new();

        let initial_key_pair = txgen.key_pair.clone().expect("should exist");
        let secret_store = SecretStore::new();
        debug!(
            "tx_gen address={:?} pub_key={:?}",
            public_to_address(initial_key_pair.public()),
            initial_key_pair.public()
        );
        debug!(
            "Tx Generation Config {:?} {:?}",
            tx_config.generate_tx, tx_config.period
        );
        secret_store.insert(initial_key_pair.clone());
        let mut tx_n = 0;
        // Wait for initial tx
        loop {
            let state = State::new(
                StateDb::new(
                    txgen
                        .storage_manager
                        .get_state_at(txgen.consensus.best_state_block_hash())
                        .unwrap(),
                ),
                0.into(),
                Default::default(),
            );
            let sender_address = initial_key_pair.address();
            let sender_balance = state.balance(&sender_address).ok();
            if sender_balance.is_none()
                || sender_balance.clone().unwrap() == 0.into()
            {
                thread::sleep(Duration::from_millis(100));
                continue;
            } else {
                balance_map
                    .insert(sender_address.clone(), sender_balance.unwrap());
                nonce_map.insert(sender_address, 0.into());
                break;
            }
        }
        debug!("Get initial transaction");
        let mut last_account = None;
        let mut wait_count = 0;
        // Setup accounts
        loop {
            match *txgen.state.read() {
                TransGenState::Stop => return Ok(()),
                _ => {}
            }
            if secret_store.count() < account_count {
                let mut receiver_kp: KeyPair;
                let sender_address = initial_key_pair.address();
                let sender_balance =
                    balance_map.get_mut(&sender_address).unwrap();
                let balance_to_transfer = *sender_balance / account_count;
                // Create a new receiver account
                loop {
                    receiver_kp = Random.generate()?;
                    if secret_store.insert(receiver_kp.clone()) {
                        nonce_map.insert(receiver_kp.address(), 0.into());
                        break;
                    }
                }
                *sender_balance -= balance_to_transfer + 21000;
                // Generate nonce for the transaction
                let sender_nonce =
                    nonce_map.get_mut(&initial_key_pair.address()).unwrap();
                let receiver_address = public_to_address(receiver_kp.public());
                *balance_map.entry(receiver_address).or_insert(0.into()) +=
                    balance_to_transfer;
                // Generate the transaction, sign it, and push into the
                // transaction pool
                let tx = Transaction {
                    nonce: *sender_nonce,
                    gas_price: U256::from(1u64),
                    gas: U256::from(21000u64),
                    value: balance_to_transfer,
                    action: Action::Call(receiver_address.clone()),
                    data: Bytes::new(),
                };
                *sender_nonce += U256::one();
                let signed_tx = tx.sign(initial_key_pair.secret());
                let mut tx_to_insert = Vec::new();
                tx_to_insert.push(signed_tx.transaction);
                txgen.txpool.insert_new_transactions(
                    txgen.consensus.best_state_block_hash(),
                    &tx_to_insert,
                );
                last_account = Some(receiver_address);
            } else {
                // Wait for preparation
                let state = State::new(
                    StateDb::new(
                        txgen
                            .storage_manager
                            .get_state_at(
                                txgen.consensus.best_state_block_hash(),
                            )
                            .unwrap(),
                    ),
                    0.into(),
                    Default::default(),
                );
                let sender_balance = state.balance(&last_account.unwrap()).ok();
                // Wait for at most 200*0.1=20 seconds
                if wait_count < 200
                    && (sender_balance.is_none()
                        || sender_balance.clone().unwrap() == 0.into())
                {
                    wait_count += 1;
                    thread::sleep(Duration::from_millis(100));
                    continue;
                } else {
                    break;
                }
            }
        }

        info!("Start Generating Workload");
        let start_time = Instant::now();
        // Generate more tx
        loop {
            match *txgen.state.read() {
                TransGenState::Stop => return Ok(()),
                _ => {}
            }

            let (sender_kp, receiver_kp, balance_to_transfer) =
                if secret_store.count() < account_count {
                    let mut receiver_kp: KeyPair;
                    let sender_address = initial_key_pair.address();
                    let sender_balance =
                        balance_map.get_mut(&sender_address).unwrap();
                    let balance_to_transfer = *sender_balance / account_count;
                    // Create a new receiver account
                    loop {
                        receiver_kp = Random.generate()?;
                        if secret_store.insert(receiver_kp.clone()) {
                            nonce_map.insert(receiver_kp.address(), 0.into());
                            break;
                        }
                    }
                    *sender_balance -= balance_to_transfer + 21000;
                    (initial_key_pair.clone(), receiver_kp, balance_to_transfer)
                } else {
                    // Randomly select sender and receiver.
                    // Sender must exist in the account list.
                    // Receiver can be not in the account list which
                    // leads to generate a new account
                    let account_count = secret_store.count();
                    let mut receiver_index: usize = random();
                    receiver_index %= account_count;
                    let receiver_kp = secret_store.get_keypair(receiver_index);
                    let mut sender_index: usize = random();
                    sender_index %= account_count;
                    let sender_kp = secret_store.get_keypair(sender_index);

                    // Randomly generate the to-be-transferred value
                    // based on the balance of sender
                    let sender_address = public_to_address(sender_kp.public());
                    let sender_balance =
                        balance_map.get_mut(&sender_address).unwrap();

                    if *sender_balance < 42000.into() {
                        secret_store.remove_keypair(sender_index);
                        if secret_store.count() == 0 {
                            break;
                        }
                        continue;
                    }
                    let balance_to_transfer = U256::from(0);
                    *sender_balance -= balance_to_transfer + 21000;
                    (sender_kp, receiver_kp, balance_to_transfer)
                };
            // Generate nonce for the transaction
            let sender_nonce = nonce_map.get_mut(&sender_kp.address()).unwrap();
            let receiver_address = public_to_address(receiver_kp.public());
            trace!(
                "receiver={:?} value={:?} nonce={:?}",
                receiver_address,
                balance_to_transfer,
                sender_nonce
            );
            *balance_map.entry(receiver_address).or_insert(0.into()) +=
                balance_to_transfer;
            // Generate the transaction, sign it, and push into the transaction
            // pool
            let tx = Transaction {
                nonce: *sender_nonce,
                gas_price: U256::from(1u64),
                gas: U256::from(21000u64),
                value: balance_to_transfer,
                action: Action::Call(receiver_address),
                data: Bytes::new(),
            };
            *sender_nonce += U256::one();

            let signed_tx = tx.sign(sender_kp.secret());
            let mut tx_to_insert = Vec::new();
            tx_to_insert.push(signed_tx.transaction);
            txgen.txpool.insert_new_transactions(
                txgen.consensus.best_state_block_hash(),
                &tx_to_insert,
            );
            tx_n += 1;
            if tx_n % 100 == 0 {
                info!("Generated {} transactions", tx_n);
            }
            let now = Instant::now();
            let time_elapsed = now.duration_since(start_time);
            if let Some(time_left) =
                (tx_config.period * tx_n).checked_sub(time_elapsed)
            {
                thread::sleep(time_left);
            } else {
                debug!("Elapsed time larger than the time needed for sleep: time_elapsed={:?}", time_elapsed);
            }
        }
        Ok(())
    }
}

pub struct SpecialTransactionGenerator {
    // Key, simple tx, erc20 balance, array index.
    accounts: HashMap<Address, (KeyPair, Account, U256)>,
    address_by_index: Vec<Address>,
    erc20_address: Address,
}

// Allow use of hex() in H256, etc.
#[allow(deprecated)]
impl SpecialTransactionGenerator {
    const MAX_TOTAL_ACCOUNTS: usize = 100000;

    pub fn new(
        start_key_pair: KeyPair, contract_creator: &Address,
        start_balance: U256, start_erc20_balance: U256,
    ) -> SpecialTransactionGenerator
    {
        let start_address = public_to_address(start_key_pair.public());
        let info = (
            start_key_pair,
            Account::new_empty_with_balance(
                &start_address,
                &start_balance,
                &0.into(),
            ),
            start_erc20_balance,
        );
        let mut accounts = HashMap::<Address, (KeyPair, Account, U256)>::new();
        accounts.insert(start_address.clone(), info);
        let address_by_index = vec![start_address.clone()];

        let erc20_address = contract_address(
            CreateContractAddress::FromSenderAndNonce,
            &contract_creator,
            &0.into(),
            &[],
        )
        .0;

        debug!(
            "Special Transaction Generator: erc20 contract address: {:?}",
            erc20_address
        );
        assert_eq!(
            erc20_address.hex(),
            "0xe2182fba747b5706a516d6cf6bf62d6117ef86ea"
        );

        SpecialTransactionGenerator {
            accounts,
            address_by_index,
            erc20_address,
        }
    }

    pub fn generate_transactions(
        &mut self, block_size_limit: &mut usize, mut num_txs_simple: usize,
        mut num_txs_erc20: usize,
    ) -> Vec<Arc<SignedTransaction>>
    {
        let mut result = vec![];
        // Generate new address with 10% probability
        while num_txs_simple > 0 {
            let number_of_accounts = self.address_by_index.len();

            let sender_index: usize = random::<usize>() % number_of_accounts;
            let sender_address =
                self.address_by_index.get(sender_index).unwrap().clone();
            let sender_kp;
            let sender_balance;
            let sender_nonce;
            {
                let sender_info = self.accounts.get(&sender_address).unwrap();
                sender_kp = sender_info.0.clone();
                sender_balance = sender_info.1.balance;
                sender_nonce = sender_info.1.nonce;
            }

            let gas = U256::from(100000u64);
            let gas_price = U256::from(1u64);
            let transaction_fee = U256::from(100000u64);

            if sender_balance <= transaction_fee {
                self.accounts.remove(&sender_address);
                self.address_by_index.swap_remove(sender_index);
                continue;
            }

            let balance_to_transfer = U256::from(
                U512::from(H512::random()) % U512::from(sender_balance),
            );

            let is_send_to_new_address = (number_of_accounts
                <= Self::MAX_TOTAL_ACCOUNTS)
                && ((number_of_accounts < 10)
                    || (rand::thread_rng().gen_range(0, 10) == 0));

            let receiver_address = match is_send_to_new_address {
                false => {
                    let index: usize = random::<usize>() % number_of_accounts;
                    self.address_by_index.get(index).unwrap().clone()
                }
                true => loop {
                    let kp =
                        Random.generate().expect("Fail to generate KeyPair.");
                    let address = public_to_address(kp.public());
                    if self.accounts.get(&address).is_none() {
                        self.accounts.insert(
                            address,
                            (
                                kp,
                                Account::new_empty_with_balance(
                                    &address,
                                    &0.into(),
                                    &0.into(),
                                ),
                                0.into(),
                            ),
                        );
                        self.address_by_index.push(address.clone());

                        break address;
                    }
                },
            };

            let tx = Transaction {
                nonce: sender_nonce,
                gas_price,
                gas,
                value: balance_to_transfer,
                action: Action::Call(receiver_address),
                data: vec![0u8; 128],
            };
            let signed_transaction = tx.sign(sender_kp.secret());
            let rlp_size = signed_transaction.transaction.rlp_bytes().len();
            if *block_size_limit <= rlp_size {
                break;
            }
            *block_size_limit -= rlp_size;

            self.accounts.get_mut(&sender_address).unwrap().1.balance -=
                balance_to_transfer;
            self.accounts.get_mut(&sender_address).unwrap().1.nonce += 1.into();
            self.accounts.get_mut(&receiver_address).unwrap().1.balance +=
                balance_to_transfer;

            result.push(Arc::new(signed_transaction));

            num_txs_simple -= 1;
        }

        while num_txs_erc20 > 0 {
            let number_of_accounts = self.address_by_index.len();

            let sender_index: usize = random::<usize>() % number_of_accounts;
            let sender_address =
                self.address_by_index.get(sender_index).unwrap().clone();
            let sender_kp;
            let sender_balance;
            let sender_erc20_balance;
            let sender_nonce;
            {
                let sender_info = self.accounts.get(&sender_address).unwrap();
                sender_kp = sender_info.0.clone();
                sender_balance = sender_info.1.balance;
                sender_erc20_balance = sender_info.2.clone();
                sender_nonce = sender_info.1.nonce;
            }

            let gas = U256::from(100000u64);
            let gas_price = U256::from(1u64);
            let transaction_fee = U256::from(100000u64);

            if sender_balance <= transaction_fee {
                self.accounts.remove(&sender_address);
                self.address_by_index.swap_remove(sender_index);
                continue;
            }

            let balance_to_transfer = if sender_erc20_balance == 0.into() {
                continue;
            } else {
                U256::from(
                    U512::from(H512::random())
                        % U512::from(sender_erc20_balance),
                )
            };

            let receiver_index = random::<usize>() % number_of_accounts;
            let receiver_address =
                self.address_by_index.get(receiver_index).unwrap().clone();

            if receiver_index == sender_index {
                continue;
            }

            // Calls transfer of ERC20 contract.
            let tx_data = Vec::from_hex(
                String::new()
                    + "a9059cbb000000000000000000000000"
                    + &receiver_address.hex()[2..]
                    + &H256::from(balance_to_transfer).hex()[2..],
            )
            .unwrap();

            let tx = Transaction {
                nonce: sender_nonce,
                gas_price,
                gas,
                value: 0.into(),
                action: Action::Call(self.erc20_address.clone()),
                data: tx_data,
            };
            let signed_transaction = tx.sign(sender_kp.secret());
            let rlp_size = signed_transaction.transaction.rlp_bytes().len();
            if *block_size_limit <= rlp_size {
                break;
            }
            *block_size_limit -= rlp_size;

            self.accounts.get_mut(&sender_address).unwrap().2 -=
                balance_to_transfer;
            self.accounts.get_mut(&sender_address).unwrap().1.nonce += 1.into();
            self.accounts.get_mut(&receiver_address).unwrap().2 +=
                balance_to_transfer;

            result.push(Arc::new(signed_transaction));

            num_txs_erc20 -= 1;
        }

        result
    }
}
