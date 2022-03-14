use std::sync::atomic::Ordering;

use roaring::{RoaringBitmap, RoaringTreemap};

use crate::leb128::Leb128;
use crate::serialize::{StoreSerialize, COLLECTION_PREFIX_LEN};
use crate::{changes, JMAPId, JMAPIdPrefix, WriteOperation};
use crate::{
    changes::ChangeId,
    serialize::{DeserializeBigEndian, INTERNAL_KEY_PREFIX},
    AccountId, ColumnFamily, Direction, Collection, JMAPStore, Store, StoreError,
};
pub type TermId = u64;
pub type LogIndex = u64;

#[derive(Default, Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct RaftId {
    pub term: TermId,
    pub index: LogIndex,
}

impl RaftId {
    pub fn new(term: TermId, index: LogIndex) -> Self {
        Self { term, index }
    }

    pub fn first() -> Self {
        Self { term: 0, index: 0 }
    }

    pub fn none() -> Self {
        Self {
            term: TermId::MAX,
            index: LogIndex::MAX,
        }
    }

    pub fn is_none(&self) -> bool {
        self.term == TermId::MAX && self.index == LogIndex::MAX
    }

    pub fn deserialize_key(bytes: &[u8]) -> Option<Self> {
        RaftId {
            term: bytes.deserialize_be_u64(1)?,
            index: bytes.deserialize_be_u64(1 + std::mem::size_of::<LogIndex>())?,
        }
        .into()
    }

    pub fn serialize_key(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity((std::mem::size_of::<LogIndex>() * 2) + 1);
        bytes.push(INTERNAL_KEY_PREFIX);
        bytes.extend_from_slice(&self.term.to_be_bytes());
        bytes.extend_from_slice(&self.index.to_be_bytes());
        bytes
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct Entry {
    pub raft_id: RaftId,
    pub account_id: AccountId,
    pub changes: Vec<Change>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct Change {
    pub change_id: ChangeId,
    pub collection: Collection,
}

impl Entry {
    pub fn deserialize(value: &[u8], raft_id: RaftId) -> Option<Self> {
        let mut value_it = value.iter();

        let account_id = AccountId::from_leb128_it(&mut value_it)?;
        let mut total_changes = usize::from_leb128_it(&mut value_it)?;
        let mut changes = Vec::with_capacity(total_changes);

        while total_changes > 0 {
            changes.push(Change {
                collection: (*value_it.next()?).into(),
                change_id: ChangeId::from_leb128_it(&mut value_it)?,
            });
            total_changes -= 1;
        }

        Entry {
            account_id,
            raft_id,
            changes,
        }
        .into()
    }
}

impl StoreSerialize for Entry {
    fn serialize(&self) -> Option<Vec<u8>> {
        let mut bytes = Vec::with_capacity(
            std::mem::size_of::<AccountId>()
                + std::mem::size_of::<usize>()
                + (self.changes.len()
                    * (std::mem::size_of::<ChangeId>() + std::mem::size_of::<Collection>())),
        );
        self.account_id.to_leb128_bytes(&mut bytes);
        self.changes.len().to_leb128_bytes(&mut bytes);

        for change in &self.changes {
            bytes.push(change.collection.into());
            change.change_id.to_leb128_bytes(&mut bytes);
        }

        Some(bytes)
    }
}

#[derive(Debug)]
pub struct PendingChanges {
    pub account_id: AccountId,
    pub collection: Collection,
    pub inserts: RoaringBitmap,
    pub updates: RoaringBitmap,
    pub deletes: RoaringBitmap,
    pub changes: RoaringTreemap,
}

impl PendingChanges {
    pub fn new(account_id: AccountId, collection: Collection) -> Self {
        Self {
            account_id,
            collection,
            inserts: RoaringBitmap::new(),
            updates: RoaringBitmap::new(),
            deletes: RoaringBitmap::new(),
            changes: RoaringTreemap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.inserts.is_empty()
            && self.updates.is_empty()
            && self.deletes.is_empty()
            && self.changes.is_empty()
    }

    pub fn deserialize(&mut self, change_id: ChangeId, bytes: &[u8]) -> Option<()> {
        let mut bytes_it = bytes.iter();
        let mut total_inserts = usize::from_leb128_it(&mut bytes_it)?;
        let mut total_updates = usize::from_leb128_it(&mut bytes_it)?;
        let mut total_deletes = usize::from_leb128_it(&mut bytes_it)?;

        let mut inserted_ids = Vec::with_capacity(total_inserts);

        while total_inserts > 0 {
            inserted_ids.push(JMAPId::from_leb128_it(&mut bytes_it)?);
            total_inserts -= 1;
        }

        while total_updates > 0 {
            let document_id = JMAPId::from_leb128_it(&mut bytes_it)?.get_document_id();
            if !self.inserts.contains(document_id) {
                self.updates.push(document_id);
            }

            total_updates -= 1;
        }

        while total_deletes > 0 {
            let deleted_id = JMAPId::from_leb128_it(&mut bytes_it)?;
            let document_id = deleted_id.get_document_id();
            let prefix_id = deleted_id.get_prefix_id();
            if let Some(pos) = inserted_ids.iter().position(|&inserted_id| {
                inserted_id.get_document_id() == document_id
                    && inserted_id.get_prefix_id() != prefix_id
            }) {
                // There was a prefix change, count this change as an update.
                inserted_ids.remove(pos);
                if !self.inserts.contains(document_id) {
                    self.updates.push(document_id);
                }
            } else {
                // This change is an actual deletion
                if !self.inserts.remove(document_id) {
                    self.deletes.push(document_id);
                }
                self.updates.remove(document_id);
            }
            total_deletes -= 1;
        }

        for inserted_id in inserted_ids {
            let document_id = inserted_id.get_document_id();
            self.inserts.insert(document_id);
            // IDs can be reused
            self.deletes.remove(document_id);
        }

        self.changes.push(change_id);

        Some(())
    }
}

impl<T> JMAPStore<T>
where
    T: for<'x> Store<'x> + 'static,
{
    pub fn assign_raft_id(&self) -> RaftId {
        RaftId {
            term: self.raft_log_term.load(Ordering::Relaxed),
            index: self.raft_log_index.fetch_add(1, Ordering::Relaxed),
        }
    }

    pub fn get_prev_raft_id(&self, key: RaftId) -> crate::Result<Option<RaftId>> {
        let key = key.serialize_key();
        let key_len = key.len();

        if let Some((key, _)) = self
            .db
            .iterator(ColumnFamily::Logs, &key, Direction::Backward)?
            .next()
        {
            if key.len() == key_len && key[0] == INTERNAL_KEY_PREFIX {
                return Ok(Some(RaftId::deserialize_key(&key).ok_or_else(|| {
                    StoreError::InternalError(format!("Corrupted raft key for [{:?}]", key))
                })?));
            }
        }
        Ok(None)
    }

    pub fn get_next_raft_id(&self, key: RaftId) -> crate::Result<Option<RaftId>> {
        let key = key.serialize_key();
        let key_len = key.len();

        if let Some((key, _)) = self
            .db
            .iterator(ColumnFamily::Logs, &key, Direction::Forward)?
            .next()
        {
            if key.len() == key_len && key[0] == INTERNAL_KEY_PREFIX {
                return Ok(Some(RaftId::deserialize_key(&key).ok_or_else(|| {
                    StoreError::InternalError(format!("Corrupted raft key for [{:?}]", key))
                })?));
            }
        }
        Ok(None)
    }

    pub fn get_raft_entries(
        &self,
        from_raft_id: RaftId,
        num_entries: usize,
    ) -> crate::Result<Vec<Entry>> {
        let mut entries = Vec::with_capacity(num_entries);
        let (is_inclusive, key) = if !from_raft_id.is_none() {
            (false, from_raft_id.serialize_key())
        } else {
            (true, RaftId::new(0, 0).serialize_key())
        };
        let key_len = key.len();

        for (key, value) in self
            .db
            .iterator(ColumnFamily::Logs, &key, Direction::Forward)?
        {
            if key.len() == key_len && key[0] == INTERNAL_KEY_PREFIX {
                let raft_id = RaftId::deserialize_key(&key).ok_or_else(|| {
                    StoreError::InternalError(format!("Corrupted raft entry for [{:?}]", key))
                })?;
                if is_inclusive || raft_id != from_raft_id {
                    entries.push(Entry::deserialize(&value, raft_id).ok_or_else(|| {
                        StoreError::InternalError(format!("Corrupted raft entry for [{:?}]", key))
                    })?);
                    if entries.len() == num_entries {
                        break;
                    }
                }
            } else {
                break;
            }
        }
        Ok(entries)
    }

    pub fn insert_raft_entries(&self, entries: Vec<Entry>) -> crate::Result<()> {
        self.db.write(
            entries
                .into_iter()
                .map(|entry| {
                    WriteOperation::set(
                        ColumnFamily::Logs,
                        entry.raft_id.serialize_key(),
                        entry.serialize().unwrap(),
                    )
                })
                .collect(),
        )
    }

    /*pub fn get_raft_entry(&self, raft_id: RaftId) -> crate::Result<Option<Entry>> {
        let key = raft_id.serialize_key();
        let key_len = key.len();

        if let Some((key, value)) = self
            .db
            .iterator(ColumnFamily::Logs, &key, Direction::Forward)?
            .next()
        {
            if key.len() == key_len && key[0] == INTERNAL_KEY_PREFIX {
                return Ok(Some(
                    Entry::deserialize(&value, RaftId::deserialize_key(key)?).ok_or_else(|| {
                        StoreError::InternalError(format!("Corrupted raft entry for [{:?}]", key))
                    })?,
                ));
            }
        }
        Ok(None)
    }*/

    pub fn get_pending_changes(
        &self,
        account: AccountId,
        collection: Collection,
        from_change_id: Option<ChangeId>,
        only_ids: bool,
    ) -> crate::Result<PendingChanges> {
        let mut changes = PendingChanges::new(account, collection);

        let (is_inclusive, from_change_id) = if let Some(from_change_id) = from_change_id {
            (true, from_change_id)
        } else {
            (false, 0)
        };

        let key = changes::Entry::serialize_key(account, collection, from_change_id);
        let key_len = key.len();
        let prefix = &key[0..COLLECTION_PREFIX_LEN];

        for (key, value) in self
            .db
            .iterator(ColumnFamily::Logs, &key, Direction::Forward)?
        {
            if !key.starts_with(prefix) {
                break;
            } else if key.len() != key_len {
                //TODO avoid collisions with Raft keys
                continue;
            }
            let change_id = key
                .as_ref()
                .deserialize_be_u64(COLLECTION_PREFIX_LEN)
                .ok_or_else(|| {
                    StoreError::InternalError(format!(
                        "Failed to deserialize changelog key for [{}/{:?}]: [{:?}]",
                        account, collection, key
                    ))
                })?;

            if change_id > from_change_id || (is_inclusive && change_id == from_change_id) {
                if !only_ids {
                    changes.deserialize(change_id, &value).ok_or_else(|| {
                        StoreError::InternalError(format!(
                            "Failed to deserialize raft changes for [{}/{:?}]",
                            account, collection
                        ))
                    })?;
                } else {
                    changes.changes.push(change_id);
                }
            }
        }

        Ok(changes)
    }
}
