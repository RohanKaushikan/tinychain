use std::fmt;
use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use b_table::{Schema, TableLock};
use destream::de;
use ds_ext::{OrdHashMap, OrdHashSet};
use freqfs::DirLock;
use futures::{future, try_join, TryFutureExt, TryStreamExt};
use log::{debug, trace};
use safecast::AsType;

use tc_error::*;
use tc_transact::fs::{CopyFrom, Dir, Inner, Persist, Restore, VERSIONS};
use tc_transact::{fs, Transact, Transaction, TxnId};
use tc_value::{Value, ValueCollator};
use tcgeneric::{label, Id, Instance, Label, Map, TCBoxTryStream, ThreadSafe};

use crate::btree::{BTreeSchema as IndexSchema, BTreeSchema, Node};
use crate::finalize_dir;
use crate::table::TableUpdate;

use super::stream::Rows;
use super::view::{Limited, Selection, TableSlice as Slice};
use super::{
    Key, Range, Row, TableInstance, TableOrder, TableRead, TableSchema, TableSlice, TableStream,
    TableType, TableWrite, Values,
};

const CANON: Label = label("canon");
const DELETES: Label = label("deletes");
const INSERTS: Label = label("inserts");
const COMMITTED: Label = label("committed");

type Version<FE> = TableLock<TableSchema, IndexSchema, ValueCollator, FE>;
type VersionReadGuard<FE> = b_table::TableReadGuard<TableSchema, IndexSchema, ValueCollator, FE>;
type VersionWriteGuard<FE> = b_table::TableWriteGuard<TableSchema, IndexSchema, ValueCollator, FE>;

type Semaphore = tc_transact::lock::Semaphore<ValueCollator, Range>;

struct Delta<FE> {
    dir: DirLock<FE>,
    deletes: Version<FE>,
    inserts: Version<FE>,
}

impl<FE> Clone for Delta<FE> {
    fn clone(&self) -> Self {
        Self {
            dir: self.dir.clone(),
            deletes: self.deletes.clone(),
            inserts: self.inserts.clone(),
        }
    }
}

// TODO: should this code be consolidated with b_tree::Delta?
impl<FE> Delta<FE>
where
    FE: AsType<Node> + ThreadSafe,
{
    fn create(schema: TableSchema, collator: ValueCollator, dir: DirLock<FE>) -> TCResult<Self> {
        let (deletes, inserts) = {
            let mut dir = dir.try_write()?;
            let deletes = dir.create_dir(DELETES.to_string())?;
            let inserts = dir.create_dir(INSERTS.to_string())?;
            (deletes, inserts)
        };

        Ok(Self {
            dir,
            deletes: Version::create(schema.clone(), collator.clone(), deletes)?,
            inserts: Version::create(schema, collator, inserts)?,
        })
    }

    fn load(schema: TableSchema, collator: ValueCollator, dir: DirLock<FE>) -> TCResult<Self> {
        let (deletes, inserts) = {
            let mut dir = dir.try_write()?;
            debug_assert!(!dir.is_empty(), "failed to sync committed version");
            let deletes = dir.get_or_create_dir(DELETES.to_string())?;
            let inserts = dir.get_or_create_dir(INSERTS.to_string())?;
            (deletes, inserts)
        };

        Ok(Self {
            dir,
            deletes: Version::load(schema.clone(), collator.clone(), deletes)?,
            inserts: Version::load(schema, collator, inserts)?,
        })
    }

    fn load_copy(source: &Self, dir: DirLock<FE>) -> TCResult<Self> {
        let (deletes, inserts) = {
            let dir = dir.try_read()?;

            let deletes = dir
                .get_dir(&*DELETES)
                .cloned()
                .ok_or_else(|| TCError::not_found(DELETES))?;

            let inserts = dir
                .get_dir(&*INSERTS)
                .cloned()
                .ok_or_else(|| TCError::not_found(INSERTS))?;

            (deletes, inserts)
        };

        let deletes = Version::load(
            source.deletes.schema().clone(),
            source.deletes.collator().inner().clone(),
            deletes,
        )?;

        let inserts = Version::load(
            source.inserts.schema().clone(),
            source.inserts.collator().inner().clone(),
            inserts,
        )?;

        Ok(Self {
            dir,
            deletes,
            inserts,
        })
    }

    fn dir(&self) -> &DirLock<FE> {
        &self.dir
    }

    async fn read(self) -> (VersionReadGuard<FE>, VersionReadGuard<FE>) {
        // acquire these locks in order to avoid the risk of a deadlock
        let inserts = self.inserts.into_read().await;
        let deletes = self.deletes.into_read().await;
        (inserts, deletes)
    }

    async fn write(self) -> (VersionWriteGuard<FE>, VersionWriteGuard<FE>) {
        // acquire these locks in order to avoid the risk of a deadlock
        let inserts = self.inserts.into_write().await;
        let deletes = self.deletes.into_write().await;
        (inserts, deletes)
    }

    async fn merge_into<'a>(
        self,
        mut rows: TCBoxTryStream<'a, Row>,
        collator: b_tree::Collator<ValueCollator>,
        range: b_table::Range<Id, Value>,
        order: &[Id],
        reverse: bool,
    ) -> TCResult<TCBoxTryStream<'a, Key>> {
        let inserted = {
            let inserts = self.inserts.read().await;
            let inserted = inserts.rows(range.clone(), order, reverse, None)?;
            inserted.map_err(TCError::from)
        };

        rows = Box::pin(collate::try_merge(collator.clone(), rows, inserted));

        let deleted = {
            let deletes = self.deletes.read().await;
            let deleted = deletes.rows(range, order, reverse, None)?;
            deleted.map_err(TCError::from)
        };

        rows = Box::pin(collate::try_diff(collator.clone(), rows, deleted));

        Ok(rows)
    }

    async fn commit(&self)
    where
        FE: for<'a> fs::FileSave<'a>,
    {
        try_join!(self.inserts.sync(), self.deletes.sync()).expect("commit");
    }
}

struct State<FE> {
    commits: OrdHashSet<TxnId>,
    deltas: OrdHashMap<TxnId, Delta<FE>>,
    pending: OrdHashMap<TxnId, Delta<FE>>,
    finalized: Option<TxnId>,
}

impl<FE> State<FE>
where
    FE: AsType<Node> + ThreadSafe,
{
    #[inline]
    fn pending_version(
        &mut self,
        txn_id: TxnId,
        dir: &freqfs::Dir<FE>,
        schema: &TableSchema,
        collator: &ValueCollator,
    ) -> TCResult<Delta<FE>> {
        if let Some(version) = self.pending.get(&txn_id) {
            debug_assert!(!self.commits.contains(&txn_id));
            Ok(version.clone())
        } else if self.commits.contains(&txn_id) {
            Err(conflict!("{} has already been committed", txn_id))
        } else if self.finalized.as_ref() > Some(&txn_id) {
            Err(conflict!("{} has already been finalized", txn_id))
        } else {
            let dir = {
                let pending = dir
                    .get_dir(VERSIONS)
                    .ok_or_else(|| internal!("missing pending versions dir"))?;

                let mut versions = pending.try_write()?;
                versions.create_dir(txn_id.to_string())?
            };

            let version = Delta::create(schema.clone(), collator.clone(), dir)?;
            self.pending.insert(txn_id, version.clone());
            Ok(version)
        }
    }
}

/// A relational database table which supports a primary key and multiple indices
pub struct TableFile<Txn, FE> {
    dir: DirLock<FE>,
    canon: Version<FE>,
    state: Arc<RwLock<State<FE>>>,
    semaphore: Semaphore,
    phantom: PhantomData<Txn>,
}

impl<Txn, FE> Clone for TableFile<Txn, FE> {
    fn clone(&self) -> Self {
        Self {
            dir: self.dir.clone(),
            canon: self.canon.clone(),
            state: self.state.clone(),
            semaphore: self.semaphore.clone(),
            phantom: PhantomData,
        }
    }
}

impl<Txn, FE> TableFile<Txn, FE>
where
    Txn: Transaction<FE>,
    FE: AsType<Node> + ThreadSafe,
{
    fn new(dir: DirLock<FE>, canon: Version<FE>, committed: DirLock<FE>) -> TCResult<Self> {
        let semaphore = Semaphore::new(Arc::new(canon.collator().inner().clone()));

        let deltas = {
            let mut deltas = OrdHashMap::new();

            let committed = committed.try_read()?;

            debug!(
                "found {} committed table versions pending merge",
                committed.len()
            );

            for (name, version) in committed.iter() {
                if name.starts_with('.') {
                    trace!("skip hidden commit dir entry {name}");
                    continue;
                }

                let version = version
                    .as_dir()
                    .cloned()
                    .ok_or_else(|| internal!("expected a table version dir but found a file"))?;

                let schema = canon.schema().clone();
                let collator = canon.collator().inner().clone();
                let version = Delta::load(schema, collator, version)?;

                deltas.insert(name.parse()?, version);
            }

            deltas
        };

        let state = State {
            commits: deltas.keys().copied().collect(),
            deltas,
            pending: OrdHashMap::new(),
            finalized: None,
        };

        Ok(Self {
            dir,
            state: Arc::new(RwLock::new(state)),
            canon,
            semaphore,
            phantom: PhantomData,
        })
    }
}

impl<Txn, FE> TableFile<Txn, FE>
where
    Txn: Transaction<FE>,
    FE: AsType<Node> + ThreadSafe,
{
    async fn into_rows<'a>(
        self,
        txn_id: TxnId,
        range: Range,
        order: Vec<Id>,
        reverse: bool,
    ) -> TCResult<TCBoxTryStream<'a, Row>> {
        debug!(
            "TableFile::into_rows: {:?} ordered by {:?} (reversed: {}) at {}",
            range, order, reverse, txn_id
        );

        let collator = (&**self.canon.collator()).clone();

        // read-lock the canonical version BEFORE locking self.state,
        // to avoid a deadlock or conflict with Self::finalize
        let mut rows: TCBoxTryStream<'static, Key> = {
            let table = self.canon.read().await;
            let rows = table.rows(range.clone(), &order, reverse, None)?;
            Box::pin(rows.map_err(TCError::from))
        };

        trace!("got canon rows");

        let (deltas, pending) = {
            let state = self.state.read().expect("state");
            let deltas = state
                .deltas
                .iter()
                .take_while(|(id, _)| *id <= &txn_id)
                .map(|(_, delta)| delta)
                .cloned()
                .collect::<Vec<_>>();

            let pending = state.pending.get(&txn_id).cloned();

            (deltas, pending)
        };

        trace!("merging {} committed deltas...", deltas.len());

        for delta in deltas {
            rows = delta
                .merge_into(rows, collator.clone(), range.clone(), &order, reverse)
                .await?;
        }

        trace!("merged committed deltas");

        if let Some(pending) = pending {
            trace!("merging pending delta");

            rows = pending
                .merge_into(rows, collator.clone(), range.clone(), &order, reverse)
                .await?;

            trace!("merged pending deltas");
        }

        Ok(rows)
    }

    pub(super) async fn into_stream<'a>(
        self,
        txn_id: TxnId,
        range: Range,
        order: Vec<Id>,
        reverse: bool,
    ) -> TCResult<Rows<'a>> {
        debug!(
            "TableFile::into_stream: {:?} ordered by {:?} (reversed: {}) at {}",
            range, order, reverse, txn_id
        );

        let permit = self.semaphore.read(txn_id, range.clone()).await?;

        trace!("got read permit for {:?}", *permit);

        let keys = self
            .into_rows(txn_id, permit.deref().clone(), order, reverse)
            .await?;

        Ok(Rows::new(permit, keys))
    }

    pub(super) fn collator(&self) -> &b_tree::Collator<ValueCollator> {
        self.canon.collator()
    }
}

impl<Txn, FE> Instance for TableFile<Txn, FE>
where
    Self: Send + Sync,
{
    type Class = TableType;

    fn class(&self) -> Self::Class {
        TableType::Table
    }
}

impl<Txn, FE> TableInstance for TableFile<Txn, FE>
where
    Txn: Transaction<FE>,
    FE: AsType<Node> + ThreadSafe,
{
    fn schema(&self) -> &TableSchema {
        self.canon.schema()
    }
}

impl<Txn, FE> TableOrder for TableFile<Txn, FE>
where
    Txn: Transaction<FE>,
    FE: AsType<Node> + ThreadSafe,
{
    type OrderBy = Slice<Txn, FE>;
    type Reverse = Slice<Txn, FE>;

    fn order_by(self, columns: Vec<Id>, reverse: bool) -> TCResult<Self::OrderBy> {
        Slice::new(self, Range::default(), columns, reverse)
    }

    fn reverse(self) -> TCResult<Self::Reverse> {
        Slice::new(self, Range::default(), vec![], true)
    }
}

#[async_trait]
impl<Txn, FE> TableRead for TableFile<Txn, FE>
where
    Txn: Transaction<FE>,
    FE: AsType<Node> + ThreadSafe,
{
    async fn read(&self, txn_id: TxnId, key: Key) -> TCResult<Option<Row>> {
        let key = b_table::Schema::validate_key(self.schema(), key)?;
        let range = self.schema().range_from_key(key.clone())?;
        let _permit = self.semaphore.read(txn_id, range).await?;

        let (deltas, pending) = {
            let state = self.state.read().expect("state");

            let deltas = state
                .deltas
                .iter()
                .take_while(|(id, _)| *id <= &txn_id)
                .map(|(_, delta)| delta)
                .cloned()
                .collect::<Vec<_>>();

            (deltas, state.pending.get(&txn_id).cloned())
        };

        if let Some(pending) = pending {
            let (inserted, deleted) = pending.read().await;

            if let Some(row) = inserted.get_row(key.to_vec()).await? {
                return Ok(Some(row));
            } else if deleted.contains(&key).await? {
                return Ok(None);
            }
        }

        for delta in deltas.into_iter().rev() {
            let (inserted, deleted) = delta.read().await;

            if let Some(row) = inserted.get_row(key.to_vec()).await? {
                return Ok(Some(row));
            } else if deleted.contains(&key).await? {
                return Ok(None);
            }
        }

        let canon = self.canon.read().await;
        canon.get_row(key).map_err(TCError::from).await
    }
}

impl<Txn, FE> TableSlice for TableFile<Txn, FE>
where
    Txn: Transaction<FE>,
    FE: AsType<Node> + ThreadSafe,
{
    type Slice = Slice<Txn, FE>;

    fn slice(self, range: Range) -> TCResult<Self::Slice> {
        Slice::new(self, range, vec![], false)
    }
}

#[async_trait]
impl<Txn, FE> TableStream for TableFile<Txn, FE>
where
    Txn: Transaction<FE>,
    FE: AsType<Node> + ThreadSafe,
{
    type Limit = Limited<Self>;
    type Selection = Selection<Self>;

    async fn count(self, txn_id: TxnId) -> TCResult<u64> {
        debug!("TableFile::count");

        let rows = self.rows(txn_id).await?;

        trace!("got rows to count");

        rows.try_fold(0, |count, _| future::ready(Ok(count + 1)))
            .await
    }

    fn limit(self, limit: u64) -> TCResult<Self::Limit> {
        Limited::new(self, limit)
    }

    fn select(self, columns: Vec<Id>) -> TCResult<Self::Selection> {
        Selection::new(self, columns)
    }

    async fn rows<'a>(self, txn_id: TxnId) -> TCResult<Rows<'a>> {
        self.into_stream(txn_id, Range::default(), vec![], false)
            .await
    }
}

#[async_trait]
impl<Txn, FE> TableUpdate<FE> for TableFile<Txn, FE>
where
    Txn: Transaction<FE>,
    FE: AsType<Node> + ThreadSafe,
{
    async fn truncate(
        &self,
        txn_id: TxnId,
        range: Range,
        tmp: b_tree::BTreeLock<BTreeSchema, ValueCollator, FE>,
    ) -> TCResult<()> {
        debug!("truncate range {range:?}");

        let _permit = self.semaphore.write(txn_id, range.clone()).await?;

        let key_len = self.schema().key().len();

        let mut rows = self.clone().into_rows(txn_id, range, vec![], false).await?;

        let mut truncated = tmp.write().await;
        while let Some(row) = rows.try_next().await? {
            truncated.insert(row).await?;
        }

        let pending = {
            let dir = self.dir.read().await;
            let mut state = self.state.write().expect("state");
            state.pending_version(txn_id, &*dir, self.schema(), self.canon.collator().inner())?
        };

        let (mut inserts, mut deletes) = pending.write().await;

        let truncated = truncated.downgrade();
        let mut rows = truncated.keys(b_tree::Range::default(), false);

        while let Some(mut row) = rows.try_next().await? {
            let values = row.drain(key_len..).collect();
            let key = row;

            deletes.upsert(key.to_vec(), values).await?;
            inserts.delete_row(key).await?;
        }

        Ok(())
    }

    async fn update(
        &self,
        txn_id: TxnId,
        range: Range,
        values: Map<Value>,
        tmp: b_tree::BTreeLock<BTreeSchema, ValueCollator, FE>,
    ) -> TCResult<()> {
        debug!("update values to {values:?} in range {range:?}");

        let value_columns = self.schema().values();
        if values.keys().any(|name| !value_columns.contains(name)) {
            return Err(bad_request!(
                "cannot update values {value_columns:?} with {values:?}"
            ));
        }

        let _permit = self.semaphore.write(txn_id, range.clone()).await?;

        let key_len = self.schema().key().len();
        let update_row = |mut row: Vec<Value>| {
            for (i, name) in value_columns.iter().enumerate() {
                if let Some(value) = values.get(name) {
                    row[key_len + i] = value.clone();
                }
            }

            row
        };

        let mut rows = self.clone().into_rows(txn_id, range, vec![], false).await?;

        let mut updated = tmp.write().await;
        while let Some(row) = rows.try_next().await? {
            updated.insert(update_row(row)).await?;
        }

        let pending = {
            let dir = self.dir.read().await;
            let mut state = self.state.write().expect("state");
            state.pending_version(txn_id, &*dir, self.schema(), self.canon.collator().inner())?
        };

        let (mut inserts, mut deletes) = pending.write().await;

        let updated = updated.downgrade();
        let mut rows = updated.keys(b_tree::Range::default(), false);

        while let Some(mut row) = rows.try_next().await? {
            let values = row.drain(key_len..).collect();
            let key = row;

            deletes.delete_row(key.to_vec()).await?;
            inserts.upsert(key, values).await?;
        }

        Ok(())
    }
}

#[async_trait]
impl<Txn, FE> TableWrite for TableFile<Txn, FE>
where
    Txn: Transaction<FE>,
    FE: AsType<Node> + ThreadSafe,
{
    async fn delete(&self, txn_id: TxnId, key: Key) -> TCResult<()> {
        debug!("TableFile::delete {:?}", key);

        let key = b_table::Schema::validate_key(self.schema(), key)?;
        let range = self.schema().range_from_key(key.clone())?;
        let _permit = self.semaphore.write(txn_id, range).await?;

        trace!("got write permit to delete {:?}", key);

        // read-lock the canonical version BEFORE locking self.state,
        // to avoid a deadlock or conflict with Self::finalize
        let canon = self.canon.read().await;

        let (deltas, pending) = {
            let dir = self.dir.read().await;
            let mut state = self.state.write().expect("state");

            let deltas = state
                .deltas
                .iter()
                .take_while(|(id, _)| *id < &txn_id)
                .map(|(_, delta)| delta)
                .cloned()
                .collect::<Vec<_>>();

            let pending = state.pending_version(
                txn_id,
                &*dir,
                self.schema(),
                self.canon.collator().inner(),
            )?;

            (deltas, pending)
        };

        let (mut inserts, mut deletes) = pending.write().await;

        if deletes.contains(&key).await? {
            return Ok(());
        }

        let mut row = inserts.get_row(key.to_vec()).await?;

        if row.is_none() {
            for delta in deltas {
                let (inserted, deleted) = delta.read().await;

                if deleted.contains(&key).await? {
                    return Ok(());
                } else if let Some(insert) = inserted.get_row(key.to_vec()).await? {
                    row = Some(insert);
                    break;
                }
            }
        }

        if row.is_none() {
            row = canon.get_row(key.to_vec()).await?;
        }

        if let Some(mut row) = row {
            trace!("found row {:?} to delete", row);

            let values = row.drain(key.len()..).collect();
            debug_assert_eq!(key, row[..key.len()]);

            inserts.delete_row(key.to_vec()).await?;
            deletes.upsert(key, values).await?;
        }

        Ok(())
    }

    async fn upsert(&self, txn_id: TxnId, key: Key, values: Values) -> TCResult<()> {
        let key = b_table::Schema::validate_key(self.schema(), key)?;
        let values = b_table::Schema::validate_values(self.schema(), values)?;

        let range = self.schema().range_from_key(key.clone())?;
        let _permit = self.semaphore.write(txn_id, range).await?;

        let pending = {
            let dir = self.dir.read().await;
            let mut state = self.state.write().expect("state");
            state.pending_version(txn_id, &*dir, self.schema(), self.canon.collator().inner())?
        };

        let (mut inserts, mut deletes) = pending.write().await;

        deletes.delete_row(key.to_vec()).await?;
        inserts.upsert(key, values).await?;

        Ok(())
    }
}

// TODO: can this logic be consolidated with impl Transact for BTreeFile?
#[async_trait]
impl<Txn, FE> Transact for TableFile<Txn, FE>
where
    Txn: Transaction<FE>,
    FE: AsType<Node> + ThreadSafe + for<'a> fs::FileSave<'a> + Clone,
{
    type Commit = ();

    async fn commit(&self, txn_id: TxnId) -> Self::Commit {
        debug!("Table::commit {}", txn_id);

        let pending = {
            let mut state = self.state.write().expect("state");

            if state.finalized.as_ref() > Some(&txn_id) {
                panic!("cannot commit finalized version {}", txn_id);
            } else if !state.commits.insert(txn_id) {
                // prevent any pending version being created at this txn
                assert!(state.pending.contains_key(&txn_id));
                log::warn!("duplicate commit at {}", txn_id);
                None
            } else {
                state.pending.remove(&txn_id)
            }
        };

        if let Some(pending) = pending {
            trace!("commit new version at {txn_id}");

            let committed = {
                let dir = self.dir.read().await;
                dir.get_dir(&*COMMITTED)
                    .cloned()
                    .expect("committed versions")
            };

            let mut committed = committed.write().await;

            let dir = committed
                .copy_dir_from(txn_id.to_string(), pending.dir())
                .await
                .expect("committed version copy");

            let delta = Delta::load_copy(&pending, dir).expect("committed version");
            delta.commit().await;

            self.state
                .write()
                .expect("state")
                .deltas
                .insert(txn_id, delta);
        }

        self.semaphore.finalize(&txn_id, false);
    }

    async fn rollback(&self, txn_id: &TxnId) {
        debug!("Table::rollback {}", txn_id);

        let mut state = self.state.write().expect("state");

        if state.finalized.as_ref() > Some(txn_id) {
            panic!("tried to roll back finalized version {}", txn_id);
        } else if state.commits.contains(txn_id) {
            panic!("tried to roll back committed version {}", txn_id);
        }

        state.pending.remove(txn_id);

        self.semaphore.finalize(txn_id, false);
    }

    async fn finalize(&self, txn_id: &TxnId) {
        debug!("Table::finalize {}", txn_id);

        let mut canon = self.canon.write().await;

        let deltas = {
            let mut state = self.state.write().expect("state");

            if state.finalized.as_ref() > Some(txn_id) {
                return;
            }

            let mut deltas = Vec::with_capacity(state.deltas.len());

            while let Some(version_id) = state.pending.keys().next().copied() {
                if &version_id <= txn_id {
                    state.pending.pop_first();
                } else {
                    break;
                }
            }

            while let Some(version_id) = state.commits.first().map(|id| **id) {
                if &version_id <= txn_id {
                    state.commits.pop_first();
                } else {
                    break;
                }
            }

            while let Some(version_id) = state.deltas.keys().next().copied() {
                if &version_id <= txn_id {
                    let version = state.deltas.pop_first().expect("version");
                    deltas.push(version);
                } else {
                    break;
                }
            }

            state.finalized = Some(*txn_id);

            deltas
        };

        for delta in deltas {
            let (inserted, deleted) = delta.read().await;
            canon.merge(inserted).await.expect("commit inserts");
            canon.delete_all(deleted).await.expect("commit deletes");
        }

        self.semaphore.finalize(txn_id, true);

        finalize_dir(&self.dir, txn_id).await;
    }
}

#[async_trait]
impl<Txn, FE> Persist<FE> for TableFile<Txn, FE>
where
    Txn: Transaction<FE>,
    FE: AsType<Node> + ThreadSafe + Clone,
{
    type Txn = Txn;
    type Schema = TableSchema;

    async fn create(_txn_id: TxnId, schema: TableSchema, store: Dir<FE>) -> TCResult<Self> {
        let dir = store.into_inner();
        let collator = ValueCollator::default();

        let (canon, committed) = {
            let mut dir = dir.write().await;
            let committed = dir.create_dir(COMMITTED.to_string())?;
            let canon = dir.create_dir(CANON.to_string())?;
            let canon = Version::create(schema, collator, canon)?;
            (canon, committed)
        };

        Self::new(dir, canon, committed)
    }

    async fn load(_txn_id: TxnId, schema: TableSchema, store: Dir<FE>) -> TCResult<Self> {
        let dir = store.into_inner();
        let collator = ValueCollator::default();

        let (canon, committed) = {
            let mut dir = dir.write().await;
            let committed = dir.get_or_create_dir(COMMITTED.to_string())?;
            let canon = dir.get_or_create_dir(CANON.to_string())?;
            let canon = Version::load(schema, collator, canon)?;
            (canon, committed)
        };

        Self::new(dir, canon, committed)
    }

    fn dir(&self) -> Inner<FE> {
        self.dir.clone()
    }
}

// TODO: can this be consolidated with impl CopyFrom for BTreeFile?
#[async_trait]
impl<Txn, FE, T> CopyFrom<FE, T> for TableFile<Txn, FE>
where
    Txn: Transaction<FE>,
    FE: AsType<Node> + ThreadSafe + Clone,
    T: TableStream + 'static,
{
    async fn copy_from(
        txn: &<Self as Persist<FE>>::Txn,
        store: Dir<FE>,
        instance: T,
    ) -> TCResult<Self> {
        let txn_id = *txn.id();
        let dir = store.into_inner();
        let schema = instance.schema().clone();
        let collator = ValueCollator::default();

        let mut rows = instance.rows(txn_id).await?;

        let (canon, versions) = {
            let mut dir = dir.write().await;

            let versions = dir
                .get_dir(&*VERSIONS)
                .cloned()
                .ok_or_else(|| internal!("missing versions dir"))?;

            let canon = dir.create_dir(CANON.to_string())?;

            (canon, versions)
        };

        let version = {
            let mut dir = versions.write().await;
            dir.create_dir(txn_id.to_string())?
        };

        let (deletes, inserts) = {
            let mut version = version.write().await;
            let deletes = version.create_dir(DELETES.to_string())?;
            let inserts = version.create_dir(INSERTS.to_string())?;
            (deletes, inserts)
        };

        let inserts = Version::create(schema.clone(), collator.clone(), inserts)?;

        {
            let key_len = b_table::Schema::key(&schema).len();
            let mut inserts = inserts.write().await;
            while let Some(mut key) = rows.try_next().await? {
                let values = key.drain(key_len..).collect();
                inserts.upsert(key, values).await?;
            }
        }

        let deletes = Version::create(schema.clone(), collator.clone(), deletes)?;

        let delta = Delta {
            dir: version,
            deletes,
            inserts,
        };

        let canon = Version::create(schema, collator.clone(), canon)?;

        let semaphore = Semaphore::with_reservation(txn_id, collator.into(), Range::default());

        Ok(Self {
            dir,
            canon,
            state: Arc::new(RwLock::new(State {
                deltas: OrdHashMap::new(),
                commits: OrdHashSet::new(),
                pending: std::iter::once((txn_id, delta)).collect(),
                finalized: None,
            })),
            semaphore,
            phantom: PhantomData,
        })
    }
}

// TODO: can this be consolidated with impl Restore for BTreeFile?
#[async_trait]
impl<Txn, FE> Restore<FE> for TableFile<Txn, FE>
where
    Txn: Transaction<FE>,
    FE: AsType<Node> + ThreadSafe + Clone,
{
    async fn restore(&self, txn_id: TxnId, backup: &Self) -> TCResult<()> {
        debug!("Table::restore");

        let _permit = self.semaphore.write(txn_id, Range::default()).await?;

        let collator = self.canon.collator().inner();

        let schema = if self.schema() == backup.schema() {
            self.schema()
        } else {
            return Err(bad_request!(
                "cannot restore a Table with schema {:?} from one with schema {:?}",
                self.schema(),
                backup.schema()
            ));
        };

        let canon = self.canon.read().await;

        let (deltas, pending) = {
            let dir = self.dir.read().await;
            let mut state = self.state.write().expect("state");

            let deltas = state
                .deltas
                .iter()
                .take_while(|(id, _)| *id < &txn_id)
                .map(|(_, delta)| delta)
                .cloned()
                .collect::<Vec<_>>();

            let pending = state.pending_version(txn_id, &*dir, schema, collator)?;

            (deltas, pending)
        };

        let (mut inserts, mut deletes) = pending.write().await;

        try_join!(inserts.truncate(), deletes.truncate())?;

        deletes.merge(canon).await?;

        for delta in deltas {
            let (inserted, deleted) = delta.read().await;
            deletes.merge(inserted).await?;
            deletes.delete_all(deleted).await?;
        }

        let key_len = b_table::Schema::key(self.schema()).len();
        let mut to_insert = backup.clone().rows(txn_id).await?;
        while let Some(mut row) = to_insert.try_next().await? {
            let values = row.drain(key_len..).collect();
            let key = row;

            deletes.delete_row(key.to_vec()).await?;
            inserts.upsert(key, values).await?;
        }

        Ok(())
    }
}

struct TableVisitor<Txn, FE> {
    txn: Txn,
    phantom: PhantomData<FE>,
}

impl<Txn, FE> TableVisitor<Txn, FE> {
    fn new(txn: Txn) -> Self {
        Self {
            txn,
            phantom: PhantomData,
        }
    }
}

#[async_trait]
impl<Txn, FE> de::Visitor for TableVisitor<Txn, FE>
where
    Txn: Transaction<FE>,
    FE: AsType<Node> + ThreadSafe,
{
    type Value = TableFile<Txn, FE>;

    fn expecting() -> &'static str {
        "a Table"
    }

    async fn visit_seq<A: de::SeqAccess>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        trace!("TableVisitor::visit_seq");

        let txn_id = *self.txn.id();
        let collator = ValueCollator::default();

        let schema = seq.expect_next::<TableSchema>(()).await?;
        trace!("decoded table schema: {:?}", schema);

        let (canon, versions) = {
            let mut dir = self.txn.context().write().await;

            let canon = dir
                .create_dir(CANON.to_string())
                .map_err(de::Error::custom)?;

            let versions = dir
                .create_dir(VERSIONS.to_string())
                .map_err(de::Error::custom)?;

            (canon, versions)
        };

        trace!("created canon and versions dirs");

        let version = {
            let mut dir = versions.write().await;
            dir.create_dir(txn_id.to_string())
                .map_err(de::Error::custom)?
        };

        trace!("created version dir");

        let (deletes, inserts) = {
            let mut dir = version.write().await;

            let deletes = dir
                .create_dir(DELETES.to_string())
                .map_err(de::Error::custom)?;

            let inserts = dir
                .create_dir(INSERTS.to_string())
                .map_err(de::Error::custom)?;

            (deletes, inserts)
        };

        let cxt = (schema.clone(), collator.clone(), inserts.clone());
        let inserts = if let Some(inserts) = seq.next_element(cxt).await? {
            inserts
        } else {
            Version::create(schema.clone(), collator.clone(), inserts).map_err(de::Error::custom)?
        };

        trace!("decoded version inserts");

        let deletes = Version::create(schema.clone(), collator.clone(), deletes)
            .map_err(de::Error::custom)?;

        let version = Delta {
            dir: version,
            inserts,
            deletes,
        };

        trace!("created version");

        let canon = Version::create(schema, collator, canon).map_err(de::Error::custom)?;

        trace!("created canonical version");

        let collator = Arc::new(canon.collator().inner().clone());
        let semaphore = Semaphore::with_reservation(txn_id, collator, Range::default());

        Ok(TableFile {
            dir: self.txn.context().clone(),
            state: Arc::new(RwLock::new(State {
                commits: OrdHashSet::with_capacity(0),
                deltas: OrdHashMap::with_capacity(0),
                pending: std::iter::once((txn_id, version)).collect(),
                finalized: None,
            })),
            canon,
            semaphore,
            phantom: PhantomData,
        })
    }
}

#[async_trait]
impl<Txn, FE> de::FromStream for TableFile<Txn, FE>
where
    Txn: Transaction<FE>,
    FE: AsType<Node> + ThreadSafe,
{
    type Context = Txn;

    async fn from_stream<D: de::Decoder>(txn: Txn, decoder: &mut D) -> Result<Self, D::Error> {
        debug!("TableFile::from_stream");

        decoder.decode_seq(TableVisitor::new(txn)).await
    }
}

impl<Txn, FE> fmt::Debug for TableFile<Txn, FE>
where
    Self: TableInstance,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "a relational database table with schema {:?}",
            self.schema()
        )
    }
}
