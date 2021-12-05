pub mod bitmaps;
pub mod document_id;
pub mod get;
pub mod insert;
pub mod iterator;
pub mod query;
pub mod tag;
pub mod term;

use std::{collections::HashSet, sync::Mutex};

use bitmaps::{bitmap_full_merge, bitmap_partial_merge};
use dashmap::DashMap;
use document_id::DocumentIdAssigner;
use rocksdb::{ColumnFamilyDescriptor, DBWithThreadMode, MultiThreaded, Options};
use store::{AccountId, CollectionId, DocumentId, Result, Store, StoreError, TermId};
use term::get_last_term_id;

pub struct RocksDBStore {
    db: DBWithThreadMode<MultiThreaded>,
    id_assigner: DashMap<(AccountId, CollectionId), DocumentIdAssigner>,
    term_id_lock: DashMap<String, (TermId, u32)>,
    term_id_last: Mutex<u64>,
}

impl RocksDBStore {
    pub fn open(path: &str) -> Result<RocksDBStore> {
        // Bitmaps
        let cf_bitmaps = {
            let mut cf_opts = Options::default();
            //cf_opts.set_max_write_buffer_number(16);
            cf_opts.set_merge_operator("bitmap merge", bitmap_full_merge, bitmap_partial_merge);
            ColumnFamilyDescriptor::new("bitmaps", cf_opts)
        };

        // Stored values
        let cf_values = {
            let cf_opts = Options::default();
            ColumnFamilyDescriptor::new("values", cf_opts)
        };

        // Secondary indexes
        let cf_indexes = {
            let cf_opts = Options::default();
            ColumnFamilyDescriptor::new("indexes", cf_opts)
        };

        // Term index
        let cf_terms = {
            let cf_opts = Options::default();
            ColumnFamilyDescriptor::new("terms", cf_opts)
        };

        let mut db_opts = Options::default();
        db_opts.create_missing_column_families(true);
        db_opts.create_if_missing(true);

        let db: DBWithThreadMode<MultiThreaded> = DBWithThreadMode::open_cf_descriptors(
            &db_opts,
            path,
            vec![cf_bitmaps, cf_values, cf_indexes, cf_terms],
        )
        .map_err(|e| StoreError::InternalError(e.into_string()))?;

        Ok(Self {
            id_assigner: DashMap::new(),
            term_id_lock: DashMap::new(),
            term_id_last: Mutex::new(get_last_term_id(&db)?),
            db,
        })
    }
}

impl<T: IntoIterator<Item = DocumentId>> Store<T> for RocksDBStore where
    RocksDBStore: store::StoreQuery<T>
{
}

#[cfg(test)]
mod tests {
    use store_test::insert_artworks;

    use crate::RocksDBStore;

    #[test]
    fn rocksdb_test() {
        let mut temp_dir = std::env::temp_dir();
        temp_dir.push("strdb_query_test");
        if temp_dir.exists() {
            std::fs::remove_dir_all(&temp_dir).unwrap();
        }

        insert_artworks(RocksDBStore::open(temp_dir.to_str().unwrap()).unwrap());
        println!("Done!");
    }
}
