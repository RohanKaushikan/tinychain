use std::convert::TryFrom;

use afarray::{Array, ArrayExt, CoordUnique, Coords, Offsets};
use futures::stream::{self, Stream, StreamExt, TryStreamExt};

use tc_btree::Node;
use tc_error::*;
use tc_transact::fs::{Dir, File};
use tc_transact::{Transaction, TxnId};
use tc_value::{Number, UIntType};

use crate::dense::{BlockListFile, PER_BLOCK};
use crate::{Coord, Shape, TensorAccess, TensorType};

use super::ReadValueAt;

pub async fn sorted_coords<FD, FS, D, T, C>(
    txn: &T,
    shape: Shape,
    coords: C,
) -> TCResult<impl Stream<Item = TCResult<Coords>> + Unpin>
where
    FD: File<Array> + TryFrom<D::File, Error = TCError>,
    FS: File<Node>,
    D: Dir,
    T: Transaction<D>,
    C: Stream<Item = TCResult<Coords>> + Unpin + Send,
    D::FileClass: From<TensorType>,
{
    let txn_id = *txn.id();
    let file: FD = txn
        .context()
        .create_file_tmp(txn_id, TensorType::Dense)
        .await?;

    let offsets = sort_coords::<FD, FS, D, T, _>(file, txn_id, coords, shape.clone()).await?;
    let offsets = offsets
        .into_stream(txn_id)
        .map_ok(|array| array.type_cast());

    let coords = offsets_to_coords(shape.clone(), offsets);
    let coords = CoordUnique::new(coords, shape.to_vec(), PER_BLOCK);
    Ok(coords)
}

pub async fn sorted_values<'a, FD, FS, T, D, A, C>(
    txn: T,
    source: A,
    coords: C,
) -> TCResult<impl Stream<Item = TCResult<(Coord, Number)>>>
where
    FD: File<Array> + TryFrom<D::File, Error = TCError>,
    FS: File<Node>,
    D: Dir,
    T: Transaction<D>,
    C: Stream<Item = TCResult<Coords>> + Send + Unpin + 'a,
    A: TensorAccess + ReadValueAt<D, Txn = T> + 'a + Clone,
    D::FileClass: From<TensorType>,
{
    let coords = sorted_coords::<FD, FS, D, T, C>(&txn, source.shape().clone(), coords).await?;

    let buffered = coords
        .map_ok(|coords| stream::iter(coords.to_vec()).map(TCResult::Ok))
        .try_flatten()
        .map_ok(move |coord| source.clone().read_value_at(txn.clone(), coord))
        .try_buffered(num_cpus::get());

    Ok(buffered)
}

async fn sort_coords<FD, FS, D, T, S>(
    file: FD,
    txn_id: TxnId,
    coords: S,
    shape: Shape,
) -> TCResult<BlockListFile<FD, FS, D, T>>
where
    FD: File<Array>,
    FS: File<Node>,
    D: Dir,
    T: Transaction<D>,
    S: Stream<Item = TCResult<Coords>> + Send + Unpin,
{
    let blocks = coords_to_offsets(shape, coords).map_ok(|block| ArrayExt::from(block).into());

    let block_list =
        BlockListFile::from_blocks(file, txn_id, None, UIntType::U64.into(), Box::pin(blocks))
            .await?;

    block_list.merge_sort(txn_id).await?;
    Ok(block_list)
}

fn coords_to_offsets<S: Stream<Item = TCResult<Coords>> + Unpin>(
    shape: Shape,
    coords: S,
) -> impl Stream<Item = TCResult<Offsets>> {
    coords.map_ok(move |coords| coords.to_offsets(&shape))
}

fn offsets_to_coords<'a, S: Stream<Item = TCResult<Offsets>> + Unpin + 'a>(
    shape: Shape,
    offsets: S,
) -> impl Stream<Item = TCResult<Coords>> + Unpin + 'a {
    offsets.map_ok(move |block| Coords::from_offsets(block, &shape))
}
