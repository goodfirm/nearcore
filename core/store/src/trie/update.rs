pub use self::iterator::TrieUpdateIterator;
use super::accounting_cache::TrieAccountingCacheSwitch;
use super::{OptimizedValueRef, Trie, TrieWithReadLock, ValueAccessToken};
use crate::contract::ContractStorage;
use crate::trie::{KeyLookupMode, TrieChanges};
use crate::StorageError;
use near_primitives::hash::{hash, CryptoHash};
use near_primitives::stateless_validation::contract_distribution::ContractUpdates;
use near_primitives::trie_key::TrieKey;
use near_primitives::types::{
    AccountId, RawStateChange, RawStateChanges, RawStateChangesWithTrieKey, StateChangeCause,
    StateRoot, TrieCacheMode,
};
use near_primitives::version::ProtocolFeature;
use near_vm_runner::logic::ProtocolVersion;
use std::collections::BTreeMap;

mod iterator;

/// Key-value update. Contains a TrieKey and a value.
pub struct TrieKeyValueUpdate {
    pub trie_key: TrieKey,
    pub value: Option<Vec<u8>>,
}

/// key that was updated -> the update.
pub type TrieUpdates = BTreeMap<Vec<u8>, TrieKeyValueUpdate>;

/// Provides a way to access Storage and record changes with future commit.
/// TODO (#7327): rename to StateUpdate
pub struct TrieUpdate {
    pub trie: Trie,
    pub contract_storage: ContractStorage,
    committed: RawStateChanges,
    prospective: TrieUpdates,
}

pub enum TrieUpdateValuePtr<'a> {
    Ref(&'a Trie, OptimizedValueRef),
    MemoryRef(&'a [u8]),
}

impl<'a> TrieUpdateValuePtr<'a> {
    pub fn len(&self) -> u32 {
        match self {
            TrieUpdateValuePtr::MemoryRef(value) => value.len() as u32,
            TrieUpdateValuePtr::Ref(_, value_ref) => value_ref.len() as u32,
        }
    }

    pub fn deref_value(&self) -> Result<Vec<u8>, StorageError> {
        match self {
            TrieUpdateValuePtr::MemoryRef(value) => Ok(value.to_vec()),
            TrieUpdateValuePtr::Ref(trie, value_ref) => Ok(trie.deref_optimized(value_ref)?),
        }
    }
}

/// Contains the result of trie updates generated during the finalization of [`TrieUpdate`].
pub struct TrieUpdateResult {
    pub trie: Trie,
    pub trie_changes: TrieChanges,
    pub state_changes: Vec<RawStateChangesWithTrieKey>,
    /// Contracts accessed and deployed while applying the chunk.
    pub contract_updates: ContractUpdates,
}

impl TrieUpdate {
    pub fn new(trie: Trie) -> Self {
        let trie_storage = trie.storage.clone();
        Self {
            trie,
            contract_storage: ContractStorage::new(trie_storage),
            committed: Default::default(),
            prospective: Default::default(),
        }
    }

    pub fn trie(&self) -> &Trie {
        &self.trie
    }

    pub fn get_ref(
        &self,
        key: &TrieKey,
        mode: KeyLookupMode,
    ) -> Result<Option<TrieUpdateValuePtr<'_>>, StorageError> {
        let key = key.to_vec();
        if let Some(key_value) = self.prospective.get(&key) {
            return Ok(key_value.value.as_deref().map(TrieUpdateValuePtr::MemoryRef));
        } else if let Some(changes_with_trie_key) = self.committed.get(&key) {
            if let Some(RawStateChange { data, .. }) = changes_with_trie_key.changes.last() {
                return Ok(data.as_deref().map(TrieUpdateValuePtr::MemoryRef));
            }
        }

        let result = self
            .trie
            .get_optimized_ref(&key, mode)?
            .map(|optimized_value_ref| TrieUpdateValuePtr::Ref(&self.trie, optimized_value_ref));

        Ok(result)
    }

    pub fn contains_key(&self, key: &TrieKey) -> Result<bool, StorageError> {
        let key = key.to_vec();
        if self.prospective.contains_key(&key) {
            return Ok(true);
        } else if let Some(changes_with_trie_key) = self.committed.get(&key) {
            if let Some(RawStateChange { data, .. }) = changes_with_trie_key.changes.last() {
                return Ok(data.is_some());
            }
        }
        self.trie.contains_key(&key)
    }

    pub fn set(&mut self, trie_key: TrieKey, value: Vec<u8>) {
        // NOTE: Converting `TrieKey` to a `Vec<u8>` is useful here for 2 reasons:
        // - Using `Vec<u8>` for sorting `BTreeMap` in the same order as a `Trie` and
        //   avoid recomputing `Vec<u8>` every time. It helps for merging iterators.
        // - Using `TrieKey` later for `RawStateChangesWithTrieKey` for State changes RPCs.
        self.prospective
            .insert(trie_key.to_vec(), TrieKeyValueUpdate { trie_key, value: Some(value) });
    }

    pub fn remove(&mut self, trie_key: TrieKey) {
        // We count removals performed by the contracts and charge extra for them.
        // A malicious contract could generate a lot of storage proof by a removal,
        // charging extra provides a safe upper bound. (https://github.com/near/nearcore/issues/10890)
        // This only applies to removals performed by the contracts. Removals performed
        // by the runtime are assumed to be non-malicious and we don't charge extra for them.
        if let Some(recorder) = &self.trie.recorder {
            if matches!(trie_key, TrieKey::ContractData { .. }) {
                recorder.borrow_mut().record_removal();
            }
        }

        self.prospective.insert(trie_key.to_vec(), TrieKeyValueUpdate { trie_key, value: None });
    }

    pub fn commit(&mut self, event: StateChangeCause) {
        let prospective = std::mem::take(&mut self.prospective);
        for (raw_key, TrieKeyValueUpdate { trie_key, value }) in prospective.into_iter() {
            self.committed
                .entry(raw_key)
                .or_insert_with(|| RawStateChangesWithTrieKey { trie_key, changes: Vec::new() })
                .changes
                .push(RawStateChange { cause: event.clone(), data: value });
        }
        self.contract_storage.commit_deploys();
    }

    pub fn rollback(&mut self) {
        self.prospective.clear();
        self.contract_storage.rollback_deploys();
    }

    /// Prepare the accumulated state changes to be applied to the underlying storage.
    ///
    /// This Function returns the [`Trie`] with which the [`TrieUpdate`] has been initially
    /// constructed. It can be reused to construct another `TrieUpdate` or to operate with `Trie`
    /// in any other way as desired.
    #[tracing::instrument(
        level = "debug",
        target = "store::trie",
        "TrieUpdate::finalize",
        skip_all,
        fields(
            committed.len = self.committed.len(),
            mem_reads = tracing::field::Empty,
            db_reads = tracing::field::Empty
        )
    )]
    pub fn finalize(self) -> Result<TrieUpdateResult, StorageError> {
        assert!(self.prospective.is_empty(), "Finalize cannot be called with uncommitted changes.");
        let span = tracing::Span::current();
        let TrieUpdate { trie, committed, contract_storage, .. } = self;
        let start_counts = trie.accounting_cache.borrow().get_trie_nodes_count();
        let mut state_changes = Vec::with_capacity(committed.len());
        let trie_changes =
            trie.update(committed.into_iter().map(|(k, changes_with_trie_key)| {
                let data = changes_with_trie_key
                    .changes
                    .last()
                    .expect("Committed entry should have at least one change")
                    .data
                    .clone();
                state_changes.push(changes_with_trie_key);
                (k, data)
            }))?;
        let end_counts = trie.accounting_cache.borrow().get_trie_nodes_count();
        if let Some(iops_delta) = end_counts.checked_sub(&start_counts) {
            span.record("mem_reads", iops_delta.mem_reads);
            span.record("db_reads", iops_delta.db_reads);
        }
        let contract_updates = contract_storage.finalize();
        Ok(TrieUpdateResult { trie, trie_changes, state_changes, contract_updates })
    }

    /// Returns Error if the underlying storage fails
    pub fn iter(&self, key_prefix: &[u8]) -> Result<TrieUpdateIterator<'_>, StorageError> {
        TrieUpdateIterator::new(self, key_prefix, None)
    }

    pub fn locked_iter<'a>(
        &'a self,
        key_prefix: &[u8],
        lock: &'a TrieWithReadLock<'_>,
    ) -> Result<TrieUpdateIterator<'a>, StorageError> {
        TrieUpdateIterator::new(self, key_prefix, Some(lock))
    }

    pub fn get_root(&self) -> &StateRoot {
        self.trie.get_root()
    }

    /// Returns a guard-style type that will reset the trie cache mode back to the initial state
    /// once dropped.
    ///
    /// Only changes the cache mode if `mode` is `Some`. Will always restore the previous cache
    /// mode upon drop. The type should not be `std::mem::forget`-ten, as it will leak memory.
    pub fn with_trie_cache_mode(&self, mode: Option<TrieCacheMode>) -> TrieCacheModeGuard {
        let switch = self.trie.accounting_cache.borrow().enable_switch();
        let previous = switch.enabled();
        if let Some(mode) = mode {
            switch.set(mode == TrieCacheMode::CachingChunk);
        }
        TrieCacheModeGuard(previous, switch)
    }

    fn get_from_updates(
        &self,
        key: &TrieKey,
        fallback: impl FnOnce(&[u8]) -> Result<Option<Vec<u8>>, StorageError>,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        let key = key.to_vec();
        if let Some(key_value) = self.prospective.get(&key) {
            return Ok(key_value.value.as_ref().map(<Vec<u8>>::clone));
        } else if let Some(changes_with_trie_key) = self.committed.get(&key) {
            if let Some(RawStateChange { data, .. }) = changes_with_trie_key.changes.last() {
                return Ok(data.as_ref().map(<Vec<u8>>::clone));
            }
        }
        fallback(&key)
    }

    /// Records an access to the contract code due to a function call.
    ///
    /// The contract code is either included in the state witness or distributed
    /// separately from the witness (see `ExcludeContractCodeFromStateWitness` feature).
    /// In the former case, we record a Trie read from the `TrieKey::ContractCode` for each contract.
    /// In the latter case, the Trie read does not happen and the code-size does not contribute to
    /// the storage-proof limit. Instead we just record that the code with the given hash was called,
    /// so that we can identify which contract-code to distribute to the validators.
    pub fn record_contract_call(
        &self,
        account_id: AccountId,
        code_hash: CryptoHash,
        protocol_version: ProtocolVersion,
    ) -> Result<(), StorageError> {
        if !ProtocolFeature::ExcludeContractCodeFromStateWitness.enabled(protocol_version) {
            // This causes trie lookup for the contract code to happen with side effects (charging gas and recording trie nodes).
            self.trie.request_code_recording(account_id);
            return Ok(());
        }

        // Only record the call if trie contains the contract (with the given hash) being called deployed to the given account.
        // This avoids recording contracts that do not exist or are newly-deployed to the account.
        // Note that the check below to see if the contract exists has no side effects (not charging gas or recording trie nodes)
        if code_hash == CryptoHash::default() {
            return Ok(());
        }
        let trie_key = TrieKey::ContractCode { account_id };
        let contract_ref = self
            .trie
            .get_optimized_ref_no_side_effects(&trie_key.to_vec(), KeyLookupMode::FlatStorage)
            .or_else(|err| {
                // If the value for the trie key is not found, we treat it as if the contract does not exist.
                // In this case, we ignore the error and skip recording the contract call below.
                if matches!(err, StorageError::MissingTrieValue(_, _)) {
                    Ok(None)
                } else {
                    Err(err)
                }
            })?;
        let contract_exists: bool = match contract_ref {
            Some(OptimizedValueRef::Ref(value_ref)) => value_ref.hash == code_hash,
            Some(OptimizedValueRef::AvailableValue(ValueAccessToken { value })) => {
                hash(value.as_slice()) == code_hash
            }
            None => false,
        };
        if contract_exists {
            self.contract_storage.record_call(code_hash);
        }
        Ok(())
    }
}

impl crate::TrieAccess for TrieUpdate {
    fn get(&self, key: &TrieKey) -> Result<Option<Vec<u8>>, StorageError> {
        self.get_from_updates(key, |k| self.trie.get(k))
    }

    fn get_no_side_effects(&self, key: &TrieKey) -> Result<Option<Vec<u8>>, StorageError> {
        self.get_from_updates(key, |_| self.trie.get_no_side_effects(&key))
    }

    fn contains_key(&self, key: &TrieKey) -> Result<bool, StorageError> {
        TrieUpdate::contains_key(&self, key)
    }
}

pub struct TrieCacheModeGuard(bool, TrieAccountingCacheSwitch);
impl Drop for TrieCacheModeGuard {
    fn drop(&mut self) {
        self.1.set(self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::TestTriesBuilder;
    use crate::{ShardUId, TrieAccess as _};
    use near_primitives::hash::CryptoHash;
    const SHARD_VERSION: u32 = 1;
    const COMPLEX_SHARD_UID: ShardUId = ShardUId { version: SHARD_VERSION, shard_id: 0 };

    fn test_key(key: Vec<u8>) -> TrieKey {
        TrieKey::ContractData { account_id: "alice".parse().unwrap(), key }
    }

    #[test]
    fn trie() {
        let tries = TestTriesBuilder::new().with_shard_layout(SHARD_VERSION, 2).build();
        let root = Trie::EMPTY_ROOT;
        let mut trie_update = tries.new_trie_update(COMPLEX_SHARD_UID, root);
        trie_update.set(test_key(b"dog".to_vec()), b"puppy".to_vec());
        trie_update.set(test_key(b"dog2".to_vec()), b"puppy".to_vec());
        trie_update.set(test_key(b"xxx".to_vec()), b"puppy".to_vec());
        trie_update
            .commit(StateChangeCause::TransactionProcessing { tx_hash: CryptoHash::default() });
        let trie_changes = trie_update.finalize().unwrap().trie_changes;
        let mut store_update = tries.store_update();
        let new_root = tries.apply_all(&trie_changes, COMPLEX_SHARD_UID, &mut store_update);
        store_update.commit().unwrap();
        let trie_update2 = tries.new_trie_update(COMPLEX_SHARD_UID, new_root);
        assert_eq!(trie_update2.get(&test_key(b"dog".to_vec())), Ok(Some(b"puppy".to_vec())));
        let values = trie_update2
            .iter(&test_key(b"dog".to_vec()).to_vec())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            values,
            vec![test_key(b"dog".to_vec()).to_vec(), test_key(b"dog2".to_vec()).to_vec()]
        );
    }

    #[test]
    fn trie_remove() {
        let tries = TestTriesBuilder::new().with_shard_layout(SHARD_VERSION, 2).build();

        // Delete non-existing element.
        let mut trie_update = tries.new_trie_update(COMPLEX_SHARD_UID, Trie::EMPTY_ROOT);
        trie_update.remove(test_key(b"dog".to_vec()));
        trie_update.commit(StateChangeCause::TransactionProcessing { tx_hash: Trie::EMPTY_ROOT });
        let trie_changes = trie_update.finalize().unwrap().trie_changes;
        let mut store_update = tries.store_update();
        let new_root = tries.apply_all(&trie_changes, COMPLEX_SHARD_UID, &mut store_update);
        store_update.commit().unwrap();
        assert_eq!(new_root, Trie::EMPTY_ROOT);

        // Add and right away delete element.
        let mut trie_update = tries.new_trie_update(COMPLEX_SHARD_UID, Trie::EMPTY_ROOT);
        trie_update.set(test_key(b"dog".to_vec()), b"puppy".to_vec());
        trie_update.remove(test_key(b"dog".to_vec()));
        trie_update
            .commit(StateChangeCause::TransactionProcessing { tx_hash: CryptoHash::default() });
        let trie_changes = trie_update.finalize().unwrap().trie_changes;
        let mut store_update = tries.store_update();
        let new_root = tries.apply_all(&trie_changes, COMPLEX_SHARD_UID, &mut store_update);
        store_update.commit().unwrap();
        assert_eq!(new_root, Trie::EMPTY_ROOT);

        // Add, apply changes and then delete element.
        let mut trie_update = tries.new_trie_update(COMPLEX_SHARD_UID, Trie::EMPTY_ROOT);
        trie_update.set(test_key(b"dog".to_vec()), b"puppy".to_vec());
        trie_update
            .commit(StateChangeCause::TransactionProcessing { tx_hash: CryptoHash::default() });
        let trie_changes = trie_update.finalize().unwrap().trie_changes;
        let mut store_update = tries.store_update();
        let new_root = tries.apply_all(&trie_changes, COMPLEX_SHARD_UID, &mut store_update);
        store_update.commit().unwrap();
        assert_ne!(new_root, Trie::EMPTY_ROOT);
        let mut trie_update = tries.new_trie_update(COMPLEX_SHARD_UID, new_root);
        trie_update.remove(test_key(b"dog".to_vec()));
        trie_update
            .commit(StateChangeCause::TransactionProcessing { tx_hash: CryptoHash::default() });
        let trie_changes = trie_update.finalize().unwrap().trie_changes;
        let mut store_update = tries.store_update();
        let new_root = tries.apply_all(&trie_changes, COMPLEX_SHARD_UID, &mut store_update);
        store_update.commit().unwrap();
        assert_eq!(new_root, Trie::EMPTY_ROOT);
    }

    #[test]
    fn trie_iter() {
        let tries = TestTriesBuilder::new().build();
        let mut trie_update = tries.new_trie_update(ShardUId::single_shard(), Trie::EMPTY_ROOT);
        trie_update.set(test_key(b"dog".to_vec()), b"puppy".to_vec());
        trie_update.set(test_key(b"aaa".to_vec()), b"puppy".to_vec());
        trie_update
            .commit(StateChangeCause::TransactionProcessing { tx_hash: CryptoHash::default() });
        let trie_changes = trie_update.finalize().unwrap().trie_changes;
        let mut store_update = tries.store_update();
        let new_root = tries.apply_all(&trie_changes, ShardUId::single_shard(), &mut store_update);
        store_update.commit().unwrap();

        let mut trie_update = tries.new_trie_update(ShardUId::single_shard(), new_root);
        trie_update.set(test_key(b"dog2".to_vec()), b"puppy".to_vec());
        trie_update.set(test_key(b"xxx".to_vec()), b"puppy".to_vec());

        let values: Result<Vec<Vec<u8>>, _> =
            trie_update.iter(&test_key(b"dog".to_vec()).to_vec()).unwrap().collect();
        assert_eq!(
            values.unwrap(),
            vec![test_key(b"dog".to_vec()).to_vec(), test_key(b"dog2".to_vec()).to_vec()]
        );

        trie_update.rollback();

        let values: Result<Vec<Vec<u8>>, _> =
            trie_update.iter(&test_key(b"dog".to_vec()).to_vec()).unwrap().collect();
        assert_eq!(values.unwrap(), vec![test_key(b"dog".to_vec()).to_vec()]);

        let mut trie_update = tries.new_trie_update(ShardUId::single_shard(), new_root);
        trie_update.remove(test_key(b"dog".to_vec()));

        let values: Result<Vec<Vec<u8>>, _> =
            trie_update.iter(&test_key(b"dog".to_vec()).to_vec()).unwrap().collect();
        assert_eq!(values.unwrap().len(), 0);

        let mut trie_update = tries.new_trie_update(ShardUId::single_shard(), new_root);
        trie_update.set(test_key(b"dog2".to_vec()), b"puppy".to_vec());
        trie_update
            .commit(StateChangeCause::TransactionProcessing { tx_hash: CryptoHash::default() });
        trie_update.remove(test_key(b"dog2".to_vec()));

        let values: Result<Vec<Vec<u8>>, _> =
            trie_update.iter(&test_key(b"dog".to_vec()).to_vec()).unwrap().collect();
        assert_eq!(values.unwrap(), vec![test_key(b"dog".to_vec()).to_vec()]);

        let mut trie_update = tries.new_trie_update(ShardUId::single_shard(), new_root);
        trie_update.set(test_key(b"dog2".to_vec()), b"puppy".to_vec());
        trie_update
            .commit(StateChangeCause::TransactionProcessing { tx_hash: CryptoHash::default() });
        trie_update.set(test_key(b"dog3".to_vec()), b"puppy".to_vec());

        let values: Result<Vec<Vec<u8>>, _> =
            trie_update.iter(&test_key(b"dog".to_vec()).to_vec()).unwrap().collect();
        assert_eq!(
            values.unwrap(),
            vec![
                test_key(b"dog".to_vec()).to_vec(),
                test_key(b"dog2".to_vec()).to_vec(),
                test_key(b"dog3".to_vec()).to_vec()
            ]
        );
    }
}
