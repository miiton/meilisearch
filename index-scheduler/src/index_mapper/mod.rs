use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use std::{fs, thread};

use log::error;
use meilisearch_types::heed::types::{SerdeJson, Str};
use meilisearch_types::heed::{Database, Env, RoTxn, RwTxn};
use meilisearch_types::milli::update::IndexerConfig;
use meilisearch_types::milli::{FieldDistribution, Index};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use self::index_map::IndexMap;
use self::IndexStatus::{Available, BeingDeleted, Closing, Missing};
use crate::uuid_codec::UuidCodec;
use crate::{Error, Result};

mod index_map;

const INDEX_MAPPING: &str = "index-mapping";
const INDEX_STATS: &str = "index-stats";

/// Structure managing meilisearch's indexes.
///
/// It is responsible for:
/// 1. Creating new indexes
/// 2. Opening indexes and storing references to these opened indexes
/// 3. Accessing indexes through their uuid
/// 4. Mapping a user-defined name to each index uuid.
///
/// # Implementation notes
///
/// An index exists as 3 bits of data:
/// 1. The index data on disk, that can exist in 3 states: Missing, Present, or BeingDeleted.
/// 2. The persistent database containing the association between the index' name and its UUID,
///    that can exist in 2 states: Missing or Present.
/// 3. The state of the index in the in-memory `IndexMap`, that can exist in multiple states:
///   - Missing
///   - Available
///   - Closing (because an index needs resizing or was evicted from the cache)
///   - BeingDeleted
///
/// All of this data should be kept consistent between index operations, which is achieved by the `IndexMapper`
/// with the use of the following primitives:
/// - A RwLock on the `IndexMap`.
/// - Transactions on the association database.
/// - ClosingEvent signals emitted when closing an environment.
#[derive(Clone)]
pub struct IndexMapper {
    /// Keep track of the opened indexes. Used mainly by the index resolver.
    index_map: Arc<RwLock<IndexMap>>,

    /// Map an index name with an index uuid currently available on disk.
    pub(crate) index_mapping: Database<Str, UuidCodec>,
    /// Map an index UUID with the cached stats associated to the index.
    ///
    /// Using an UUID forces to use the index_mapping table to recover the index behind a name, ensuring
    /// consistency wrt index swapping.
    pub(crate) index_stats: Database<UuidCodec, SerdeJson<IndexStats>>,

    /// Path to the folder where the LMDB environments of each index are.
    base_path: PathBuf,
    /// The map size an index is opened with on the first time.
    index_base_map_size: usize,
    /// The quantity by which the map size of an index is incremented upon reopening, in bytes.
    index_growth_amount: usize,
    /// Weither we open a meilisearch index with the MDB_WRITEMAP option or not.
    enable_mdb_writemap: bool,
    pub indexer_config: Arc<IndexerConfig>,
}

/// Whether the index is available for use or is forbidden to be inserted back in the index map
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum IndexStatus {
    /// Not currently in the index map.
    Missing,
    /// Do not insert it back in the index map as it is currently being deleted.
    BeingDeleted,
    /// Temporarily do not insert the index in the index map as it is currently being resized/evicted from the map.
    Closing(index_map::ClosingIndex),
    /// You can use the index without worrying about anything.
    Available(Index),
}

/// The statistics that can be computed from an `Index` object.
#[derive(Serialize, Deserialize, Debug)]
pub struct IndexStats {
    /// Number of documents in the index.
    pub number_of_documents: u64,
    /// Size of the index' DB, in bytes.
    pub database_size: u64,
    /// Association of every field name with the number of times it occurs in the documents.
    pub field_distribution: FieldDistribution,
    /// Creation date of the index.
    pub created_at: OffsetDateTime,
    /// Date of the last update of the index.
    pub updated_at: OffsetDateTime,
}

impl IndexStats {
    /// Compute the stats of an index
    ///
    /// # Parameters
    ///
    /// - rtxn: a RO transaction for the index, obtained from `Index::read_txn()`.
    pub fn new(index: &Index, rtxn: &RoTxn) -> Result<Self> {
        let database_size = index.on_disk_size()?;
        Ok(IndexStats {
            number_of_documents: index.number_of_documents(rtxn)?,
            database_size,
            field_distribution: index.field_distribution(rtxn)?,
            created_at: index.created_at(rtxn)?,
            updated_at: index.updated_at(rtxn)?,
        })
    }
}

impl IndexMapper {
    pub fn new(
        env: &Env,
        base_path: PathBuf,
        index_base_map_size: usize,
        index_growth_amount: usize,
        index_count: usize,
        enable_mdb_writemap: bool,
        indexer_config: IndexerConfig,
    ) -> Result<Self> {
        let mut wtxn = env.write_txn()?;
        let index_mapping = env.create_database(&mut wtxn, Some(INDEX_MAPPING))?;
        let index_stats = env.create_database(&mut wtxn, Some(INDEX_STATS))?;
        wtxn.commit()?;

        Ok(Self {
            index_map: Arc::new(RwLock::new(IndexMap::new(index_count))),
            index_mapping,
            index_stats,
            base_path,
            index_base_map_size,
            index_growth_amount,
            enable_mdb_writemap,
            indexer_config: Arc::new(indexer_config),
        })
    }

    /// Get or create the index.
    pub fn create_index(
        &self,
        mut wtxn: RwTxn,
        name: &str,
        date: Option<(OffsetDateTime, OffsetDateTime)>,
    ) -> Result<Index> {
        match self.index(&wtxn, name) {
            Ok(index) => {
                wtxn.commit()?;
                Ok(index)
            }
            Err(Error::IndexNotFound(_)) => {
                let uuid = Uuid::new_v4();
                self.index_mapping.put(&mut wtxn, name, &uuid)?;

                let index_path = self.base_path.join(uuid.to_string());
                fs::create_dir_all(&index_path)?;

                // Error if the UUIDv4 somehow already exists in the map, since it should be fresh.
                // This is very unlikely to happen in practice.
                // TODO: it would be better to lazily create the index. But we need an Index::open function for milli.
                let index = self.index_map.write().unwrap().create(
                    &uuid,
                    &index_path,
                    date,
                    self.enable_mdb_writemap,
                    self.index_base_map_size,
                )?;

                wtxn.commit()?;

                Ok(index)
            }
            error => error,
        }
    }

    /// Removes the index from the mapping table and the in-memory index map
    /// but keeps the associated tasks.
    pub fn delete_index(&self, mut wtxn: RwTxn, name: &str) -> Result<()> {
        let uuid = self
            .index_mapping
            .get(&wtxn, name)?
            .ok_or_else(|| Error::IndexNotFound(name.to_string()))?;

        // Not an error if the index had no stats in cache.
        self.index_stats.delete(&mut wtxn, &uuid)?;

        // Once we retrieved the UUID of the index we remove it from the mapping table.
        assert!(self.index_mapping.delete(&mut wtxn, name)?);

        wtxn.commit()?;

        let mut tries = 0;
        // Attempts to remove the index from the in-memory index map in a loop.
        //
        // If the index is currently being closed, we will wait for it to be closed and retry getting it in a subsequent
        // loop iteration.
        //
        // We make 100 attempts before giving up.
        // This could happen in the following situations:
        //
        // 1. There is a bug preventing the index from being correctly closed, or us from detecting this.
        // 2. A user of the index is keeping it open for more than 600 seconds. This could happen e.g. during a pathological search.
        //    This can not be caused by indexation because deleting an index happens in the scheduler itself, so cannot be concurrent with indexation.
        //
        // In these situations, reporting the error through a panic is in order.
        let closing_event = loop {
            let mut lock = self.index_map.write().unwrap();
            match lock.start_deletion(&uuid) {
                Ok(env_closing) => break env_closing,
                Err(Some(reopen)) => {
                    // drop the lock here so that we don't synchronously wait for the index to close.
                    drop(lock);
                    tries += 1;
                    if tries >= 100 {
                        panic!("Too many attempts to close index {name} prior to deletion.")
                    }
                    let reopen = if let Some(reopen) = reopen.wait_timeout(Duration::from_secs(6)) {
                        reopen
                    } else {
                        continue;
                    };
                    reopen.close(&mut self.index_map.write().unwrap());
                    continue;
                }
                Err(None) => return Ok(()),
            }
        };

        let index_map = self.index_map.clone();
        let index_path = self.base_path.join(uuid.to_string());
        let index_name = name.to_string();
        thread::Builder::new()
            .name(String::from("index_deleter"))
            .spawn(move || {
                // We first wait to be sure that the previously opened index is effectively closed.
                // This can take a lot of time, this is why we do that in a separate thread.
                if let Some(closing_event) = closing_event {
                    closing_event.wait();
                }

                // Then we remove the content from disk.
                if let Err(e) = fs::remove_dir_all(&index_path) {
                    error!(
                        "An error happened when deleting the index {} ({}): {}",
                        index_name, uuid, e
                    );
                }

                // Finally we remove the entry from the index map.
                index_map.write().unwrap().end_deletion(&uuid);
            })
            .unwrap();

        Ok(())
    }

    pub fn exists(&self, rtxn: &RoTxn, name: &str) -> Result<bool> {
        Ok(self.index_mapping.get(rtxn, name)?.is_some())
    }

    /// Resizes the maximum size of the specified index to the double of its current maximum size.
    ///
    /// This operation involves closing the underlying environment and so can take a long time to complete.
    ///
    /// # Panics
    ///
    /// - If the Index corresponding to the passed name is concurrently being deleted/resized or cannot be found in the
    ///   in memory hash map.
    pub fn resize_index(&self, rtxn: &RoTxn, name: &str) -> Result<()> {
        let uuid = self
            .index_mapping
            .get(rtxn, name)?
            .ok_or_else(|| Error::IndexNotFound(name.to_string()))?;

        // We remove the index from the in-memory index map.
        self.index_map.write().unwrap().close_for_resize(
            &uuid,
            self.enable_mdb_writemap,
            self.index_growth_amount,
        );

        Ok(())
    }

    /// Return an index, may open it if it wasn't already opened.
    pub fn index(&self, rtxn: &RoTxn, name: &str) -> Result<Index> {
        let uuid = self
            .index_mapping
            .get(rtxn, name)?
            .ok_or_else(|| Error::IndexNotFound(name.to_string()))?;

        let mut tries = 0;
        // attempts to open the index in a loop.
        //
        // If the index is currently being closed, we will wait for it to be closed and retry getting it in a subsequent
        // loop iteration.
        //
        // We make 100 attempts before giving up.
        // This could happen in the following situations:
        //
        // 1. There is a bug preventing the index from being correctly closed, or us from detecting it was.
        // 2. A user of the index is keeping it open for more than 600 seconds. This could happen e.g. during a long indexation,
        //    a pathological search, and so on.
        //
        // In these situations, reporting the error through a panic is in order.
        let index = loop {
            tries += 1;
            if tries > 100 {
                panic!("Too many spurious wake ups while trying to open the index {name}");
            }

            // we get the index here to drop the lock before entering the match
            let index = self.index_map.read().unwrap().get(&uuid);

            match index {
                Available(index) => break index,
                Closing(reopen) => {
                    // Avoiding deadlocks: no lock taken while doing this operation.
                    let reopen = if let Some(reopen) = reopen.wait_timeout(Duration::from_secs(6)) {
                        reopen
                    } else {
                        continue;
                    };
                    let index_path = self.base_path.join(uuid.to_string());
                    // take the lock to reopen the environment.
                    reopen.reopen(&mut self.index_map.write().unwrap(), &index_path)?;
                    continue;
                }
                BeingDeleted => return Err(Error::IndexNotFound(name.to_string())),
                // since we're lazy, it's possible that the index has not been opened yet.
                Missing => {
                    let mut index_map = self.index_map.write().unwrap();
                    // between the read lock and the write lock it's not impossible
                    // that someone already opened the index (eg if two searches happen
                    // at the same time), thus before opening it we check a second time
                    // if it's not already there.
                    match index_map.get(&uuid) {
                        Missing => {
                            let index_path = self.base_path.join(uuid.to_string());

                            break index_map.create(
                                &uuid,
                                &index_path,
                                None,
                                self.enable_mdb_writemap,
                                self.index_base_map_size,
                            )?;
                        }
                        Available(index) => break index,
                        Closing(_) => {
                            // the reopening will be handled in the next loop operation
                            continue;
                        }
                        BeingDeleted => return Err(Error::IndexNotFound(name.to_string())),
                    }
                }
            }
        };

        Ok(index)
    }

    /// Attempts `f` for each index that exists in the index mapper.
    ///
    /// It is preferable to use this function rather than a loop that opens all indexes, as a way to avoid having all indexes opened,
    /// which is unsupported in general.
    ///
    /// Since `f` is allowed to return a result, and `Index` is cloneable, it is still possible to wrongly build e.g. a vector of
    /// all the indexes, but this function makes it harder and so less likely to do accidentally.
    pub fn try_for_each_index<U, V>(
        &self,
        rtxn: &RoTxn,
        mut f: impl FnMut(&str, &Index) -> Result<U>,
    ) -> Result<V>
    where
        V: FromIterator<U>,
    {
        self.index_mapping
            .iter(rtxn)?
            .map(|res| {
                res.map_err(Error::from)
                    .and_then(|(name, _)| self.index(rtxn, name).and_then(|index| f(name, &index)))
            })
            .collect()
    }

    /// Return the name of all indexes without opening them.
    pub fn index_names(&self, rtxn: &RoTxn) -> Result<Vec<String>> {
        self.index_mapping
            .iter(rtxn)?
            .map(|res| res.map_err(Error::from).map(|(name, _)| name.to_string()))
            .collect()
    }

    /// Swap two index names.
    pub fn swap(&self, wtxn: &mut RwTxn, lhs: &str, rhs: &str) -> Result<()> {
        let lhs_uuid = self
            .index_mapping
            .get(wtxn, lhs)?
            .ok_or_else(|| Error::IndexNotFound(lhs.to_string()))?;
        let rhs_uuid = self
            .index_mapping
            .get(wtxn, rhs)?
            .ok_or_else(|| Error::IndexNotFound(rhs.to_string()))?;

        self.index_mapping.put(wtxn, lhs, &rhs_uuid)?;
        self.index_mapping.put(wtxn, rhs, &lhs_uuid)?;

        Ok(())
    }

    /// The stats of an index.
    ///
    /// If available in the cache, they are directly returned.
    /// Otherwise, the `Index` is opened to compute the stats on the fly (the result is not cached).
    /// The stats for an index are cached after each `Index` update.
    pub fn stats_of(&self, rtxn: &RoTxn, index_uid: &str) -> Result<IndexStats> {
        let uuid = self
            .index_mapping
            .get(rtxn, index_uid)?
            .ok_or_else(|| Error::IndexNotFound(index_uid.to_string()))?;

        match self.index_stats.get(rtxn, &uuid)? {
            Some(stats) => Ok(stats),
            None => {
                let index = self.index(rtxn, index_uid)?;
                let index_rtxn = index.read_txn()?;
                IndexStats::new(&index, &index_rtxn)
            }
        }
    }

    /// Stores the new stats for an index.
    ///
    /// Expected usage is to compute the stats the index using `IndexStats::new`, the pass it to this function.
    pub fn store_stats_of(
        &self,
        wtxn: &mut RwTxn,
        index_uid: &str,
        stats: &IndexStats,
    ) -> Result<()> {
        let uuid = self
            .index_mapping
            .get(wtxn, index_uid)?
            .ok_or_else(|| Error::IndexNotFound(index_uid.to_string()))?;

        self.index_stats.put(wtxn, &uuid, stats)?;
        Ok(())
    }

    pub fn index_exists(&self, rtxn: &RoTxn, name: &str) -> Result<bool> {
        Ok(self.index_mapping.get(rtxn, name)?.is_some())
    }

    pub fn indexer_config(&self) -> &IndexerConfig {
        &self.indexer_config
    }
}
