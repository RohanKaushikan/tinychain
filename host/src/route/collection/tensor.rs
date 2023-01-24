use std::convert::TryInto;

use destream::de::Error;
use futures::future::{self, Future, TryFutureExt};
use futures::stream::{self, FuturesUnordered, StreamExt, TryStreamExt};
use log::debug;
use safecast::*;

use tc_btree::{Node, NodeId};
use tc_error::*;
use tc_tensor::*;
use tc_transact::fs::{CopyFrom, Dir, Persist};
use tc_transact::Transaction;
use tc_value::{
    Bound, FloatType, Number, NumberClass, NumberInstance, NumberType, Range, Value, ValueType,
};
use tcgeneric::{label, Id, Label, PathSegment, TCBoxTryFuture, Tuple};

use crate::collection::{
    Collection, DenseTensor, DenseTensorFile, SparseTable, SparseTensor, Tensor,
};
use crate::fs;
use crate::fs::File;
use crate::object::Object;
use crate::route::{AttributeHandler, GetHandler, PostHandler, PutHandler, SelfHandlerOwned};
use crate::scalar::Scalar;
use crate::state::{State, StateType};
use crate::stream::{Source, TCStream};
use crate::txn::Txn;

use super::{Handler, Route};

const AXIS: Label = label("axis");
const KEEPDIMS: Label = label("keepdims");
const RIGHT: Label = label("r");
const TENSOR: Label = label("tensor");
const TENSORS: Label = label("tensors");

const MEAN: f64 = 0.0;
const STD: f64 = 0.0;

struct ArgmaxHandler<T> {
    tensor: T,
}

impl<'a, T> Handler<'a> for ArgmaxHandler<T>
where
    T: TensorAccess + TensorIndex<fs::Dir, Txn = Txn> + Send + Sync + 'a,
    Tensor: From<T::Index>,
{
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, key| {
            Box::pin(async move {
                let txn = txn.clone();

                let axis = if key.is_none() {
                    None
                } else {
                    let ndim = self.tensor.ndim();
                    match cast_axis(key, ndim)? {
                        axis if ndim == 1 && axis == 0 => None,
                        axis => Some(axis),
                    }
                };

                if let Some(axis) = axis {
                    self.tensor
                        .argmax(txn, axis)
                        .map_ok(Tensor::from)
                        .map_ok(Collection::Tensor)
                        .map_ok(State::Collection)
                        .await
                } else {
                    self.tensor
                        .argmax_all(txn)
                        .map_ok(Value::from)
                        .map_ok(State::from)
                        .await
                }
            })
        }))
    }
}

impl<T> From<T> for ArgmaxHandler<T> {
    fn from(tensor: T) -> Self {
        Self { tensor }
    }
}

struct BroadcastHandler<T> {
    tensor: T,
}

impl<'a, T> Handler<'a> for BroadcastHandler<T>
where
    T: TensorAccess + TensorTransform + Send + Sync + 'a,
    Tensor: From<T::Broadcast>,
{
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|_txn, key| {
            Box::pin(async move {
                let shape = Shape::try_cast_from(key, |v| {
                    TCError::invalid_value(v, "a Tensor shape for broadcasting")
                })?;

                let shape = broadcast_shape(self.tensor.shape(), &shape)?;
                self.tensor
                    .broadcast(shape)
                    .map(Tensor::from)
                    .map(State::from)
            })
        }))
    }
}

impl<T> From<T> for BroadcastHandler<T> {
    fn from(tensor: T) -> Self {
        Self { tensor }
    }
}

struct CastHandler<T> {
    tensor: T,
}

impl<'a, T> Handler<'a> for CastHandler<T>
where
    T: TensorTransform + Send + Sync + 'a,
    Tensor: From<T::Cast>,
{
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|_txn, key| {
            Box::pin(async move {
                let dtype =
                    ValueType::try_cast_from(key, |v| TCError::invalid_type(v, "a Number class"))?;

                let dtype = dtype.try_into()?;
                self.tensor
                    .cast_into(dtype)
                    .map(Tensor::from)
                    .map(State::from)
            })
        }))
    }
}

impl<T> From<T> for CastHandler<T> {
    fn from(tensor: T) -> Self {
        Self { tensor }
    }
}

struct ConcatenateHandler;

impl ConcatenateHandler {
    async fn blank(
        txn: &Txn,
        shape: Vec<u64>,
        dtype: NumberType,
    ) -> TCResult<DenseTensor<DenseTensorFile>> {
        let txn_id = *txn.id();
        let file = txn.context().create_file_unique(txn_id).await?;
        DenseTensor::constant(file, txn_id, shape, dtype.zero())
    }

    async fn concatenate_axis(
        txn: &Txn,
        axis: usize,
        dtype: NumberType,
        tensors: Vec<Tensor>,
    ) -> TCResult<Tensor> {
        const ERR_OFF_AXIS: &str = "Tensors to concatenate must have the same off-axis dimensions";

        let ndim = tensors[0].ndim();
        let mut offsets = Vec::with_capacity(tensors.len());
        let mut shape_out = tensors[0].shape().to_vec();
        shape_out[axis] = 0;

        for tensor in &tensors {
            tensor.shape().validate("concatenate")?;

            if tensor.ndim() != ndim {
                return Err(bad_request!(
                    "Tensors to concatenate must have the same dimensions"
                ));
            }

            let shape = tensor.shape();

            for x in 0..axis {
                let dim = shape[x];
                if dim != shape_out[x] {
                    return Err(bad_request!("{}", ERR_OFF_AXIS));
                }
            }

            if axis < tensor.ndim() {
                for x in (axis + 1)..ndim {
                    if shape[x] != shape_out[x] {
                        return Err(bad_request!("{}", ERR_OFF_AXIS));
                    }
                }
            }

            shape_out[axis] += shape[axis];
            if let Some(offset) = offsets.last().cloned() {
                offsets.push(offset + shape[axis]);
            } else {
                offsets.push(shape[axis]);
            }
        }

        let bounds: Bounds = shape_out.iter().map(|dim| AxisBounds::all(*dim)).collect();
        let concatenated = Self::blank(txn, shape_out.clone().into(), dtype).await?;
        debug!("concantenation shape is {}", concatenated.shape());

        let mut writes: FuturesUnordered<_> = tensors
            .into_iter()
            .zip(offsets)
            .map(|(tensor, offset)| {
                debug!("concatenate {} along axis {}", tensor, axis);
                let mut bounds = bounds.clone();
                bounds[axis] = AxisBounds::In((offset - tensor.shape()[axis])..offset);
                concatenated.clone().write(txn.clone(), bounds, tensor)
            })
            .collect();

        while let Some(()) = writes.try_next().await? {
            // no-op
        }

        Ok(concatenated.into())
    }
}

impl<'a> Handler<'a> for ConcatenateHandler {
    fn post<'b>(self: Box<Self>) -> Option<PostHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, mut params| {
            Box::pin(async move {
                let tensors: Vec<Tensor> = params.require(&TENSORS.into())?;
                let axis: Value = params.or_default(&AXIS.into())?;
                params.expect_empty()?;

                if tensors.is_empty() {
                    return Err(bad_request!("no Tensors to concatenate"));
                }

                let dtype = tensors
                    .iter()
                    .map(TensorAccess::dtype)
                    .fold(tensors[0].dtype(), Ord::max);

                let axis = if axis.is_some() {
                    cast_axis(axis, tensors[0].ndim())?
                } else {
                    0
                };

                if axis > tensors[0].ndim() {
                    return Err(bad_request!(
                        "axis {} is out of bounds for {}",
                        axis,
                        tensors[0]
                    ));
                }

                let tensor = Self::concatenate_axis(txn, axis, dtype, tensors).await?;
                Ok(State::Collection(tensor.into()))
            })
        }))
    }
}

struct ConstantHandler;

impl<'a> Handler<'a> for ConstantHandler {
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, key| {
            Box::pin(async move {
                let (shape, value): (Vec<u64>, Number) =
                    key.try_cast_into(|v| TCError::invalid_value(v, "a Tensor schema"))?;

                let shape = Shape::from(shape);
                constant(&txn, shape, value)
                    .map_ok(Tensor::from)
                    .map_ok(Collection::from)
                    .map_ok(State::from)
                    .await
            })
        }))
    }
}

struct CopyFromHandler;

impl<'a> Handler<'a> for CopyFromHandler {
    fn post<'b>(self: Box<Self>) -> Option<PostHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, mut params| {
            Box::pin(async move {
                let source: Tensor = params.require(&TENSOR.into())?;
                params.expect_empty()?;

                source.shape().validate("copy Tensor")?;

                let copy = match source {
                    Tensor::Dense(source) => {
                        let store = txn.context().create_store_unique(*txn.id()).await?;
                        let blocks =
                            BlockListFile::copy_from(txn, store, source.into_inner()).await?;

                        DenseTensor::from(blocks.accessor()).into()
                    }
                    Tensor::Sparse(source) => {
                        let store = txn.context().create_store_unique(*txn.id()).await?;
                        let table = SparseTable::copy_from(txn, store, source).await?;
                        SparseTensor::from(table.accessor()).into()
                    }
                };

                Ok(State::Collection(Collection::Tensor(copy)))
            })
        }))
    }
}

struct CopyDenseHandler;

impl<'a> Handler<'a> for CopyDenseHandler {
    fn post<'b>(self: Box<Self>) -> Option<PostHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, mut params| {
            Box::pin(async move {
                let schema: Value = params.require(&label("schema").into())?;
                let Schema { dtype, shape } =
                    schema.try_cast_into(|v| TCError::invalid_value(v, "a Tensor schema"))?;

                shape.validate("copy Dense")?;

                let source: TCStream = params.require(&label("source").into())?;
                params.expect_empty()?;

                let elements = source.into_stream(txn.clone()).await?;
                let elements = elements.map(|r| {
                    r.and_then(|n| {
                        Number::try_cast_from(n, |n| TCError::invalid_type(n, "a Tensor element"))
                    })
                });

                let txn_id = *txn.id();
                let file = create_file(txn).await?;
                DenseTensorFile::from_values(file, txn_id, shape, dtype, elements)
                    .map_ok(DenseTensor::from)
                    .map_ok(Tensor::from)
                    .map_ok(Collection::Tensor)
                    .map_ok(State::Collection)
                    .await
            })
        }))
    }
}

struct CopySparseHandler;

impl<'a> Handler<'a> for CopySparseHandler {
    fn post<'b>(self: Box<Self>) -> Option<PostHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, mut params| {
            Box::pin(async move {
                let schema: Value = params.require(&label("schema").into())?;
                let schema: Schema =
                    schema.try_cast_into(|v| TCError::invalid_value(v, "invalid Tensor schema"))?;

                schema.validate("copy Sparse")?;

                let source: TCStream = params.require(&label("source").into())?;
                params.expect_empty()?;

                let elements = source.into_stream(txn.clone()).await?;

                let tensor = create_sparse(txn, schema).await?;

                let elements = elements
                    .map(|r| {
                        r.and_then(|state| {
                            Value::try_cast_from(state, |s| {
                                TCError::invalid_type(s, "a sparse Tensor element")
                            })
                        })
                    })
                    .map(|r| {
                        r.and_then(|row| {
                            row.try_cast_into(|v| {
                                bad_request!(
                                    "sparse Tensor expected a (Coord, Number) tuple, found {}",
                                    v,
                                )
                            })
                        })
                    });

                elements
                    .map_ok(|(coord, value)| tensor.write_value_at(*txn.id(), coord, value))
                    .try_buffer_unordered(num_cpus::get())
                    .try_fold((), |(), ()| future::ready(Ok(())))
                    .await?;

                Ok(Collection::Tensor(tensor.into()).into())
            })
        }))
    }
}

struct CreateHandler {
    class: TensorType,
}

impl<'a> Handler<'a> for CreateHandler {
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, key| {
            Box::pin(async move {
                let schema: Schema =
                    key.try_cast_into(|v| TCError::invalid_value(v, "a Tensor schema"))?;

                create_tensor(self.class, schema, txn)
                    .map_ok(Collection::Tensor)
                    .map_ok(State::Collection)
                    .await
            })
        }))
    }
}

struct LoadHandler {
    class: Option<TensorType>,
}

impl<'a> Handler<'a> for LoadHandler {
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, key| {
            Box::pin(async move {
                let (schema, elements): (Value, Value) =
                    key.try_cast_into(|v| TCError::invalid_value(v, "a Tensor schema"))?;

                let txn_id = *txn.id();

                if elements.matches::<Vec<(Vec<u64>, Number)>>() {
                    let elements: Vec<(Vec<u64>, Number)> = elements
                        .opt_cast_into()
                        .expect("tensor coordinate elements");

                    let schema = if schema.matches::<Scalar>() {
                        schema.opt_cast_into().expect("load tensor schema")
                    } else if schema.matches::<Shape>() {
                        let shape = schema.opt_cast_into().expect("load tensor shape");
                        let dtype = if elements.is_empty() {
                            NumberType::Float(FloatType::F32)
                        } else {
                            elements[0].1.class()
                        };

                        Schema { shape, dtype }
                    } else {
                        return Err(TCError::invalid_value(schema, "a Tensor schema"));
                    };

                    let class = self.class.unwrap_or(TensorType::Sparse);
                    let tensor = create_tensor(class, schema, txn).await?;

                    stream::iter(elements)
                        .map(|(coord, value)| tensor.write_value_at(txn_id, coord, value))
                        .buffer_unordered(num_cpus::get())
                        .try_fold((), |(), ()| future::ready(Ok(())))
                        .await?;

                    Ok(State::Collection(Collection::Tensor(tensor)))
                } else if elements.matches::<Vec<Number>>() {
                    let elements: Vec<Number> = elements.opt_cast_into().expect("tensor elements");
                    if elements.is_empty() {
                        return Err(bad_request!(
                            "a dense Tensor cannot be loaded from a zero-element Tuple",
                        ));
                    }

                    let schema = if schema.matches::<Schema>() {
                        schema.opt_cast_into().expect("load tensor schema")
                    } else if schema.matches::<Shape>() {
                        let shape = schema.opt_cast_into().expect("load tensor shape");
                        Schema {
                            shape,
                            dtype: elements[0].class(),
                        }
                    } else {
                        return Err(TCError::invalid_value(schema, "a Tensor schema"));
                    };

                    if elements.len() as u64 != schema.shape.size() {
                        return Err(bad_request!(
                            "wrong number of elements for Tensor with shape {}: {}",
                            schema.shape,
                            Tuple::from(elements)
                        ));
                    }

                    if let Some(class) = self.class {
                        if class != TensorType::Dense {
                            return Err(bad_request!(
                                "loading all elements {} of a Sparse tensor does not make sense",
                                Tuple::from(elements),
                            ))?;
                        }
                    }

                    let txn_id = *txn.id();
                    let file = create_file(txn).await?;
                    let elements = stream::iter(elements).map(Ok);
                    DenseTensorFile::from_values(file, txn_id, schema.shape, schema.dtype, elements)
                        .map_ok(DenseTensor::from)
                        .map_ok(Tensor::from)
                        .map_ok(Collection::Tensor)
                        .map_ok(State::Collection)
                        .await
                } else {
                    Err(bad_request!("tensor elements must be a Tuple of Numbers or a Tuple of (Coord, Number) pairs, not {}", elements))
                }
            })
        }))
    }
}

struct ElementsHandler<T> {
    tensor: T,
}

impl<T> ElementsHandler<T> {
    fn new(tensor: T) -> Self {
        Self { tensor }
    }
}

impl<'a, T> Handler<'a> for ElementsHandler<T>
where
    T: TensorAccess + TensorTransform + Send + Sync + 'a,
    Tensor: From<T> + From<T::Slice>,
{
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|_txn, key| {
            Box::pin(async move {
                let tensor = if key.is_none() {
                    Tensor::from(self.tensor)
                } else {
                    let bounds = cast_bounds(self.tensor.shape(), key)?;
                    let slice = self.tensor.slice(bounds)?;
                    Tensor::from(slice)
                };

                Ok(TCStream::from(Collection::Tensor(tensor)).into())
            })
        }))
    }
}

struct EyeHandler;

impl<'a> Handler<'a> for EyeHandler {
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, key| {
            Box::pin(async move {
                let size = u64::try_cast_from(key, |v| {
                    TCError::invalid_value(v, "the size of an identity tensor")
                })?;

                let schema = Schema::from((vec![size, size].into(), NumberType::Bool));
                let tensor = create_sparse(txn, schema).await?;

                stream::iter(0..size)
                    .map(|i| (vec![i, i], true.into()))
                    .map(|(coord, value)| tensor.write_value_at(*txn.id(), coord, value))
                    .buffer_unordered(num_cpus::get())
                    .try_fold((), |(), ()| future::ready(Ok(())))
                    .await?;

                Ok(State::Collection(tensor.into()))
            })
        }))
    }
}

struct DiagonalHandler<T> {
    tensor: T,
}

impl<'a, T> Handler<'a> for DiagonalHandler<T>
where
    T: TensorAccess + TensorDiagonal<fs::Dir, Txn = Txn> + Send + 'a,
    Tensor: From<T::Diagonal>,
{
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, key| {
            Box::pin(async move {
                key.expect_none()?;

                self.tensor
                    .diagonal(txn.clone())
                    .map_ok(Tensor::from)
                    .map_ok(Collection::from)
                    .map_ok(State::Collection)
                    .await
            })
        }))
    }
}

impl<T> From<T> for DiagonalHandler<T> {
    fn from(tensor: T) -> Self {
        Self { tensor }
    }
}

struct ExpandHandler<T> {
    tensor: T,
}

impl<'a, T> Handler<'a> for ExpandHandler<T>
where
    T: TensorAccess + TensorTransform + Send + 'a,
    Tensor: From<T::Expand>,
{
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|_txn, key| {
            Box::pin(async move {
                self.tensor.shape().validate("expand")?;

                let axis = if key.is_none() {
                    self.tensor.ndim()
                } else {
                    cast_axis(key, self.tensor.ndim())?
                };

                self.tensor
                    .expand_dims(axis)
                    .map(Tensor::from)
                    .map(Collection::from)
                    .map(State::from)
            })
        }))
    }
}

impl<T> From<T> for ExpandHandler<T> {
    fn from(tensor: T) -> Self {
        Self { tensor }
    }
}

struct FlipHandler<T> {
    tensor: T,
}

impl<'a, T> Handler<'a> for FlipHandler<T>
where
    T: TensorAccess + TensorTransform + Send + 'a,
    Tensor: From<T::Flip>,
{
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|_txn, key| {
            Box::pin(async move {
                self.tensor.shape().validate("flip")?;

                let axis = cast_axis(key, self.tensor.ndim())?;
                self.tensor
                    .flip(axis)
                    .map(Tensor::from)
                    .map(Collection::from)
                    .map(State::from)
            })
        }))
    }
}

impl<T> From<T> for FlipHandler<T> {
    fn from(tensor: T) -> Self {
        Self { tensor }
    }
}

struct RandomNormalHandler;

impl<'a> Handler<'a> for RandomNormalHandler {
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, key| {
            Box::pin(async move {
                let shape = key.try_cast_into(|v| TCError::invalid_value(v, "a Tensor shape"))?;

                let file = create_file(&txn).await?;

                let tensor = BlockListFile::random_normal(
                    file,
                    *txn.id(),
                    shape,
                    FloatType::F64,
                    MEAN.into(),
                    STD.into(),
                )
                .map_ok(DenseTensor::from)
                .map_ok(Tensor::from)
                .await?;

                Ok(State::Collection(tensor.into()))
            })
        }))
    }

    fn post<'b>(self: Box<Self>) -> Option<PostHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, mut params| {
            Box::pin(async move {
                let shape: Value = params.require(&label("shape").into())?;
                let shape: Shape =
                    shape.try_cast_into(|v| TCError::invalid_value(v, "a Tensor shape"))?;

                let mean = params.option(&label("mean").into(), || MEAN.into())?;
                let std = params.option(&label("std").into(), || STD.into())?;
                params.expect_empty()?;

                let file = create_file(&txn).await?;

                let tensor = BlockListFile::random_normal(
                    file,
                    *txn.id(),
                    shape.into(),
                    FloatType::F64,
                    mean,
                    std,
                )
                .map_ok(DenseTensor::from)
                .map_ok(Tensor::from)
                .await?;

                Ok(State::Collection(tensor.into()))
            })
        }))
    }
}

struct RandomUniformHandler;

impl<'a> Handler<'a> for RandomUniformHandler {
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, key| {
            Box::pin(async move {
                let shape = key.try_cast_into(|v| TCError::invalid_value(v, "a Tensor shape"))?;

                let file = create_file(&txn).await?;

                let tensor = BlockListFile::random_uniform(file, *txn.id(), shape, FloatType::F64)
                    .map(DenseTensor::from)
                    .map(Tensor::from)?;

                Ok(State::Collection(tensor.into()))
            })
        }))
    }
}

struct RangeHandler;

impl<'a> Handler<'a> for RangeHandler {
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, key| {
            Box::pin(async move {
                if key.matches::<(Vec<u64>, Number, Number)>() {
                    let (shape, start, stop): (Vec<u64>, Number, Number) =
                        key.opt_cast_into().unwrap();

                    let file = create_file(&txn).await?;

                    DenseTensor::range(file, *txn.id(), shape, start, stop)
                        .map_ok(Tensor::from)
                        .map_ok(Collection::from)
                        .map_ok(State::from)
                        .await
                } else {
                    Err(TCError::invalid_value(key, "a Tensor schema"))
                }
            })
        }))
    }
}

struct ReshapeHandler<T> {
    tensor: T,
}

impl<'a, T> Handler<'a> for ReshapeHandler<T>
where
    T: TensorAccess + TensorTransform + Send + 'a,
    Tensor: From<T::Reshape>,
{
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|_txn, key| {
            Box::pin(async move {
                let shape = key.try_into()?;
                let shape = cast_shape(self.tensor.shape(), shape)?;
                self.tensor
                    .reshape(shape.into())
                    .map(Tensor::from)
                    .map(Collection::from)
                    .map(State::from)
            })
        }))
    }
}

impl<T> From<T> for ReshapeHandler<T> {
    fn from(tensor: T) -> Self {
        Self { tensor }
    }
}

struct TileHandler;

impl<'a> Handler<'a> for TileHandler {
    fn post<'b>(self: Box<Self>) -> Option<PostHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, mut params| {
            Box::pin(async move {
                let tensor: Tensor = params.require(&TENSOR.into())?;
                let multiples: Value = params.require(&label("multiples").into())?;
                params.expect_empty()?;

                let multiples: Vec<u64> = match multiples {
                    Value::Number(n) if n >= Number::from(1) => {
                        assert!(tensor.ndim() > 0);
                        let mut multiples = vec![1; tensor.ndim() - 1];
                        multiples.push(n.cast_into());
                        Ok(multiples)
                    }
                    Value::Number(n) => Err(bad_request!("cannot tile a Tensor {} times", n))?,
                    Value::Tuple(multiples) if multiples.len() == tensor.ndim() => multiples
                        .try_cast_into(|v| {
                            TCError::invalid_value(v, "a list of multiples for tiling")
                        }),
                    other => Err(TCError::invalid_value(
                        other,
                        "a list of multiples for tiling",
                    )),
                }?;

                match tensor {
                    Tensor::Dense(dense) => {
                        DenseTensor::tile(txn.clone(), dense, multiples)
                            .map_ok(Tensor::from)
                            .map_ok(State::from)
                            .await
                    }
                    Tensor::Sparse(sparse) => {
                        SparseTensor::tile(txn.clone(), sparse, multiples)
                            .map_ok(Tensor::from)
                            .map_ok(State::from)
                            .await
                    }
                }
            })
        }))
    }
}

struct TransposeHandler<T> {
    tensor: T,
}

impl<'a, T> Handler<'a> for TransposeHandler<T>
where
    T: TensorTransform + Send + 'a,
    Tensor: From<T::Transpose>,
{
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|_txn, key| {
            Box::pin(async move {
                let transpose = if key.is_none() {
                    self.tensor.transpose(None)
                } else {
                    let permutation =
                        key.try_cast_into(|v| TCError::invalid_value(v, "a Tensor permutation"))?;

                    self.tensor.transpose(Some(permutation))
                };

                transpose
                    .map(Tensor::from)
                    .map(Collection::from)
                    .map(State::from)
            })
        }))
    }
}

impl<T> From<T> for TransposeHandler<T> {
    fn from(tensor: T) -> Self {
        Self { tensor }
    }
}

impl Route for TensorType {
    fn route<'a>(&'a self, path: &'a [PathSegment]) -> Option<Box<dyn Handler<'a> + 'a>> {
        if path.is_empty() {
            return Some(Box::new(CreateHandler { class: *self }));
        }

        if path.len() == 1 {
            match self {
                Self::Dense => match path[0].as_str() {
                    "copy_from" => Some(Box::new(CopyDenseHandler)),
                    "concatenate" => Some(Box::new(ConcatenateHandler)),
                    "constant" => Some(Box::new(ConstantHandler)),
                    "load" => Some(Box::new(LoadHandler { class: Some(*self) })),
                    "range" => Some(Box::new(RangeHandler)),
                    "random" if path.len() == 1 => Some(Box::new(RandomUniformHandler)),
                    _ => None,
                },
                Self::Sparse => match path[0].as_str() {
                    "copy_from" => Some(Box::new(CopySparseHandler)),
                    "eye" => Some(Box::new(EyeHandler)),
                    "load" => Some(Box::new(LoadHandler { class: Some(*self) })),
                    _ => None,
                },
            }
        } else if path.len() == 2 {
            match self {
                Self::Dense => match path[0].as_str() {
                    "random" => match path[1].as_str() {
                        "normal" => Some(Box::new(RandomNormalHandler)),
                        "uniform" => Some(Box::new(RandomUniformHandler)),
                        _ => None,
                    },
                    _ => None,
                },
                Self::Sparse => None,
            }
        } else {
            None
        }
    }
}

struct DualHandler {
    tensor: Tensor,
    op: fn(Tensor, Tensor) -> TCResult<Tensor>,
    op_const: fn(Tensor, Number) -> TCResult<Tensor>,
    op_name: &'static str,
}

impl DualHandler {
    fn new<T>(
        tensor: T,
        op: fn(Tensor, Tensor) -> TCResult<Tensor>,
        op_const: fn(Tensor, Number) -> TCResult<Tensor>,
        op_name: &'static str,
    ) -> Self
    where
        Tensor: From<T>,
    {
        Self {
            tensor: tensor.into(),
            op,
            op_const,
            op_name,
        }
    }
}

impl<'a> Handler<'a> for DualHandler {
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|_txn, r| {
            Box::pin(async move {
                let r = Number::try_cast_from(r, |r| TCError::invalid_type(r, "a Number"))?;

                self.tensor.shape().validate(self.op_name)?;

                (self.op_const)(self.tensor, r)
                    .map(Collection::from)
                    .map(State::from)
            })
        }))
    }

    fn post<'b>(self: Box<Self>) -> Option<PostHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|_txn, mut params| {
            Box::pin(async move {
                let l = self.tensor;
                let r = params.remove::<Id>(&RIGHT.into()).ok_or_else(|| {
                    TCError::invalid_value(&params, "missing right-hand-side parameter r")
                })?;

                params.expect_empty()?;

                l.shape().validate(self.op_name)?;

                match r {
                    State::Collection(Collection::Tensor(r)) => {
                        r.shape().validate(self.op_name)?;

                        if l.shape() == r.shape() {
                            (self.op)(l, r).map(Collection::from).map(State::from)
                        } else {
                            let (l, r) = broadcast(l, r)?;
                            (self.op)(l, r).map(Collection::from).map(State::from)
                        }
                    }
                    State::Scalar(Scalar::Value(r)) if r.matches::<Number>() => {
                        let r = r.opt_cast_into().expect("numeric constant");
                        (self.op_const)(l, r).map(Collection::from).map(State::from)
                    }
                    other => Err(TCError::invalid_value(other, "a Tensor or Number")),
                }
            })
        }))
    }
}

// TODO: should this be more general, like `DualHandlerWithDefaultArgument`?
struct LogHandler {
    tensor: Tensor,
}

impl LogHandler {
    fn new<T>(tensor: T) -> Self
    where
        Tensor: From<T>,
    {
        Self {
            tensor: tensor.into(),
        }
    }
}

impl<'a> Handler<'a> for LogHandler {
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, r| {
            Box::pin(async move {
                self.tensor.shape().validate("Tensor log")?;

                // TODO: perform this check while computing the logarithm itself
                if !self.tensor.clone().all(txn.clone()).await? {
                    return Err(bad_request!("the logarithm of zero is undefined"));
                }

                let log = if r.is_none() {
                    self.tensor.ln()?
                } else {
                    let base =
                        Number::try_cast_from(r, |r| bad_request!("invalid base {} for log", r))?;

                    self.tensor.log_const(base)?
                };

                Ok(State::Collection(Collection::Tensor(log)))
            })
        }))
    }

    fn post<'b>(self: Box<Self>) -> Option<PostHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|_txn, mut params| {
            Box::pin(async move {
                let r = params.or_default(&RIGHT.into())?;
                params.expect_empty()?;

                let l = self.tensor;
                l.shape().validate("Tensor log")?;

                let log = match r {
                    State::Collection(Collection::Tensor(base)) => {
                        base.shape().validate("Tensor log")?;

                        if l.shape() == base.shape() {
                            l.log(base)
                        } else {
                            let (l, base) = broadcast(l, base)?;
                            l.log(base)
                        }
                    }
                    State::Scalar(Scalar::Value(base)) if base.matches::<Number>() => {
                        let base = base.opt_cast_into().expect("numeric constant");
                        l.log_const(base)
                    }
                    base if base.is_none() => l.ln(),
                    other => Err(TCError::invalid_type(other, "a Tensor or Number")),
                }?;

                Ok(State::Collection(Collection::Tensor(log)))
            })
        }))
    }
}

// TODO: delete this after implementing a custom Matrix type (separate from Tensor)
struct MatMulHandler {
    tensor: Tensor,
}

impl MatMulHandler {
    fn new<T: Into<Tensor>>(tensor: T) -> Self {
        Self {
            tensor: tensor.into(),
        }
    }
}

impl<'a> Handler<'a> for MatMulHandler {
    fn post<'b>(self: Box<Self>) -> Option<PostHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|_txn, mut params| {
            Box::pin(async move {
                let right: Tensor = params.require(&RIGHT.into())?;
                params.expect_empty()?;

                if self.tensor.ndim() != 2 {
                    return Err(TCError::invalid_value(self.tensor, "a matrix"));
                }

                if right.ndim() != 2 {
                    return Err(TCError::invalid_value(right, "a matrix"));
                }

                if self.tensor.shape()[1] != right.shape()[0] {
                    return Err(bad_request!(
                        "invalid dimensions for matmul: {} @ {}",
                        self.tensor.shape(),
                        right.shape()
                    ));
                }

                // ij,jk->ijk
                let left = self.tensor.expand_dims(2)?;
                let right = right.expand_dims(0)?;
                let (left, right) = broadcast(left, right)?;
                let op = left.mul(right)?;

                // ijk -> uk
                op.sum(1, false).map(State::from)
            })
        }))
    }
}

struct NormHandler {
    tensor: Tensor,
}

impl NormHandler {
    async fn call(
        tensor: Tensor,
        txn: Txn,
        axis: Option<usize>,
        keepdims: bool,
    ) -> TCResult<State> {
        if let Some(axis) = axis {
            debug!("norm of {} at axis {}", tensor, axis);

            return tensor
                .pow_const(2i32.into())
                .and_then(|pow| pow.sum(axis, keepdims))
                .and_then(|sum| sum.pow_const(0.5f32.into()))
                .map(Collection::Tensor)
                .map(State::Collection);
        } else if tensor.ndim() <= 2 {
            if keepdims {
                Err(not_implemented!("matrix norm with keepdims"))
            } else {
                let squared = tensor.pow_const(2i32.into())?;
                let summed = squared.sum_all(txn).await?;
                Ok(Value::from(summed.pow(0.5f32.into())).into())
            }
        } else {
            debug!("norm of {}, keepdims is {}", tensor, keepdims);

            tensor
                .pow_const(2i32.into())
                .and_then(|pow| {
                    let axis = pow.ndim() - 1;
                    pow.sum(axis, keepdims)
                })
                .and_then(|pow| {
                    let axis = pow.ndim() - if keepdims { 2 } else { 1 };
                    pow.sum(axis, keepdims)
                })
                .and_then(|sum| sum.pow_const(0.5f32.into()))
                .map(Collection::Tensor)
                .map(State::Collection)
        }
    }
}

impl<'a> Handler<'a> for NormHandler {
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, key| {
            Box::pin(async move {
                let axis = if key.is_some() {
                    cast_axis(key, self.tensor.ndim()).map(Some)?
                } else {
                    None
                };

                Self::call(self.tensor, txn.clone(), axis, false).await
            })
        }))
    }

    fn post<'b>(self: Box<Self>) -> Option<PostHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, mut params| {
            Box::pin(async move {
                let axis = if params.contains_key::<Id>(&AXIS.into()) {
                    let axis = params.require(&AXIS.into())?;
                    cast_axis(axis, self.tensor.ndim()).map(Some)?
                } else {
                    None
                };

                let keepdims = params.or_default(&KEEPDIMS.into())?;

                params.expect_empty()?;

                Self::call(self.tensor, txn.clone(), axis, keepdims).await
            })
        }))
    }
}

impl From<Tensor> for NormHandler {
    fn from(tensor: Tensor) -> Self {
        Self { tensor }
    }
}

struct ReduceHandler<'a, T: TensorReduce<fs::Dir>> {
    tensor: &'a T,
    reduce: fn(T, usize, bool) -> TCResult<<T as TensorReduce<fs::Dir>>::Reduce>,
    reduce_all: fn(&'a T, Txn) -> TCBoxTryFuture<'a, Number>,
}

impl<'a, T: TensorReduce<fs::Dir>> ReduceHandler<'a, T> {
    fn new(
        tensor: &'a T,
        reduce: fn(T, usize, bool) -> TCResult<<T as TensorReduce<fs::Dir>>::Reduce>,
        reduce_all: fn(&'a T, Txn) -> TCBoxTryFuture<'a, Number>,
    ) -> Self {
        Self {
            tensor,
            reduce,
            reduce_all,
        }
    }
}

impl<'a, T: TensorReduce<fs::Dir>> ReduceHandler<'a, T>
where
    T: TensorAccess + TensorReduce<fs::Dir> + Clone + Sync,
    Tensor: From<<T as TensorReduce<fs::Dir>>::Reduce>,
{
    async fn call(&self, txn: &Txn, axis: Option<usize>, keepdims: bool) -> TCResult<State> {
        if axis.is_none() && keepdims {
            Err(not_implemented!("reduce all axes but keep dimensions"))
        } else if axis.is_none() || (axis == Some(0) && self.tensor.ndim() == 1) {
            (self.reduce_all)(self.tensor, txn.clone())
                .map_ok(Value::from)
                .map_ok(State::from)
                .await
        } else {
            (self.reduce)(self.tensor.clone(), axis.expect("axis"), keepdims)
                .map(Tensor::from)
                .map(Collection::from)
                .map(State::from)
        }
    }
}

impl<'a, T> Handler<'a> for ReduceHandler<'a, T>
where
    T: TensorAccess + TensorReduce<fs::Dir> + Clone + Sync,
    Tensor: From<<T as TensorReduce<fs::Dir>>::Reduce>,
{
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, key| {
            Box::pin(async move {
                let axis = if key.is_none() {
                    None
                } else {
                    cast_axis(key, self.tensor.ndim()).map(Some)?
                };

                self.call(txn, axis, false).await
            })
        }))
    }

    fn post<'b>(self: Box<Self>) -> Option<PostHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, mut params| {
            Box::pin(async move {
                let axis = if params.contains_key::<Id>(&AXIS.into()) {
                    let axis = params.require(&AXIS.into())?;
                    cast_axis(axis, self.tensor.ndim()).map(Some)?
                } else {
                    None
                };

                let keepdims = params.or_default(&KEEPDIMS.into())?;
                params.expect_empty()?;

                self.call(txn, axis, keepdims).await
            })
        }))
    }
}

struct TensorHandler<T> {
    tensor: T,
}

impl<'a, T: 'a> Handler<'a> for TensorHandler<T>
where
    T: TensorAccess
        + TensorIO<fs::Dir, Txn = Txn>
        + TensorDualIO<fs::Dir, Tensor, Txn = Txn>
        + TensorTransform
        + Clone
        + Send
        + Sync,
    <T as TensorTransform>::Slice: TensorAccess + Send,
    Tensor: From<<T as TensorTransform>::Slice>,
{
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, key| {
            Box::pin(async move {
                debug!("GET Tensor: {}", key);
                let bounds = cast_bounds(self.tensor.shape(), key)?;

                if bounds.size() == 0 {
                    return Err(bad_request!(
                        "invalid bounds for tensor with shape {}: {}",
                        self.tensor.shape(),
                        bounds
                    ));
                }

                let shape = self.tensor.shape();
                if bounds.is_coord(shape) {
                    let coord = bounds.as_coord(shape).expect("tensor coordinate");

                    self.tensor
                        .read_value(txn.clone(), coord)
                        .map_ok(Value::from)
                        .map_ok(State::from)
                        .await
                } else {
                    self.tensor
                        .slice(bounds)
                        .map(Tensor::from)
                        .map(Collection::from)
                        .map(State::from)
                }
            })
        }))
    }

    fn put<'b>(self: Box<Self>) -> Option<PutHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(move |txn, key, value| {
            debug!("PUT Tensor: {} <- {}", key, value);
            Box::pin(write(self.tensor, txn, key, value))
        }))
    }
}

impl<T> From<T> for TensorHandler<T> {
    fn from(tensor: T) -> Self {
        Self { tensor }
    }
}

struct UnaryHandler {
    tensor: Tensor,
    op: fn(&Tensor) -> TCResult<Tensor>,
    op_name: &'static str,
}

impl UnaryHandler {
    fn new(tensor: Tensor, op: fn(&Tensor) -> TCResult<Tensor>, op_name: &'static str) -> Self {
        Self {
            tensor,
            op,
            op_name,
        }
    }
}

impl<'a> Handler<'a> for UnaryHandler {
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|_txn, key| {
            Box::pin(async move {
                self.tensor.shape().validate(self.op_name)?;

                let tensor = if key.is_none() {
                    self.tensor
                } else {
                    let bounds = cast_bounds(self.tensor.shape(), key.into())?;
                    self.tensor.slice(bounds)?
                };

                (self.op)(&tensor).map(Collection::from).map(State::from)
            })
        }))
    }
}

struct UnaryHandlerAsync<F: Send> {
    tensor: Tensor,
    op: fn(Tensor, Txn) -> F,
    op_name: &'static str,
}

impl<'a, F: Send> UnaryHandlerAsync<F> {
    fn new(tensor: Tensor, op: fn(Tensor, Txn) -> F, op_name: &'static str) -> Self {
        Self {
            tensor,
            op,
            op_name,
        }
    }
}

impl<'a, F> Handler<'a> for UnaryHandlerAsync<F>
where
    F: Future<Output = TCResult<bool>> + Send + 'a,
{
    fn get<'b>(self: Box<Self>) -> Option<GetHandler<'a, 'b>>
    where
        'b: 'a,
    {
        Some(Box::new(|txn, key| {
            Box::pin(async move {
                self.tensor.shape().validate(self.op_name)?;

                let txn = txn.clone();

                if key.is_none() {
                    (self.op)(self.tensor, txn).map_ok(State::from).await
                } else {
                    let bounds = cast_bounds(self.tensor.shape(), key.into())?;
                    let slice = self.tensor.slice(bounds)?;
                    (self.op)(slice, txn).map_ok(State::from).await
                }
            })
        }))
    }
}

impl<B: DenseWrite<fs::File<u64, Array>, fs::File<NodeId, Node>, fs::Dir, Txn>> Route
    for DenseTensor<B>
{
    fn route<'a>(&'a self, path: &'a [PathSegment]) -> Option<Box<dyn Handler<'a> + 'a>> {
        route(self, path)
    }
}

impl<A> Route for SparseTensor<A>
where
    A: SparseAccess<fs::File<u64, Array>, File<NodeId, Node>, fs::Dir, Txn> + SparseWrite,
    Self: Clone,
{
    fn route<'a>(&'a self, path: &'a [PathSegment]) -> Option<Box<dyn Handler<'a> + 'a>> {
        route(self, path)
    }
}

impl Route for Tensor {
    fn route<'a>(&'a self, path: &'a [PathSegment]) -> Option<Box<dyn Handler<'a> + 'a>> {
        route(self, path)
    }
}

fn route<'a, T>(tensor: &'a T, path: &'a [PathSegment]) -> Option<Box<dyn Handler<'a> + 'a>>
where
    T: TensorAccess
        + TensorInstance
        + TensorBoolean<Tensor, Combine = Tensor>
        + TensorDiagonal<fs::Dir, Txn = Txn>
        + TensorCompare<Tensor, Compare = Tensor, Dense = Tensor>
        + TensorDualIO<fs::Dir, Tensor, Txn = Txn>
        + TensorIndex<fs::Dir, Txn = Txn>
        + TensorIO<fs::Dir, Txn = Txn>
        + TensorMath<Tensor, Combine = Tensor>
        + TensorReduce<fs::Dir, Txn = Txn>
        + TensorTransform
        + TensorTrig
        + TensorUnary<fs::Dir, Txn = Txn>
        + Clone
        + Send
        + Sync,
    Collection: From<T>,
    Tensor: From<T>,
    Tensor: From<<T as TensorDiagonal<fs::Dir>>::Diagonal>,
    Tensor: From<<T as TensorIndex<fs::Dir>>::Index>,
    Tensor: From<<T as TensorInstance>::Dense> + From<<T as TensorInstance>::Sparse>,
    Tensor: From<<T as TensorReduce<fs::Dir>>::Reduce>,
    Tensor: From<<T as TensorTransform>::Broadcast>,
    Tensor: From<<T as TensorTransform>::Cast>,
    Tensor: From<<T as TensorTransform>::Expand>,
    Tensor: From<<T as TensorTransform>::Flip>,
    Tensor: From<<T as TensorTransform>::Reshape>,
    Tensor: From<<T as TensorTransform>::Slice>,
    Tensor: From<<T as TensorTransform>::Transpose>,
    <T as TensorTransform>::Slice: TensorAccess + Send + 'a,
{
    if path.is_empty() {
        Some(Box::new(TensorHandler::from(tensor.clone())))
    } else if path.len() == 1 {
        match path[0].as_str() {
            // attributes
            "dtype" => {
                return Some(Box::new(AttributeHandler::from(State::Object(
                    Object::Class(StateType::from(tensor.dtype()).into()),
                ))))
            }

            "ndim" => {
                return Some(Box::new(AttributeHandler::from(Value::Number(
                    (tensor.ndim() as u64).into(),
                ))))
            }

            "shape" => {
                return Some(Box::new(AttributeHandler::from(
                    tensor
                        .shape()
                        .iter()
                        .map(|dim| Number::from(*dim))
                        .collect::<Tuple<Value>>(),
                )))
            }

            "size" => {
                return Some(Box::new(AttributeHandler::from(Value::Number(
                    tensor.size().into(),
                ))))
            }

            // reduce ops (which require borrowing)
            "max" => {
                return Some(Box::new(ReduceHandler::new(
                    tensor,
                    TensorReduce::max,
                    TensorReduce::max_all,
                )))
            }
            "min" => {
                return Some(Box::new(ReduceHandler::new(
                    tensor,
                    TensorReduce::min,
                    TensorReduce::min_all,
                )))
            }
            "product" => {
                return Some(Box::new(ReduceHandler::new(
                    tensor,
                    TensorReduce::product,
                    TensorReduce::product_all,
                )))
            }
            "sum" => {
                return Some(Box::new(ReduceHandler::new(
                    tensor,
                    TensorReduce::sum,
                    TensorReduce::sum_all,
                )))
            }
            _ => {}
        };

        let tensor = tensor.clone();

        match path[0].as_str() {
            // to stream
            "elements" => Some(Box::new(ElementsHandler::new(tensor))),

            // views
            "dense" => {
                return Some(Box::new(SelfHandlerOwned::from(Tensor::from(
                    tensor.into_dense(),
                ))));
            }

            "sparse" => {
                return Some(Box::new(SelfHandlerOwned::from(Tensor::from(
                    tensor.into_sparse(),
                ))));
            }

            // boolean ops
            "and" => Some(Box::new(DualHandler::new(
                tensor,
                TensorBoolean::and,
                TensorBooleanConst::and_const,
                "and",
            ))),
            "or" => Some(Box::new(DualHandler::new(
                tensor,
                TensorBoolean::or,
                TensorBooleanConst::or_const,
                "or",
            ))),
            "xor" => Some(Box::new(DualHandler::new(
                tensor,
                TensorBoolean::xor,
                TensorBooleanConst::xor_const,
                "xor",
            ))),

            // comparison ops
            "eq" => Some(Box::new(DualHandler::new(
                tensor,
                TensorCompare::eq,
                TensorCompareConst::eq_const,
                "eq",
            ))),
            "gt" => Some(Box::new(DualHandler::new(
                tensor,
                TensorCompare::gt,
                TensorCompareConst::gt_const,
                "gt",
            ))),
            "gte" => Some(Box::new(DualHandler::new(
                tensor,
                TensorCompare::gte,
                TensorCompareConst::gte_const,
                "gte",
            ))),
            "lt" => Some(Box::new(DualHandler::new(
                tensor,
                TensorCompare::lt,
                TensorCompareConst::lt_const,
                "lt",
            ))),
            "lte" => Some(Box::new(DualHandler::new(
                tensor,
                TensorCompare::lte,
                TensorCompareConst::lte_const,
                "lte",
            ))),
            "ne" => Some(Box::new(DualHandler::new(
                tensor,
                TensorCompare::ne,
                TensorCompareConst::ne_const,
                "ne",
            ))),

            // trigonometry
            "asin" => Some(Box::new(UnaryHandler::new(
                tensor.into(),
                TensorTrig::asin,
                "asin",
            ))),
            "sin" => Some(Box::new(UnaryHandler::new(
                tensor.into(),
                TensorTrig::sin,
                "sin",
            ))),
            "asinh" => Some(Box::new(UnaryHandler::new(
                tensor.into(),
                TensorTrig::asinh,
                "asinh",
            ))),
            "sinh" => Some(Box::new(UnaryHandler::new(
                tensor.into(),
                TensorTrig::sinh,
                "sinh",
            ))),

            "acos" => Some(Box::new(UnaryHandler::new(
                tensor.into(),
                TensorTrig::acos,
                "acos",
            ))),
            "cos" => Some(Box::new(UnaryHandler::new(
                tensor.into(),
                TensorTrig::cos,
                "cos",
            ))),
            "acosh" => Some(Box::new(UnaryHandler::new(
                tensor.into(),
                TensorTrig::acosh,
                "acosh",
            ))),
            "cosh" => Some(Box::new(UnaryHandler::new(
                tensor.into(),
                TensorTrig::cosh,
                "cosh",
            ))),

            "atan" => Some(Box::new(UnaryHandler::new(
                tensor.into(),
                TensorTrig::atan,
                "atan",
            ))),
            "tan" => Some(Box::new(UnaryHandler::new(
                tensor.into(),
                TensorTrig::tan,
                "tan",
            ))),
            "atanh" => Some(Box::new(UnaryHandler::new(
                tensor.into(),
                TensorTrig::atanh,
                "atanh",
            ))),
            "tanh" => Some(Box::new(UnaryHandler::new(
                tensor.into(),
                TensorTrig::tanh,
                "tanh",
            ))),

            // unary ops
            "abs" => Some(Box::new(UnaryHandler::new(
                tensor.into(),
                TensorUnary::abs,
                "abs",
            ))),
            "all" => Some(Box::new(UnaryHandlerAsync::new(
                tensor.into(),
                TensorUnary::all,
                "all",
            ))),
            "any" => Some(Box::new(UnaryHandlerAsync::new(
                tensor.into(),
                TensorUnary::any,
                "any",
            ))),
            "exp" => Some(Box::new(UnaryHandler::new(
                tensor.into(),
                TensorUnary::exp,
                "exp",
            ))),
            "not" => Some(Box::new(UnaryHandler::new(
                tensor.into(),
                TensorUnary::not,
                "not",
            ))),
            "round" => Some(Box::new(UnaryHandler::new(
                tensor.into(),
                TensorUnary::round,
                "round",
            ))),

            // basic math
            "add" => Some(Box::new(DualHandler::new(
                tensor,
                TensorMath::add,
                TensorMathConst::add_const,
                "add",
            ))),
            "div" => Some(Box::new(DualHandler::new(
                tensor,
                TensorMath::div,
                TensorMathConst::div_const,
                "div",
            ))),
            "log" => Some(Box::new(LogHandler::new(tensor))),
            "matmul" => Some(Box::new(MatMulHandler::new(tensor))),
            "mul" => Some(Box::new(DualHandler::new(
                tensor,
                TensorMath::mul,
                TensorMathConst::mul_const,
                "mul",
            ))),
            "pow" => Some(Box::new(DualHandler::new(
                tensor,
                TensorMath::pow,
                TensorMathConst::pow_const,
                "pow",
            ))),
            "sub" => Some(Box::new(DualHandler::new(
                tensor,
                TensorMath::sub,
                TensorMathConst::sub_const,
                "sub",
            ))),

            // transforms
            "broadcast" => Some(Box::new(BroadcastHandler::from(tensor))),
            "cast" => Some(Box::new(CastHandler::from(tensor))),
            "flip" => Some(Box::new(FlipHandler::from(tensor))),
            "expand_dims" => Some(Box::new(ExpandHandler::from(tensor))),
            "reshape" => Some(Box::new(ReshapeHandler::from(tensor))),
            "transpose" => Some(Box::new(TransposeHandler::from(tensor))),

            // indexing
            "argmax" => Some(Box::new(ArgmaxHandler::from(tensor))),

            // linear algebra
            "diagonal" => Some(Box::new(DiagonalHandler::from(tensor))),

            // other
            "norm" => Some(Box::new(NormHandler::from(Tensor::from(tensor)))),

            _ => None,
        }
    } else {
        None
    }
}

pub struct Static;

impl Route for Static {
    fn route<'a>(&'a self, path: &'a [PathSegment]) -> Option<Box<dyn Handler<'a> + 'a>> {
        if path.is_empty() {
            return None;
        }

        match path[0].as_str() {
            "dense" => TensorType::Dense.route(&path[1..]),
            "sparse" => TensorType::Sparse.route(&path[1..]),
            "copy_from" if path.len() == 1 => Some(Box::new(CopyFromHandler)),
            "load" if path.len() == 1 => Some(Box::new(LoadHandler { class: None })),
            "tile" if path.len() == 1 => Some(Box::new(TileHandler)),
            _ => None,
        }
    }
}

async fn constant(
    txn: &Txn,
    shape: Shape,
    value: Number,
) -> TCResult<DenseTensor<DenseTensorFile>> {
    let file = create_file(txn).await?;
    DenseTensor::constant(file, *txn.id(), shape, value)
}

async fn create_sparse(txn: &Txn, schema: Schema) -> TCResult<SparseTensor<SparseTable>> {
    let store = txn.context().create_store_unique(*txn.id()).await?;
    SparseTensor::create(*txn.id(), schema, store)
}

async fn write<T>(tensor: T, txn: &Txn, key: Value, value: State) -> TCResult<()>
where
    T: TensorAccess
        + TensorIO<fs::Dir, Txn = Txn>
        + TensorDualIO<fs::Dir, Tensor, Txn = Txn>
        + TensorTransform
        + Clone,
    <T as TensorTransform>::Slice: TensorAccess + Send,
{
    debug!("write {} to {}", value, key);
    let bounds = cast_bounds(tensor.shape(), key)?;

    match value {
        State::Collection(Collection::Tensor(value)) => {
            tensor.write(txn.clone(), bounds, value).await
        }
        State::Scalar(scalar) => {
            let value = scalar.try_cast_into(|v| TCError::invalid_type(v, "a Tensor element"))?;

            tensor.write_value(*txn.id(), bounds, value).await
        }
        other => Err(bad_request!("cannot write {} to a Tensor", other)),
    }
}

async fn create_file(txn: &Txn) -> TCResult<fs::File<u64, Array>> {
    txn.context().create_file_unique(*txn.id()).await
}

async fn create_tensor(class: TensorType, schema: Schema, txn: &Txn) -> TCResult<Tensor> {
    match class {
        TensorType::Dense => {
            constant(txn, schema.shape, schema.dtype.zero())
                .map_ok(Tensor::from)
                .await
        }
        TensorType::Sparse => create_sparse(txn, schema).map_ok(Tensor::from).await,
    }
}

fn cast_bound(dim: u64, bound: Value) -> TCResult<u64> {
    let bound = i64::try_cast_from(bound, |v| TCError::invalid_value(v, "an axis bound"))?;
    if bound.abs() as u64 > dim {
        return Err(bad_request!(
            "index {} is out of bounds for dimension {}",
            bound,
            dim
        ));
    }

    if bound < 0 {
        Ok(dim - bound.abs() as u64)
    } else {
        Ok(bound as u64)
    }
}

fn cast_axis(axis: Value, ndim: usize) -> TCResult<usize> {
    debug!("cast axis {} with ndim {}", axis, ndim);

    let axis = Number::try_cast_from(axis, |v| TCError::invalid_type(v, "a Tensor axis"))?;

    match axis {
        Number::UInt(axis) => Ok(axis.into()),
        Number::Int(axis) if axis < 0.into() => {
            Ok(Number::from(ndim as u64) - Number::Int(axis.abs()))
        }
        Number::Int(axis) => Ok(axis.into()),
        other => Err(TCError::invalid_type(other, "a Tensor axis")),
    }
    .map(|axis: Number| axis.cast_into())
}

fn cast_range(dim: u64, range: Range) -> TCResult<AxisBounds> {
    debug!("cast range from {} with dimension {}", range, dim);

    let start = match range.start {
        Bound::Un => 0,
        Bound::In(start) => cast_bound(dim, start)?,
        Bound::Ex(start) => cast_bound(dim, start)? + 1,
    };

    let end = match range.end {
        Bound::Un => dim,
        Bound::In(end) => cast_bound(dim, end)? + 1,
        Bound::Ex(end) => cast_bound(dim, end)?,
    };

    if end >= start {
        Ok(AxisBounds::In(start..end))
    } else {
        Err(TCError::invalid_value(
            format!("{}..{}", start, end),
            "axis bounds",
        ))
    }
}

pub fn cast_bounds(shape: &Shape, value: Value) -> TCResult<Bounds> {
    debug!("tensor bounds from {} (shape is {})", value, shape);

    match value {
        Value::None => Ok(Bounds::all(shape)),
        Value::Number(i) => {
            let bound = cast_bound(shape[0], i.into())?;
            Ok(Bounds::from(vec![bound]))
        }
        Value::Tuple(range) if range.matches::<(Bound, Bound)>() => {
            if shape.is_empty() {
                return Err(bad_request!(
                    "empty Tensor has no valid bounds, but the requested range is {}",
                    range,
                ));
            }

            let range = range.opt_cast_into().unwrap();
            Ok(Bounds::from(vec![cast_range(shape[0], range)?]))
        }
        Value::Tuple(bounds) => {
            if bounds.len() > shape.len() {
                return Err(bad_request!(
                    "tensor of shape {} does not support bounds with {} axes",
                    shape,
                    bounds.len()
                ));
            }

            let mut axes = Vec::with_capacity(shape.len());

            for (axis, bound) in bounds.into_inner().into_iter().enumerate() {
                debug!(
                    "bound for axis {} with dimension {} is {}",
                    axis, shape[axis], bound
                );

                let bound = if bound.is_none() {
                    AxisBounds::all(shape[axis])
                } else if bound.matches::<Range>() {
                    let range = Range::opt_cast_from(bound).unwrap();
                    cast_range(shape[axis], range)?
                } else if bound.matches::<Vec<u64>>() {
                    bound.opt_cast_into().map(AxisBounds::Of).unwrap()
                } else if let Value::Number(value) = bound {
                    cast_bound(shape[axis], value.into()).map(AxisBounds::At)?
                } else {
                    return Err(bad_request!("invalid bound {} for axis {}", bound, axis));
                };

                axes.push(bound);
            }

            Ok(Bounds::from(axes))
        }
        other => Err(TCError::invalid_value(other, "Tensor bounds")),
    }
}

fn cast_shape(source_shape: &Shape, value: Tuple<Value>) -> TCResult<Vec<u64>> {
    if value.is_empty() {
        return Err(TCError::invalid_value(value, "a Tensor shape"));
    }

    let mut shape = vec![1; value.len()];
    if value.iter().filter(|dim| *dim == &Value::None).count() > 1 {
        return Err(bad_request!(
            "Tensor reshape accepts a maximum of one unknown dimension"
        ));
    }

    let mut unknown = None;
    for (x, dim) in value.iter().enumerate() {
        match dim {
            Value::Number(dim) if dim >= &Number::from(1) => {
                shape[x] = (*dim).cast_into();
            }
            Value::None => {
                unknown = Some(x);
            }
            Value::Number(dim) if dim == &Number::from(-1) => {
                return Err(bad_request!(
                    "use value/none to specify an unknown dimension, not -1"
                ));
            }
            other => {
                return Err(TCError::invalid_value(
                    other,
                    "the dimension of a Tensor axis",
                ))
            }
        }
    }

    let size = source_shape.size();
    if let Some(unknown) = unknown {
        let known: u64 = shape.iter().product();
        if size % known == 0 {
            shape[unknown] = size / known;
        } else {
            return Err(bad_request!(
                "cannot reshape Tensor with size {} into shape {}",
                size,
                value
            ));
        }
    }

    if shape.iter().product::<u64>() == size {
        Ok(shape)
    } else {
        Err(bad_request!(
            "cannot reshape Tensor with shape {} into shape {}",
            source_shape,
            value
        ))
    }
}
