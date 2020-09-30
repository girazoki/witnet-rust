use std::{
    collections::HashMap,
    convert::TryFrom,
    str::FromStr,
    sync::{Arc, RwLock},
};

use state::State;

use witnet_crypto::hash::calculate_sha256;
use witnet_data_structures::{
    chain::{CheckpointBeacon, Environment, Epoch, EpochConstants, PublicKeyHash},
    get_environment,
};
use witnet_util::timestamp::get_timestamp;

use crate::{
    constants,
    db::{Database, WriteBatch as _},
    model,
    params::Params,
    types::{self, signature, ExtendedPK, Hash, Hashable as _, RadonError},
};

use super::*;

mod state;
#[cfg(test)]
mod tests;

/// Internal structure used to gather state mutations while indexing block transactions
struct AccountMutation {
    balance_movement: model::BalanceMovement,
    utxo_inserts: Vec<(model::OutPtr, model::KeyBalance)>,
    utxo_removals: Vec<model::OutPtr>,
}

pub struct Wallet<T> {
    pub id: String,
    pub session_id: types::SessionId,
    db: T,
    params: Params,
    engine: types::CryptoEngine,
    state: RwLock<State>,
}

impl<T> Wallet<T>
where
    T: Database,
{
    /// Returns the bootstrap hash consensus constant
    pub fn get_bootstrap_hash(&self) -> Hash {
        self.params.genesis_prev_hash
    }

    /// Returns the superblock period consensus constant
    pub fn get_superblock_period(&self) -> u16 {
        self.params.superblock_period
    }

    /// Clears local pending wallet state to match the persisted state in database
    pub fn clear_pending_state(&self) -> Result<()> {
        let account = 0;

        let mut state = self.state.write()?;

        state.last_sync = state.last_confirmed;
        state.pending_blocks.clear();
        state.pending_movements.clear();
        state.pending_addresses_by_path.clear();
        state.pending_addresses_by_block.clear();
        state.local_movements.clear();

        // Restore state from database
        state.transaction_next_id = self
            .db
            .get_or_default::<_, u32>(&keys::transaction_next_id(account))?;
        state.next_external_index = self.db.get_or_default(&keys::account_next_index(
            account,
            constants::EXTERNAL_KEYCHAIN,
        ))?;
        state.next_internal_index = self.db.get_or_default(&keys::account_next_index(
            account,
            constants::INTERNAL_KEYCHAIN,
        ))?;
        state.utxo_set = self.db.get_or_default(&keys::account_utxo_set(account))?;
        state.balance.confirmed = self.db.get_or_default(&keys::account_balance(account))?;
        state.balance.unconfirmed = state.balance.confirmed;

        Ok(())
    }

    pub fn unlock(
        id: &str,
        session_id: types::SessionId,
        db: T,
        params: Params,
        engine: types::CryptoEngine,
    ) -> Result<Self> {
        let id = id.to_owned();
        let name = db.get_opt(keys::wallet_name())?;
        let caption = db.get_opt(keys::wallet_caption())?;
        let account = db.get_or_default(keys::wallet_default_account())?;
        let available_accounts = db
            .get_opt(keys::wallet_accounts())?
            .unwrap_or_else(|| vec![account]);

        let transaction_next_id = db.get_or_default(&keys::transaction_next_id(account))?;
        let utxo_set: model::UtxoSet = db.get_or_default(&keys::account_utxo_set(account))?;
        let timestamp =
            u64::try_from(get_timestamp()).expect("Get timestamp should return a positive value");
        let balance_info = db
            .get_opt(&keys::account_balance(account))?
            .unwrap_or_else(|| {
                // compute balance from utxo set if is not cached in the
                // database, this is mostly used for testing where overflow
                // checks are enabled
                utxo_set
                    .iter()
                    .map(|(_, balance)| (balance.amount, balance.time_lock))
                    .fold(
                        model::BalanceInfo::default(),
                        |mut acc, (amount, time_lock)| {
                            if timestamp >= time_lock {
                                acc.available =
                                    acc.available.checked_add(amount).expect("balance overflow");
                            } else {
                                acc.locked =
                                    acc.locked.checked_add(amount).expect("balance overflow");
                            }

                            acc
                        },
                    )
            });
        let balance = model::WalletBalance {
            local: 0,
            unconfirmed: balance_info,
            confirmed: balance_info,
        };

        let last_sync = db
            .get(&keys::wallet_last_sync())
            .unwrap_or_else(|_| CheckpointBeacon {
                checkpoint: 0,
                hash_prev_block: params.genesis_prev_hash,
            });
        let last_confirmed = last_sync;

        let external_key = db.get(&keys::account_key(account, constants::EXTERNAL_KEYCHAIN))?;
        let next_external_index = db.get_or_default(&keys::account_next_index(
            account,
            constants::EXTERNAL_KEYCHAIN,
        ))?;
        let internal_key = db.get(&keys::account_key(account, constants::INTERNAL_KEYCHAIN))?;
        let next_internal_index = db.get_or_default(&keys::account_next_index(
            account,
            constants::INTERNAL_KEYCHAIN,
        ))?;
        let keychains = [external_key, internal_key];
        let epoch_constants = params.epoch_constants;

        let state = RwLock::new(State {
            name,
            caption,
            account,
            keychains,
            next_external_index,
            next_internal_index,
            available_accounts,
            balance,
            transaction_next_id,
            utxo_set,
            epoch_constants,
            last_sync,
            last_confirmed,
            local_movements: Default::default(),
            pending_movements: Default::default(),
            pending_addresses_by_block: Default::default(),
            pending_addresses_by_path: Default::default(),
            pending_blocks: Default::default(),
            pending_dr_movements: Default::default(),
        });

        Ok(Self {
            id,
            session_id,
            db,
            params,
            engine,
            state,
        })
    }

    /// Return all non-sensitive data regarding the wallet.
    pub fn public_data(&self) -> Result<types::WalletData> {
        let state = self.state.read()?;
        let current_account = state.account;
        let balance = state.balance;
        let last_sync = state.last_sync;
        let last_confirmed = state.last_confirmed;

        Ok(types::WalletData {
            id: self.id.clone(),
            name: state.name.clone(),
            caption: state.caption.clone(),
            balance,
            current_account,
            available_accounts: state.available_accounts.clone(),
            last_sync,
            last_confirmed,
        })
    }

    /// Generic method for generating an address.
    ///
    /// See `gen_internal_address` and `gen_external_address` for more
    /// concrete implementations.
    pub fn gen_address(
        &self,
        label: Option<String>,
        parent_key: &types::ExtendedSK,
        account: u32,
        keychain: u32,
        index: u32,
    ) -> Result<(Arc<model::Address>, u32)> {
        let next_index = index.checked_add(1).ok_or_else(|| Error::IndexOverflow)?;

        let extended_sk =
            parent_key.derive(&self.engine, &types::KeyPath::default().index(index))?;
        let types::ExtendedPK { key, .. } =
            types::ExtendedPK::from_secret_key(&self.engine, &extended_sk);

        let pkh = witnet_data_structures::chain::PublicKey::from(key).pkh();
        let address = pkh.bech32(get_environment());
        let path = model::Path {
            account,
            keychain,
            index,
        }
        .to_string();
        let info = model::AddressInfo {
            db_key: keys::address_info(account, keychain, index),
            label,
            received_payments: vec![],
            received_amount: 0,
            first_payment_date: None,
            last_payment_date: None,
        };

        // Persist changes and new address in database
        let mut batch = self.db.batch();

        batch.put(keys::address(account, keychain, index), &address)?;
        batch.put(keys::address_path(account, keychain, index), &path)?;
        batch.put(keys::address_pkh(account, keychain, index), &pkh)?;
        batch.put(&info.db_key, &info)?;
        batch.put(
            keys::pkh(&pkh),
            &model::Path {
                account,
                keychain,
                index,
            },
        )?;
        batch.put(keys::account_next_index(account, keychain), &next_index)?;

        self.db.write(batch)?;

        let address = model::Address {
            address,
            path,
            info,
            index,
            account,
            keychain,
            pkh,
        };

        Ok((Arc::new(address), next_index))
    }

    /// Generate an address in the external keychain (WIP-0001).
    pub fn gen_external_address(&self, label: Option<String>) -> Result<Arc<model::Address>> {
        let mut state = self.state.write()?;

        self._gen_external_address(&mut state, label)
    }

    /// Generate an address in the internal keychain (WIP-0001).
    #[cfg(test)]
    pub fn gen_internal_address(&self, label: Option<String>) -> Result<Arc<model::Address>> {
        let mut state = self.state.write()?;

        self._gen_internal_address(&mut state, label)
    }

    /// Return a list of the generated external addresses that.
    pub fn external_addresses(&self, offset: u32, limit: u32) -> Result<model::Addresses> {
        let keychain = constants::EXTERNAL_KEYCHAIN;
        let state = self.state.read()?;
        let account = state.account;
        let total = state.next_external_index;

        let end = total.saturating_sub(offset);
        let start = end.saturating_sub(limit);
        let range = start..end;
        let mut addresses = Vec::with_capacity(range.len());

        log::debug!(
            "Retrieving addresses in range {:?}. Start({}), End({}), Total({})",
            range,
            start,
            end,
            total
        );
        for index in range.rev() {
            let address = self.get_address(account, keychain, index)?;
            addresses.push((*address).clone());
        }

        Ok(model::Addresses { addresses, total })
    }

    /// Return a list of the transactions.
    pub fn transactions(&self, offset: u32, limit: u32) -> Result<model::Transactions> {
        let state = self.state.read()?;
        let account = state.account;

        // Total amount of state and db transactions
        let total = state.transaction_next_id + u32::try_from(state.local_movements.len()).unwrap();
        let mut transactions: Vec<model::BalanceMovement> = Vec::with_capacity(total as usize);

        // Prepend local pending balance movements (not yet included in blocks)
        let local_movements = state.local_movements.values().cloned().collect::<Vec<_>>();
        transactions.extend_from_slice(&local_movements);

        // Prepend balance movements of pending blocks
        state.pending_movements.values().for_each(|x| {
            transactions.extend_from_slice(x);
        });

        // Query database `transaction_next_id` to compute total amount of transactions
        let db_total = self
            .db
            .get_or_default::<_, u32>(&keys::transaction_next_id(account))?;

        if db_total > 0 {
            let end = db_total.saturating_sub(offset);
            let start = end.saturating_sub(limit);
            let range = start..end;

            log::debug!(
                "Retrieving database transactions in range {:?}. Start({}), End({}), Total({})",
                range,
                start,
                end,
                db_total,
            );
            for index in range.rev() {
                match self.get_transaction(account, index) {
                    Ok(transaction) => {
                        transactions.push(transaction);
                    }
                    Err(e) => {
                        log::error!(
                            "Error while retrieving transaction with index {}: {}",
                            index,
                            e
                        );
                    }
                }
            }
        }

        Ok(model::Transactions {
            transactions,
            total,
        })
    }

    /// Get an address if it exists in memory or storage.
    pub fn get_address(
        &self,
        account: u32,
        keychain: u32,
        index: u32,
    ) -> Result<Arc<model::Address>> {
        let state = self.state.read()?;

        self._get_address(&state, account, keychain, index)
    }

    /// Non-locking version of `get_address` (requires a reference to `State` to be passed as
    /// argument instead of taking a read lock on `self.state`, so as to avoid deadlocks).
    pub fn _get_address(
        &self,
        state: &State,
        account: u32,
        keychain: u32,
        index: u32,
    ) -> Result<Arc<model::Address>> {
        let path = model::Path {
            account,
            keychain,
            index,
        }
        .to_string();

        if let Some(address) = state.pending_addresses_by_path.get(&path) {
            log::trace!("Address {} found in memory", path);
            Ok(address.clone())
        } else {
            log::trace!(
                "Address {} not found in memory, looking for it in storage...",
                path,
            );
            let address = self.db.get(&keys::address(account, keychain, index))?;
            let pkh = self.db.get(&keys::address_pkh(account, keychain, index))?;
            let info = self.db.get(&keys::address_info(account, keychain, index))?;

            Ok(Arc::new(model::Address {
                address,
                path,
                pkh,
                index,
                account,
                keychain,
                info,
            }))
        }
    }

    /// Get a transaction if exists.
    pub fn get_transaction(&self, account: u32, index: u32) -> Result<model::BalanceMovement> {
        Ok(self
            .db
            .get::<_, model::BalanceMovement>(&keys::transaction_movement(account, index))?)
    }

    /// Get a previously put serialized value.
    ///
    /// See `kv_set`.
    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        let value = self.db.get_opt(&keys::custom(key))?;

        Ok(value)
    }

    /// Set an arbitrary string value under a custom key.
    ///
    /// See `kv_get`.
    pub fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        self.db.put(&keys::custom(key), value)?;

        Ok(())
    }

    /// Update a wallet's name and/or caption
    pub fn update(&self, name: Option<String>, caption: Option<String>) -> Result<()> {
        let mut batch = self.db.batch();
        let mut state = self.state.write()?;

        state.name = name;
        if let Some(ref name) = state.name {
            batch.put(keys::wallet_name(), name)?;
        }

        state.caption = caption;
        if let Some(ref caption) = state.caption {
            batch.put(keys::wallet_caption(), caption)?;
        }

        self.db.write(batch)?;

        Ok(())
    }

    /// Filter transactions in a block (received from a node) if they belong to wallet accounts.
    pub fn filter_wallet_transactions(
        &self,
        txns: &[types::Transaction],
    ) -> Result<Vec<types::Transaction>> {
        let state = self.state.read()?;

        let mut filtered_txns = vec![];
        for txn in txns {
            // Inputs and outputs from different transaction types
            let (inputs, outputs): (&[types::TransactionInput], &[types::VttOutput]) = match txn {
                types::Transaction::ValueTransfer(vt) => (&vt.body.inputs, &vt.body.outputs),
                types::Transaction::DataRequest(dr) => (&dr.body.inputs, &dr.body.outputs),
                types::Transaction::Commit(commit) => {
                    (&commit.body.collateral, &commit.body.outputs)
                }
                types::Transaction::Tally(tally) => (&[], &tally.outputs),
                types::Transaction::Mint(mint) => (&[], &mint.outputs),
                _ => continue,
            };

            // Check if tally txn corresponds to a wallet sent data request
            if let types::Transaction::Tally(tally) = &txn {
                if self
                    .db
                    .get::<_, model::Path>(&keys::transactions_index(tally.dr_pointer.as_ref()))
                    .is_ok()
                {
                    filtered_txns.push(txn.clone());
                    continue;
                }
                else if let Some(_dr_tx) = state.pending_dr_movements.get(&tally.dr_pointer) {
                        filtered_txns.push(txn.clone());
                        continue;
                }
                /*else {
                    let push_needed = state.pending_blocks.iter().any(|&(block_hash, _)| {
                        if let Some(balance_m{ovements) = state.pending_movements.get(block_hash) {
                            balance_movements.iter().any(|&balance_movement| {
                                balance_movement.transaction.hash == tally.dr_pointer.to_string()
                            })
                        }
                        else {
                            false
                        }
                    });
                    if push_needed {
                        filtered_txns.push(txn.clone());
                        continue;
                    }
                }*/
            }

            let check_db = |output: &types::VttOutput| {
                self.db
                    .get::<_, model::Path>(&keys::pkh(&output.pkh))
                    .is_ok()
            };
            // Check if any input or output is from the wallet (input is an UTXO or output points to any wallet's pkh)
            if inputs
                .iter()
                .any(|input| state.utxo_set.get(&input.output_pointer().into()).is_some())
                || outputs.iter().any(check_db)
            {
                filtered_txns.push(txn.clone());
            }
        }

        Ok(filtered_txns)
    }

    /// Index transactions in a block received from a node.
    pub fn index_block_transactions(
        &self,
        block_info: &model::Beacon,
        txns: &[model::ExtendedTransaction],
        confirmed: bool,
    ) -> Result<Vec<model::BalanceMovement>> {
        let mut state = self.state.write()?;
        let mut addresses = Vec::new();
        let mut balance_movements = Vec::new();
        let mut dr_balance_movements = HashMap::new();

        // Index all transactions
        for txn in txns {
            // Check if transaction already exists in the database
            let hash = txn.transaction.hash().as_ref().to_vec();
            match self
                .db
                .get_opt::<_, u32>(&keys::transactions_index(&hash))?
            {
                None => match self._index_transaction(&mut state, txn, block_info, confirmed) {
                    Ok(Some((balance_movement, mut new_addresses))) => {
                        if let types::Transaction::DataRequest(dr_tx) = &txn.transaction {
                            dr_balance_movements.insert(dr_tx.hash(),(block_info.block_hash, balance_movements.len()));
                        }
                        balance_movements.push(balance_movement);
                        addresses.append(&mut new_addresses);
                    }
                    Ok(None) => {}
                    e @ Err(_) => {
                        e?;
                    }
                },
                Some(_) => log::warn!(
                    "The transaction {} already exists in the database",
                    txn.transaction.hash()
                ),
            }
            if let types::Transaction::Tally(tally) = &txn.transaction {
                // esta en la base de datos
                if let Ok((dr_movement, txn_id)) = self
                    .db
                    .get::<_, u32>(&keys::transactions_index(tally.dr_pointer.as_ref()))
                    .and_then(|txn_id| {
                        self.db
                            .get::<_, model::BalanceMovement>(&keys::transaction_movement(
                                state.account,
                                txn_id,
                            ))
                            .map(|dr_movement| (dr_movement, txn_id))
                    })
                {
                    match &dr_movement.transaction.data {
                        model::TransactionData::DataRequest(dr_data) => {
                            let mut updated_dr_movement = dr_movement.clone();
                            updated_dr_movement.transaction.data = model::TransactionData::DataRequest(model::DrData {
                                inputs: dr_data.inputs.clone(),
                                outputs: dr_data.outputs.clone(),
                                tally: Some(build_tally_report(tally, &txn.metadata)?),
                            });updated_dr_movement.db_key = txn_id;
                            balance_movements.push(updated_dr_movement);
                        }
                        _ => log::warn!("data request tally update failed because wrong transaction type (txn: {})", tally.dr_pointer),
                    }
                }
                    else if let Some((pending_block_hash, index)) = state.pending_dr_movements.get(&tally.dr_pointer).cloned() {
                        let dr_movement = state.pending_movements.get(&pending_block_hash.to_string()).unwrap()[index].clone();
                        match &dr_movement.transaction.data {
                            model::TransactionData::DataRequest(dr_data) => {
                                let mut updated_dr_movement = dr_movement.clone();
                                updated_dr_movement.transaction.data = model::TransactionData::DataRequest(model::DrData {
                                    inputs: dr_data.inputs.clone(),
                                    outputs: dr_data.outputs.clone(),
                                    tally: Some(build_tally_report(tally, &txn.metadata)?),
                                });
                                state.pending_movements.get_mut(&pending_block_hash.to_string()).unwrap()[index] = updated_dr_movement;
                                state.pending_dr_movements.remove(&tally.dr_pointer);
                            }
                            _ => log::warn!("data request tally update failed because wrong transaction type (txn: {})", tally.dr_pointer),
                        }
                    }
                else {
                    log::debug!(
                        "data request tally update not required it was not found (txn: {})",
                        tally.dr_pointer
                    )
                }
            }
        }

        let timestamp = convert_block_epoch_to_timestamp(state.epoch_constants, block_info.epoch);
        state.balance.unconfirmed = state
            .utxo_set
            .iter()
            .map(|(_, balance)| (balance.amount, balance.time_lock))
            .fold(
                model::BalanceInfo::default(),
                |mut acc, (amount, time_lock)| {
                    if timestamp > time_lock {
                        acc.available =
                            acc.available.checked_add(amount).expect("balance overflow");
                    } else {
                        acc.locked = acc.locked.checked_add(amount).expect("balance overflow");
                    }

                    acc
                },
            );

        // Persist into database
        if confirmed {
            self._persist_block_txns(
                balance_movements.clone(),
                addresses,
                state.transaction_next_id,
                state.next_external_index,
                state.next_internal_index,
                state.utxo_set.clone(),
                &state.balance.unconfirmed,
                block_info,
            )?
        } else {
            for address in &addresses {
                let path = address.path.clone();
                state
                    .pending_addresses_by_path
                    .insert(path, address.clone());
            }

            // Build wallet state after block index
            let block_state = state::StateSnapshot {
                balance: state.balance.unconfirmed,
                beacon: block_info.clone(),
                transaction_next_id: state.transaction_next_id,
                utxo_set: state.utxo_set.clone(),
            };

            state
                .pending_blocks
                .insert(block_info.block_hash.to_string(), block_state);

            state
                .pending_movements
                .insert(block_info.block_hash.to_string(), balance_movements.clone());
            state
                .pending_dr_movements
                .extend(dr_balance_movements);
            state
                .pending_addresses_by_block
                .insert(block_info.block_hash.to_string(), addresses);
        }

        Ok(balance_movements)
    }

    #[allow(clippy::too_many_arguments)]
    fn _persist_block_txns(
        &self,
        balance_movements: Vec<model::BalanceMovement>,
        addresses: Vec<Arc<model::Address>>,
        transaction_next_id: u32,
        next_external_address_index: u32,
        next_internal_address_index: u32,
        utxo_set: model::UtxoSet,
        balance: &model::BalanceInfo,
        block_info: &model::Beacon,
    ) -> Result<()> {
        log::debug!(
            "Persisting block #{} changes: {} balance movements and {} address changes",
            block_info.epoch,
            balance_movements.len(),
            addresses.len(),
        );

        let account = 0;
        let mut batch = self.db.batch();

        // Write transactional data (index, hash and balance movement)
        for mut movement in balance_movements {
            let txn_hash = types::Hash::from_str(&movement.transaction.hash)?;
            movement.transaction.confirmed = true;
            batch.put(
                keys::transactions_index(txn_hash.as_ref()),
                &movement.db_key,
            )?;
            batch.put(
                keys::transaction_hash(account, movement.db_key).into_bytes(),
                txn_hash.as_ref(),
            )?;
            batch.put(
                keys::transaction_movement(account, movement.db_key).into_bytes(),
                &movement,
            )?;
        }

        // Write account state
        batch.put(
            keys::transaction_next_id(account).into_bytes(),
            transaction_next_id,
        )?;
        batch.put(keys::account_utxo_set(account).into_bytes(), utxo_set)?;
        batch.put(keys::account_balance(account).into_bytes(), balance)?;
        batch.put(
            keys::account_next_index(account, constants::EXTERNAL_KEYCHAIN),
            next_external_address_index,
        )?;
        batch.put(
            keys::account_next_index(account, constants::INTERNAL_KEYCHAIN),
            next_internal_address_index,
        )?;

        // Persist addresses
        for address in addresses {
            batch.put(&address.info.db_key, &address.info)?;
            batch.put(
                keys::address(account, address.keychain, address.index),
                &address.address,
            )?;
            batch.put(
                keys::address_path(account, address.keychain, address.index),
                &address.path,
            )?;
            batch.put(
                keys::address_pkh(account, address.keychain, address.index),
                &address.pkh,
            )?;
        }

        // FIXME(#1539): persist update of DR movements (because of tally txn)

        // Update the last_sync in the database (which corresponds with the last_confirmed in the state)
        batch.put(
            &keys::wallet_last_sync(),
            CheckpointBeacon {
                checkpoint: block_info.epoch,
                hash_prev_block: block_info.block_hash,
            },
        )?;

        self.db.write(batch)?;

        Ok(())
    }

    /// Retrieve the balance for the current wallet account.
    pub fn balance(&self) -> Result<model::WalletBalance> {
        let state = self.state.read()?;
        let balance = state.balance;

        Ok(balance)
    }

    /// Create a new value transfer transaction using available UTXOs.
    pub fn create_vtt(
        &self,
        types::VttParams {
            pkh,
            value,
            fee,
            time_lock,
        }: types::VttParams,
    ) -> Result<types::VTTransaction> {
        let mut state = self.state.write()?;
        let components =
            self.create_vt_transaction_components(&mut state, value, fee, Some((pkh, time_lock)))?;

        let body = types::VTTransactionBody::new(components.inputs, components.outputs);
        let sign_data = body.hash();
        let signatures: Result<Vec<types::KeyedSignature>> = components
            .sign_keys
            .into_iter()
            .map(|sign_key| {
                let public_key = From::from(types::PK::from_secret_key(&self.engine, &sign_key));
                let signature = From::from(types::signature::sign(
                    &self.engine,
                    sign_key,
                    sign_data.as_ref(),
                )?);

                Ok(types::KeyedSignature {
                    signature,
                    public_key,
                })
            })
            .collect();

        Ok(types::VTTransaction::new(body, signatures?))
    }

    /// Create a new data request transaction using available UTXOs.
    pub fn create_data_req(
        &self,
        types::DataReqParams { fee, request }: types::DataReqParams,
    ) -> Result<types::DRTransaction> {
        let mut state = self.state.write()?;
        let value = request
            .checked_total_value()
            .map_err(|_| Error::TransactionValueOverflow)?;
        let components = self.create_dr_transaction_components(&mut state, value, fee)?;

        let body = types::DRTransactionBody::new(components.inputs, components.outputs, request);
        let sign_data = body.hash();
        let signatures: Result<Vec<types::KeyedSignature>> = components
            .sign_keys
            .into_iter()
            .map(|sign_key| {
                let public_key = From::from(types::PK::from_secret_key(&self.engine, &sign_key));
                let signature = From::from(types::signature::sign(
                    &self.engine,
                    sign_key,
                    sign_data.as_ref(),
                )?);

                Ok(types::KeyedSignature {
                    signature,
                    public_key,
                })
            })
            .collect();

        Ok(types::DRTransaction::new(body, signatures?))
    }

    fn create_vt_transaction_components(
        &self,
        state: &mut State,
        value: u64,
        fee: u64,
        recipient: Option<(types::PublicKeyHash, u64)>,
    ) -> Result<types::TransactionComponents> {
        self.create_transaction_components(state, value, fee, recipient, false)
    }

    fn create_dr_transaction_components(
        &self,
        state: &mut State,
        value: u64,
        fee: u64,
    ) -> Result<types::TransactionComponents> {
        self.create_transaction_components(state, value, fee, None, true)
    }

    fn create_transaction_components(
        &self,
        state: &mut State,
        value: u64,
        fee: u64,
        recipient: Option<(types::PublicKeyHash, u64)>,
        // When creating data request transactions, the change address must be the same as the
        // first input address
        change_address_same_as_input: bool,
    ) -> Result<types::TransactionComponents> {
        let target = value.saturating_add(fee);
        let mut payment = 0u64;
        let mut inputs = Vec::with_capacity(5);
        let mut outputs = Vec::with_capacity(2);
        let mut sign_keys = Vec::with_capacity(5);
        let mut used_utxos = Vec::with_capacity(5);
        let mut balance = state.balance;

        if let Some((pkh, time_lock)) = recipient {
            outputs.push(types::VttOutput {
                pkh,
                value,
                time_lock,
            });
        }

        let mut first_pkh = None;
        let timestamp =
            u64::try_from(get_timestamp()).expect("Get timestamp should return a positive value");
        for (out_ptr, key_balance) in state.utxo_set.iter() {
            if payment >= target {
                break;
            } else if key_balance.time_lock > timestamp {
                continue;
            }

            let input = types::TransactionInput::new(types::OutputPointer {
                transaction_id: out_ptr.transaction_id(),
                output_index: out_ptr.output_index,
            });
            let model::Path {
                keychain, index, ..
            } = self.db.get(&keys::pkh(&key_balance.pkh))?;
            let parent_key = &state
                .keychains
                .get(keychain as usize)
                .expect("could not get keychain");

            let extended_sign_key =
                parent_key.derive(&self.engine, &types::KeyPath::default().index(index))?;

            if first_pkh.is_none() && change_address_same_as_input {
                let public_key: types::PK =
                    types::ExtendedPK::from_secret_key(&self.engine, &extended_sign_key).into();

                first_pkh = Some(witnet_data_structures::chain::PublicKey::from(public_key).pkh());
            }

            payment = payment
                .checked_add(key_balance.amount)
                .ok_or_else(|| Error::TransactionValueOverflow)?;
            balance.unconfirmed.available = balance
                .unconfirmed
                .available
                .checked_sub(key_balance.amount)
                .ok_or_else(|| Error::TransactionBalanceUnderflow)?;
            inputs.push(input);
            sign_keys.push(extended_sign_key.into());
            used_utxos.push(out_ptr.clone());
        }

        if payment < target {
            Err(Error::InsufficientBalance)
        } else {
            let change = payment - target;

            if change > 0 {
                let change_pkh = if let Some(pkh) = first_pkh {
                    pkh
                } else {
                    self._gen_internal_address(state, None)?.pkh
                };

                outputs.push(types::VttOutput {
                    pkh: change_pkh,
                    value: change,
                    time_lock: 0,
                });
            }

            Ok(types::TransactionComponents {
                value,
                balance: balance.unconfirmed,
                change,
                inputs,
                outputs,
                sign_keys,
                used_utxos,
            })
        }
    }

    fn _gen_internal_address(
        &self,
        state: &mut State,
        label: Option<String>,
    ) -> Result<Arc<model::Address>> {
        let keychain = constants::INTERNAL_KEYCHAIN;
        let account = state.account;
        let index = state.next_internal_index;
        let parent_key = &state.keychains[keychain as usize];

        let (address, next_index) =
            self.gen_address(label, parent_key, account, keychain, index)?;

        state.next_internal_index = next_index;

        Ok(address)
    }

    fn _index_transaction(
        &self,
        state: &mut State,
        txn: &model::ExtendedTransaction,
        block_info: &model::Beacon,
        confirmed: bool,
    ) -> Result<Option<(model::BalanceMovement, Vec<Arc<model::Address>>)>> {
        // Wallet's account mutation (utxo set changes + balance movement)
        let account_mutation =
            match self._get_account_mutation(&state, &txn, &block_info, confirmed)? {
                // If UTXO set has not changed, then there is no balance movement derived from the transaction being processed
                None => return Ok(None),
                Some(account_mutation) => account_mutation,
            };

        // If exists, remove transaction from local pending movements
        let txn_hash = txn.transaction.hash();
        if let Some(local_movement) = state.local_movements.remove(&txn_hash) {
            log::debug!(
                "Updating local pending movement (txn id: {}) because it has been included in block #{}",
                txn_hash,
                block_info.epoch,
            );
            state.balance.local = state
                .balance
                .local
                .checked_sub(local_movement.amount)
                .ok_or_else(|| Error::TransactionValueOverflow)?;
        }

        // Update memory state: `utxo_set`
        for pointer in &account_mutation.utxo_removals {
            state.utxo_set.remove(pointer);
        }
        for (pointer, key_balance) in &account_mutation.utxo_inserts {
            state.utxo_set.insert(pointer.clone(), key_balance.clone());
        }

        // Update `transaction_next_id`
        state.transaction_next_id = state
            .transaction_next_id
            .checked_add(1)
            .ok_or_else(|| Error::TransactionIdOverflow)?;

        // Update addresses and their information if there were payments (new UTXOs)
        let mut addresses = vec![];

        for (output_pointer, key_balance) in account_mutation.utxo_inserts {
            // Retrieve previous address information
            let path = self
                .db
                .get::<_, model::Path>(&keys::pkh(&key_balance.pkh))?;

            // Get address from memory or DB
            let old_address = self._get_address(state, path.account, path.keychain, path.index)?;

            // Build the new address information
            let info = &old_address.info;
            let mut received_payments = info.received_payments.clone();
            received_payments.push(output_pointer.to_string());
            let current_timestamp =
                convert_block_epoch_to_timestamp(state.epoch_constants, block_info.epoch);
            let first_payment_date = Some(info.first_payment_date.unwrap_or(current_timestamp));
            let updated_address = model::Address {
                address: old_address.address.clone(),
                index: old_address.index,
                keychain: old_address.keychain,
                account: old_address.account,
                path: old_address.path.clone(),
                info: model::AddressInfo {
                    db_key: keys::address_info(path.account, path.keychain, path.index),
                    label: info.label.clone(),
                    received_payments,
                    received_amount: info.received_amount + key_balance.amount,
                    first_payment_date,
                    last_payment_date: Some(current_timestamp),
                },
                pkh: old_address.pkh,
            };

            log::trace!(
                "Updating address:\nOld: {:?}\nNew: {:?}",
                old_address,
                updated_address
            );

            addresses.push(Arc::new(updated_address));
        }

        // FIXME(#1539): if tally txn, compute update of data request balance movement

        Ok(Some((account_mutation.balance_movement, addresses)))
    }

    // TODO: notify client of new local pending transaction
    /// Add local pending balance movement submitted by wallet client
    pub fn add_local_movement(
        &self,
        txn: &model::ExtendedTransaction,
    ) -> Result<Option<model::BalanceMovement>> {
        let mut state = self.state.write()?;

        if let Some(mut account_mutation) =
            self._get_account_mutation(&state, txn, &model::Beacon::default(), false)?
        {
            account_mutation.balance_movement.transaction.timestamp =
                u64::try_from(get_timestamp())
                    .expect("Get timestamp should return a positive value");
            let txn_hash = txn.transaction.hash();
            state
                .local_movements
                .insert(txn_hash, account_mutation.balance_movement.clone());
            log::debug!(
                "Local pending movement added for transaction id: {})",
                txn_hash
            );
            state.balance.local = state
                .balance
                .local
                .checked_add(account_mutation.balance_movement.amount)
                .ok_or_else(|| Error::TransactionValueOverflow)?;

            return Ok(Some(account_mutation.balance_movement));
        }

        Ok(None)
    }

    fn _get_account_mutation(
        &self,
        state: &State,
        txn: &model::ExtendedTransaction,
        block_info: &model::Beacon,
        confirmed: bool,
    ) -> Result<Option<AccountMutation>> {
        // Inputs and outputs from different transaction types
        let (inputs, outputs) = match &txn.transaction {
            types::Transaction::ValueTransfer(vt) => {
                (vt.body.inputs.clone(), vt.body.outputs.clone())
            }
            types::Transaction::DataRequest(dr) => {
                (dr.body.inputs.clone(), dr.body.outputs.clone())
            }
            types::Transaction::Commit(commit) => {
                (commit.body.collateral.clone(), commit.body.outputs.clone())
            }
            types::Transaction::Tally(tally) => (vec![], tally.outputs.clone()),
            types::Transaction::Mint(mint) => (vec![], mint.outputs.clone()),
            _ => {
                return Err(Error::UnsupportedTransactionType(format!(
                    "{:?}",
                    txn.transaction
                )));
            }
        };

        let mut utxo_removals: Vec<model::OutPtr> = vec![];
        let mut utxo_inserts: Vec<(model::OutPtr, model::KeyBalance)> = vec![];

        let mut input_amount: u64 = 0;
        for input in inputs.iter() {
            let out_ptr: model::OutPtr = input.output_pointer().into();

            if let Some(model::KeyBalance { amount, .. }) = state.utxo_set.get(&out_ptr) {
                input_amount = input_amount
                    .checked_add(*amount)
                    .ok_or_else(|| Error::TransactionBalanceOverflow)?;
                utxo_removals.push(out_ptr);
            }
        }

        let mut output_amount: u64 = 0;
        for (index, output) in outputs.iter().enumerate() {
            if let Some(model::Path { .. }) = self.db.get_opt(&keys::pkh(&output.pkh))? {
                let out_ptr = model::OutPtr {
                    txn_hash: txn.transaction.hash().as_ref().to_vec(),
                    output_index: u32::try_from(index).unwrap(),
                };
                let key_balance = model::KeyBalance {
                    amount: output.value,
                    pkh: output.pkh,
                    time_lock: output.time_lock,
                };
                output_amount = output_amount
                    .checked_add(output.value)
                    .ok_or_else(|| Error::TransactionBalanceOverflow)?;

                let address = output.pkh.bech32(if self.params.testnet {
                    Environment::Testnet
                } else {
                    Environment::Mainnet
                });
                log::warn!(
                    "Found transaction to our address {}! Amount: +{} nanowits",
                    address,
                    output.value
                );
                utxo_inserts.push((out_ptr, key_balance));
            }
        }

        // If UTXO set has not changed, then there is no balance movement derived from the transaction being processed
        if utxo_inserts.is_empty() && utxo_removals.is_empty() {
            return Ok(None);
        }

        let (amount, kind) = if output_amount >= input_amount {
            (output_amount - input_amount, model::MovementType::Positive)
        } else {
            (input_amount - output_amount, model::MovementType::Negative)
        };

        // Build the balance movement, first computing the miner fee
        let miner_fee: u64 = match &txn.metadata {
            Some(model::TransactionMetadata::InputValues(input_values)) => {
                let total_input_amount = input_values.iter().fold(0, |acc, x| acc + x.value);
                let total_output_amount = outputs.iter().fold(0, |acc, x| acc + x.value);

                total_input_amount
                    .checked_sub(total_output_amount)
                    .unwrap_or_else(|| {
                        log::warn!("Miner fee below 0 in a transaction of type value transfer or data request: {:?}", txn.transaction);

                        0
                    })
            }
            _ => 0,
        };

        let balance_movement = build_balance_movement(
            state.transaction_next_id,
            &txn,
            miner_fee,
            kind,
            amount,
            &block_info,
            convert_block_epoch_to_timestamp(state.epoch_constants, block_info.epoch),
            confirmed,
        )?;

        Ok(Some(AccountMutation {
            balance_movement,
            utxo_inserts,
            utxo_removals,
        }))
    }

    fn _gen_external_address(
        &self,
        state: &mut State,
        label: Option<String>,
    ) -> Result<Arc<model::Address>> {
        let keychain = constants::EXTERNAL_KEYCHAIN;
        let account = state.account;
        let index = state.next_external_index;
        let parent_key = &state.keychains[keychain as usize];

        let (address, next_index) =
            self.gen_address(label, parent_key, account, keychain, index)?;

        let path = model::Path {
            account,
            keychain,
            index,
        }
        .to_string();
        state.next_external_index = next_index;
        state
            .pending_addresses_by_path
            .entry(path)
            .or_insert_with(|| address.clone());

        Ok(address)
    }

    /// Get previously created Transaction by its hash.
    pub fn get_db_transaction(&self, hex_hash: &str) -> Result<Option<types::Transaction>> {
        let txn = self.db.get_opt(&keys::transaction(hex_hash))?;

        Ok(txn)
    }

    /// Sign data using the wallet master key.
    pub fn sign_data(
        &self,
        data: &str,
        extended_pk: bool,
    ) -> Result<model::ExtendedKeyedSignature> {
        let state = self.state.read()?;

        let keychain = constants::EXTERNAL_KEYCHAIN;
        let parent_key = &state.keychains[keychain as usize];

        let chaincode = if extended_pk {
            hex::encode(parent_key.chain_code())
        } else {
            "".to_string()
        };
        let public_key = ExtendedPK::from_secret_key(&self.engine, &parent_key)
            .key
            .to_string();

        let hashed_data = calculate_sha256(data.as_bytes());
        let signature =
            signature::sign(&self.engine, parent_key.secret_key, hashed_data.as_ref())?.to_string();

        Ok(model::ExtendedKeyedSignature {
            chaincode,
            public_key,
            signature,
        })
    }

    /// Update which was the epoch of the last block that was processed by this wallet.
    pub fn update_sync_state(&self, beacon: CheckpointBeacon, confirmed: bool) -> Result<()> {
        log::debug!(
            "Setting {} tip of the chain for wallet {} to {:?}",
            if confirmed { "confirmed" } else { "pending " },
            self.id,
            beacon,
        );

        if let Ok(mut write_guard) = self.state.write() {
            write_guard.last_sync = beacon;
            if confirmed {
                write_guard.last_confirmed = beacon;
            }
        }

        // // Only persist last_sync if block is confirmed
        // if confirmed {
        //     // TODO: modify last_sync for last_confirmed?
        //     self.db
        //         .put(&keys::wallet_last_sync(), beacon)
        //         .map_err(Error::from)?
        // }

        Ok(())
    }

    /// Handle superblock in wallet by confirming pending block changes
    pub fn handle_superblock(&self, block_hashes: &[String]) -> Result<()> {
        if let Some(last_confirmed_hash) = block_hashes.last() {
            let state = self.state.read()?;
            if last_confirmed_hash == &state.last_confirmed.hash_prev_block.to_string() {
                log::debug!(
                    "Superblock notification was previously handled (Block #{}: {} is already confirmed)",
                    state.last_confirmed.checkpoint,
                    last_confirmed_hash
                );

                return Ok(());
            }
        }

        block_hashes.iter().try_for_each(|block_hash| {
            // Genesis block is always confirmed
            if block_hash == &self.params.genesis_hash.to_string() {
                Ok(())
            } else {
                self.try_consolidate_block(block_hash)
            }
        })
    }

    /// Try to consolidate a block by persisting all changes into the database.
    pub fn try_consolidate_block(&self, block_hash: &str) -> Result<()> {
        let mut state = self.state.write()?;

        // Retrieve and remove pending changes of the block
        let block_state = state.pending_blocks.remove(block_hash).ok_or_else(|| {
            Error::BlockConsolidation(format!("beacon not found for pending block {}", block_hash))
        })?;
        let movements = state.pending_movements.remove(block_hash).ok_or_else(|| {
            Error::BlockConsolidation(format!(
                "balance movements not found for pending block {}",
                block_hash
            ))
        })?;
        let addresses = state
            .pending_addresses_by_block
            .remove(block_hash)
            .ok_or_else(|| {
                Error::BlockConsolidation(format!(
                    "address infos not found for pending block {}",
                    block_hash
                ))
            })?;

        // Try to persist block transaction changes
        self._persist_block_txns(
            movements,
            addresses,
            block_state.transaction_next_id,
            state.next_external_index,
            state.next_internal_index,
            block_state.utxo_set.clone(),
            &block_state.balance,
            &block_state.beacon,
        )?;

        // If everything was OK, update `last_confirmed` beacon
        state.last_confirmed = CheckpointBeacon {
            checkpoint: block_state.beacon.epoch,
            hash_prev_block: block_state.beacon.block_hash,
        };
        state.balance.confirmed = block_state.balance;

        log::debug!(
            "Block #{} ({}) was successfully consolidated",
            state.last_confirmed.checkpoint,
            state.last_confirmed.hash_prev_block,
        );

        Ok(())
    }
}

fn convert_block_epoch_to_timestamp(epoch_constants: EpochConstants, epoch: Epoch) -> u64 {
    // In case of error, return timestamp 0
    u64::try_from(epoch_constants.epoch_timestamp(epoch).unwrap_or(0))
        .expect("Epoch timestamp should return a positive value")
}

// Balance Movement Factory
#[allow(clippy::too_many_arguments)]
fn build_balance_movement(
    identifier: u32,
    txn: &model::ExtendedTransaction,
    miner_fee: u64,
    kind: model::MovementType,
    amount: u64,
    block_info: &model::Beacon,
    timestamp: u64,
    confirmed: bool,
) -> Result<model::BalanceMovement> {
    // Input values with their ValueTransferOutput data
    let transaction_inputs = match &txn.metadata {
        Some(model::TransactionMetadata::InputValues(inputs)) => inputs
            .iter()
            .map(|output| model::Input {
                address: output.pkh.to_string(),
                value: output.value,
            })
            .collect::<Vec<model::Input>>(),
        _ => vec![],
    };

    // Transaction Data
    let transaction_data = match &txn.transaction {
        types::Transaction::ValueTransfer(vtt) => {
            model::TransactionData::ValueTransfer(model::VtData {
                inputs: transaction_inputs,
                outputs: vtt
                    .body
                    .outputs
                    .clone()
                    .into_iter()
                    .map(|output| model::Output {
                        address: output.pkh.to_string(),
                        time_lock: output.time_lock,
                        value: output.value,
                    })
                    .collect::<Vec<model::Output>>(),
            })
        }
        types::Transaction::DataRequest(dr) => model::TransactionData::DataRequest(model::DrData {
            inputs: transaction_inputs,
            outputs: dr
                .body
                .outputs
                .clone()
                .into_iter()
                .map(|output| model::Output {
                    address: output.pkh.to_string(),
                    time_lock: output.time_lock,
                    value: output.value,
                })
                .collect::<Vec<model::Output>>(),
            tally: None,
        }),
        types::Transaction::Commit(commit) => model::TransactionData::Commit(model::VtData {
            inputs: transaction_inputs,
            outputs: commit
                .body
                .outputs
                .clone()
                .into_iter()
                .map(|output| model::Output {
                    address: output.pkh.to_string(),
                    time_lock: output.time_lock,
                    value: output.value,
                })
                .collect::<Vec<model::Output>>(),
        }),
        types::Transaction::Mint(mint) => model::TransactionData::Mint(model::MintData {
            outputs: mint
                .outputs
                .clone()
                .into_iter()
                .map(|output| model::Output {
                    address: output.pkh.to_string(),
                    time_lock: output.time_lock,
                    value: output.value,
                })
                .collect::<Vec<model::Output>>(),
        }),
        types::Transaction::Tally(tally) => model::TransactionData::Tally(model::TallyData {
            request_transaction_hash: tally.dr_pointer.to_string(),
            outputs: tally
                .outputs
                .clone()
                .into_iter()
                .map(|output| model::Output {
                    address: output.pkh.to_string(),
                    time_lock: output.time_lock,
                    value: output.value,
                })
                .collect::<Vec<model::Output>>(),
            tally: build_tally_report(tally, &txn.metadata)?,
        }),
        _ => {
            return Err(Error::UnsupportedTransactionType(format!(
                "{:?}",
                txn.transaction
            )));
        }
    };

    Ok(model::BalanceMovement {
        db_key: identifier,
        kind,
        amount,
        transaction: model::Transaction {
            block: Some(block_info.clone()),
            confirmed,
            data: transaction_data,
            hash: hex::encode(txn.transaction.hash()),
            miner_fee,
            timestamp,
        },
    })
}

fn build_tally_report(
    tally: &types::TallyTransaction,
    metadata: &Option<model::TransactionMetadata>,
) -> Result<model::TallyReport> {
    let reveals = match metadata {
        Some(model::TransactionMetadata::Tally(report)) => {
            // List of reveals extracted from Data Request Report
            let mut reveals: HashMap<PublicKeyHash, model::Reveal> = report
                .reveals
                .iter()
                .map(|(pkh, reveal_txn)| {
                    types::RadonTypes::try_from(reveal_txn.body.reveal.as_slice())
                        .map(|x| {
                            (
                                *pkh,
                                model::Reveal {
                                    value: x.to_string(),
                                    in_consensus: true,
                                },
                            )
                        })
                        .map_err(|err| Error::RevealRadDecode(err.to_string()))
                })
                .collect::<Result<HashMap<PublicKeyHash, model::Reveal>>>()?;

            // Set not `in_consensus` reveals
            for pkh in &tally.out_of_consensus {
                let outlier = reveals.get_mut(&pkh).cloned();
                if let Some(mut reveal) = outlier {
                    reveal.in_consensus = false;
                } else {
                    reveals.insert(
                        *pkh,
                        model::Reveal {
                            value: types::RadonTypes::from(
                                RadonError::try_from(types::RadError::NoReveals).unwrap(),
                            )
                            .to_string(),
                            in_consensus: false,
                        },
                    );
                }
            }

            Ok(reveals.values().cloned().collect::<Vec<model::Reveal>>())
        }
        _ => Err(Error::WrongMetadataType(format!("{:?}", tally))),
    }?;

    Ok(model::TallyReport {
        result: types::RadonTypes::try_from(tally.tally.as_slice())
            .map_err(|err| Error::TallyRadDecode(err.to_string()))?
            .to_string(),
        reveals,
    })
}

#[cfg(test)]
impl<T> Wallet<T>
where
    T: Database,
{
    pub fn utxo_set(&self) -> Result<model::UtxoSet> {
        let state = self.state.read()?;

        Ok(state.utxo_set.clone())
    }
}
