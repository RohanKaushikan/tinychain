use async_trait::async_trait;

use crate::class::TCBoxTryFuture;
use crate::error::{self, TCResult};
use crate::scalar::value::number::*;
use crate::transaction::{Txn, TxnId};

mod einsum;
mod transform;

pub mod bounds;
pub mod class;
pub mod dense;
pub mod sparse;

pub use class::{Tensor, TensorAccessor, TensorBaseType, TensorType, TensorView};
pub use dense::{Array, DenseTensor};
pub use einsum::einsum;
pub use sparse::SparseTensor;

pub const ERR_NONBIJECTIVE_WRITE: &str = "Cannot write to a derived Tensor which is not a \
bijection of its source. Consider copying first, or writing directly to the source Tensor.";

pub trait IntoView {
    fn into_view(self) -> TensorView;
}

#[async_trait]
pub trait TensorBoolean<O>: TensorAccessor + Sized {
    type Unary: IntoView;
    type Combine: IntoView;

    async fn all(&self, txn: Txn) -> TCResult<bool>;

    async fn any(&self, txn: Txn) -> TCResult<bool>;

    fn and(&self, other: &O) -> TCResult<Self::Combine>;

    fn not(&self) -> TCResult<Self::Unary>;

    fn or(&self, other: &O) -> TCResult<Self::Combine>;

    fn xor(&self, other: &O) -> TCResult<Self::Combine>;
}

#[async_trait]
pub trait TensorCompare<O>: TensorAccessor + Sized {
    type Compare: IntoView;
    type Dense: IntoView;

    async fn eq(&self, other: &O, txn: Txn) -> TCResult<Self::Dense>;

    fn gt(&self, other: &O) -> TCResult<Self::Compare>;

    async fn gte(&self, other: &O, txn: Txn) -> TCResult<Self::Dense>;

    fn lt(&self, other: &O) -> TCResult<Self::Compare>;

    async fn lte(&self, other: &O, txn: Txn) -> TCResult<Self::Dense>;

    fn ne(&self, other: &O) -> TCResult<Self::Compare>;
}

#[async_trait]
pub trait TensorIO<O>: TensorAccessor + Sized {
    async fn mask(&self, txn: &Txn, other: O) -> TCResult<()>;

    async fn read_value(&self, txn: &Txn, coord: &[u64]) -> TCResult<Number>;

    async fn write(&self, txn: Txn, bounds: bounds::Bounds, value: O) -> TCResult<()>;

    async fn write_value(
        &self,
        txn_id: TxnId,
        bounds: bounds::Bounds,
        value: Number,
    ) -> TCResult<()>;

    async fn write_value_at(&self, txn_id: TxnId, coord: Vec<u64>, value: Number) -> TCResult<()>;
}

pub trait TensorMath<O>: TensorAccessor + Sized {
    type Unary: IntoView;
    type Combine: IntoView;

    fn abs(&self) -> TCResult<Self::Unary>;

    fn add(&self, other: &O) -> TCResult<Self::Combine>;

    fn multiply(&self, other: &O) -> TCResult<Self::Combine>;
}

pub trait TensorReduce: TensorAccessor + Sized {
    type Reduce: IntoView;

    fn product(&self, axis: usize) -> TCResult<Self::Reduce>;

    fn product_all(&self, txn: Txn) -> TCBoxTryFuture<Number>;

    fn sum(&self, axis: usize) -> TCResult<Self::Reduce>;

    fn sum_all(&self, txn: Txn) -> TCBoxTryFuture<Number>;
}

pub trait TensorTransform: TensorAccessor + Sized {
    type Cast: IntoView;
    type Broadcast: IntoView;
    type Expand: IntoView;
    type Slice: IntoView;
    type Reshape: IntoView;
    type Transpose: IntoView;

    fn as_type(&self, dtype: NumberType) -> TCResult<Self::Cast>;

    fn broadcast(&self, shape: bounds::Shape) -> TCResult<Self::Broadcast>;

    fn expand_dims(&self, axis: usize) -> TCResult<Self::Expand>;

    fn slice(&self, bounds: bounds::Bounds) -> TCResult<Self::Slice>;

    fn reshape(&self, shape: bounds::Shape) -> TCResult<Self::Reshape>;

    fn transpose(&self, permutation: Option<Vec<usize>>) -> TCResult<Self::Transpose>;
}

#[async_trait]
impl TensorBoolean<TensorView> for TensorView {
    type Unary = TensorView;
    type Combine = TensorView;

    async fn all(&self, txn: Txn) -> TCResult<bool> {
        match self {
            Self::Dense(dense) => dense.all(txn).await,
            Self::Sparse(sparse) => sparse.all(txn).await,
        }
    }

    async fn any(&self, txn: Txn) -> TCResult<bool> {
        match self {
            Self::Dense(dense) => dense.any(txn).await,
            Self::Sparse(sparse) => sparse.any(txn).await,
        }
    }

    fn and(&self, other: &Self) -> TCResult<Self> {
        use TensorView::*;
        match (self, other) {
            (Dense(left), Dense(right)) => left.and(right).map(Self::from),
            (Sparse(left), Sparse(right)) => left.and(right).map(Self::from),
            (Sparse(left), Dense(right)) => left
                .and(&SparseTensor::from_dense(right.clone()))
                .map(Self::from),

            _ => other.and(self),
        }
    }

    fn not(&self) -> TCResult<Self> {
        match self {
            Self::Dense(dense) => dense.not().map(Self::from),
            Self::Sparse(sparse) => sparse.not().map(Self::from),
        }
    }

    fn or(&self, other: &Self) -> TCResult<Self> {
        use TensorView::*;
        match (self, other) {
            (Dense(left), Dense(right)) => left.or(right).map(Self::from),
            (Sparse(left), Sparse(right)) => left.or(right).map(Self::from),
            (Dense(left), Sparse(right)) => left
                .or(&DenseTensor::from_sparse(right.clone()))
                .map(Self::from),

            _ => other.or(self),
        }
    }

    fn xor(&self, other: &Self) -> TCResult<Self> {
        use TensorView::*;
        match (self, other) {
            (Dense(left), Dense(right)) => left.xor(right).map(Self::from),
            (Sparse(left), _) => Dense(DenseTensor::from_sparse(left.clone())).xor(other),
            (left, right) => right.xor(left),
        }
    }
}

#[async_trait]
impl TensorCompare<TensorView> for TensorView {
    type Compare = Self;
    type Dense = Self;

    async fn eq(&self, other: &Self, txn: Txn) -> TCResult<Self> {
        let eq = match (self, other) {
            (Self::Dense(left), Self::Dense(right)) => left.eq(right, txn).await,
            (Self::Sparse(left), Self::Sparse(right)) => left.eq(right, txn).await,
            (Self::Dense(left), Self::Sparse(right)) => {
                left.eq(&DenseTensor::from_sparse(right.clone()), txn).await
            }
            (Self::Sparse(left), Self::Dense(right)) => {
                DenseTensor::from_sparse(left.clone()).eq(right, txn).await
            }
        };

        eq.map(Self::from)
    }

    fn gt(&self, other: &Self) -> TCResult<Self> {
        match (self, other) {
            (Self::Dense(left), Self::Dense(right)) => left.gt(right).map(Self::from),
            (Self::Sparse(left), Self::Sparse(right)) => left.gt(right).map(Self::from),
            (Self::Dense(left), Self::Sparse(right)) => left
                .gt(&DenseTensor::from_sparse(right.clone()))
                .map(Self::from),
            (Self::Sparse(left), Self::Dense(right)) => left
                .gt(&SparseTensor::from_dense(right.clone()))
                .map(Self::from),
        }
    }

    async fn gte(&self, other: &Self, txn: Txn) -> TCResult<Self> {
        let gte = match (self, other) {
            (Self::Dense(left), Self::Dense(right)) => left.gte(right, txn).await,
            (Self::Sparse(left), Self::Sparse(right)) => left.gte(right, txn).await,
            (Self::Dense(left), Self::Sparse(right)) => {
                left.gte(&DenseTensor::from_sparse(right.clone()), txn)
                    .await
            }
            (Self::Sparse(left), Self::Dense(right)) => {
                DenseTensor::from_sparse(left.clone()).gte(right, txn).await
            }
        };

        gte.map(Self::from)
    }

    fn lt(&self, other: &Self) -> TCResult<Self> {
        match (self, other) {
            (Self::Dense(left), Self::Dense(right)) => left.lt(right).map(Self::from),
            (Self::Sparse(left), Self::Sparse(right)) => left.lt(right).map(Self::from),
            (Self::Dense(left), Self::Sparse(right)) => left
                .lt(&DenseTensor::from_sparse(right.clone()))
                .map(Self::from),
            (Self::Sparse(left), Self::Dense(right)) => left
                .lt(&SparseTensor::from_dense(right.clone()))
                .map(Self::from),
        }
    }

    async fn lte(&self, other: &Self, txn: Txn) -> TCResult<Self> {
        let lte = match (self, other) {
            (Self::Dense(left), Self::Dense(right)) => left.lte(right, txn).await,
            (Self::Sparse(left), Self::Sparse(right)) => left.lte(right, txn).await,
            (Self::Dense(left), Self::Sparse(right)) => {
                left.lte(&DenseTensor::from_sparse(right.clone()), txn)
                    .await
            }
            (Self::Sparse(left), Self::Dense(right)) => {
                DenseTensor::from_sparse(left.clone()).lte(right, txn).await
            }
        };

        lte.map(Self::from)
    }

    fn ne(&self, other: &Self) -> TCResult<Self> {
        match (self, other) {
            (Self::Dense(left), Self::Dense(right)) => left.ne(right).map(Self::from),
            (Self::Sparse(left), Self::Sparse(right)) => left.ne(right).map(Self::from),
            (Self::Dense(left), Self::Sparse(right)) => left
                .ne(&DenseTensor::from_sparse(right.clone()))
                .map(Self::from),
            (Self::Sparse(left), Self::Dense(right)) => DenseTensor::from_sparse(left.clone())
                .ne(right)
                .map(Self::from),
        }
    }
}

#[async_trait]
impl TensorIO<TensorView> for TensorView {
    async fn mask(&self, txn: &Txn, other: Self) -> TCResult<()> {
        match (self, &other) {
            (Self::Dense(l), Self::Dense(r)) => l.mask(txn, r.clone()).await,
            (Self::Sparse(l), Self::Sparse(r)) => l.mask(txn, r.clone()).await,
            (Self::Sparse(l), Self::Dense(r)) => {
                l.mask(txn, SparseTensor::from_dense(r.clone())).await
            }
            (Self::Dense(l), Self::Sparse(r)) => {
                l.mask(txn, DenseTensor::from_sparse(r.clone())).await
            }
        }
    }

    async fn read_value(&self, txn: &Txn, coord: &[u64]) -> TCResult<Number> {
        match self {
            Self::Dense(dense) => dense.read_value(txn, coord).await,
            Self::Sparse(sparse) => sparse.read_value(txn, coord).await,
        }
    }

    async fn write(&self, txn: Txn, bounds: bounds::Bounds, value: Self) -> TCResult<()> {
        match self {
            Self::Dense(this) => match value {
                Self::Dense(that) => this.write(txn, bounds, that).await,
                Self::Sparse(that) => {
                    this.write(txn, bounds, DenseTensor::from_sparse(that))
                        .await
                }
            },
            Self::Sparse(this) => match value {
                Self::Dense(that) => {
                    this.write(txn, bounds, SparseTensor::from_dense(that))
                        .await
                }
                Self::Sparse(that) => this.write(txn, bounds, that).await,
            },
        }
    }

    async fn write_value(
        &self,
        txn_id: TxnId,
        bounds: bounds::Bounds,
        value: Number,
    ) -> TCResult<()> {
        match self {
            Self::Dense(dense) => dense.write_value(txn_id, bounds, value).await,
            Self::Sparse(sparse) => sparse.write_value(txn_id, bounds, value).await,
        }
    }

    async fn write_value_at(&self, txn_id: TxnId, coord: Vec<u64>, value: Number) -> TCResult<()> {
        match self {
            Self::Dense(dense) => dense.write_value_at(txn_id, coord, value).await,
            Self::Sparse(sparse) => sparse.write_value_at(txn_id, coord, value).await,
        }
    }
}

impl TensorMath<TensorView> for TensorView {
    type Unary = Self;
    type Combine = Self;

    fn abs(&self) -> TCResult<Self> {
        match self {
            Self::Dense(dense) => dense.abs().map(Self::from),
            Self::Sparse(sparse) => sparse.abs().map(Self::from),
        }
    }

    fn add(&self, other: &Self) -> TCResult<Self> {
        match (self, other) {
            (Self::Dense(left), Self::Dense(right)) => left.add(right).map(Self::from),
            (Self::Sparse(left), Self::Sparse(right)) => left.add(right).map(Self::from),
            (Self::Dense(left), Self::Sparse(right)) => left
                .add(&DenseTensor::from_sparse(right.clone()))
                .map(Self::from),
            (Self::Sparse(left), Self::Dense(right)) => DenseTensor::from_sparse(left.clone())
                .add(right)
                .map(Self::from),
        }
    }

    fn multiply(&self, other: &Self) -> TCResult<Self> {
        match (self, other) {
            (Self::Dense(left), Self::Dense(right)) => left.multiply(right).map(Self::from),
            (Self::Sparse(left), Self::Sparse(right)) => left.multiply(right).map(Self::from),
            (Self::Dense(left), Self::Sparse(right)) => left
                .multiply(&DenseTensor::from_sparse(right.clone()))
                .map(Self::from),
            (Self::Sparse(left), Self::Dense(right)) => DenseTensor::from_sparse(left.clone())
                .multiply(right)
                .map(Self::from),
        }
    }
}

impl TensorReduce for TensorView {
    type Reduce = Self;

    fn product(&self, axis: usize) -> TCResult<Self::Reduce> {
        match self {
            Self::Dense(dense) => dense.product(axis).map(Self::from),
            Self::Sparse(sparse) => sparse.product(axis).map(Self::from),
        }
    }

    fn product_all(&self, txn: Txn) -> TCBoxTryFuture<Number> {
        match self {
            Self::Dense(dense) => dense.product_all(txn),
            Self::Sparse(sparse) => sparse.product_all(txn),
        }
    }

    fn sum(&self, axis: usize) -> TCResult<Self::Reduce> {
        match self {
            Self::Dense(dense) => dense.product(axis).map(Self::from),
            Self::Sparse(sparse) => sparse.product(axis).map(Self::from),
        }
    }

    fn sum_all(&self, txn: Txn) -> TCBoxTryFuture<Number> {
        match self {
            Self::Dense(dense) => dense.product_all(txn),
            Self::Sparse(sparse) => sparse.product_all(txn),
        }
    }
}

impl TensorTransform for TensorView {
    type Cast = Self;
    type Broadcast = Self;
    type Expand = Self;
    type Slice = Self;
    type Reshape = Self;
    type Transpose = Self;

    fn as_type(&self, dtype: NumberType) -> TCResult<Self> {
        match self {
            Self::Dense(dense) => dense.as_type(dtype).map(Self::from),
            Self::Sparse(sparse) => sparse.as_type(dtype).map(Self::from),
        }
    }

    fn broadcast(&self, shape: bounds::Shape) -> TCResult<Self> {
        match self {
            Self::Dense(dense) => dense.broadcast(shape).map(Self::from),
            Self::Sparse(sparse) => sparse.broadcast(shape).map(Self::from),
        }
    }

    fn expand_dims(&self, axis: usize) -> TCResult<Self> {
        match self {
            Self::Dense(dense) => dense.expand_dims(axis).map(Self::from),
            Self::Sparse(sparse) => sparse.expand_dims(axis).map(Self::from),
        }
    }

    fn slice(&self, bounds: bounds::Bounds) -> TCResult<Self> {
        match self {
            Self::Dense(dense) => dense.slice(bounds).map(Self::from),
            Self::Sparse(sparse) => sparse.slice(bounds).map(Self::from),
        }
    }

    fn reshape(&self, shape: bounds::Shape) -> TCResult<Self> {
        match self {
            Self::Dense(dense) => dense.reshape(shape).map(Self::from),
            Self::Sparse(sparse) => sparse.reshape(shape).map(Self::from),
        }
    }

    fn transpose(&self, permutation: Option<Vec<usize>>) -> TCResult<Self> {
        match self {
            Self::Dense(dense) => dense.transpose(permutation).map(Self::from),
            Self::Sparse(sparse) => sparse.transpose(permutation).map(Self::from),
        }
    }
}

fn broadcast<L: Clone + TensorTransform, R: Clone + TensorTransform>(
    left: &L,
    right: &R,
) -> TCResult<(
    <L as TensorTransform>::Broadcast,
    <R as TensorTransform>::Broadcast,
)> {
    let mut left_shape = left.shape().to_vec();
    let mut right_shape = right.shape().to_vec();

    match (left_shape.len(), right_shape.len()) {
        (l, r) if l < r => {
            for _ in 0..(r - l) {
                left_shape.insert(0, 1);
            }
        }
        (l, r) if r < l => {
            for _ in 0..(l - r) {
                right_shape.insert(0, 1);
            }
        }
        _ => {}
    }

    let mut shape = Vec::with_capacity(left_shape.len());
    for (l, r) in left_shape.iter().zip(right_shape.iter()) {
        if l == r || *l == 1 {
            shape.push(*r);
        } else if *r == 1 {
            shape.push(*l)
        } else {
            return Err(error::bad_request(
                "Cannot broadcast dimension",
                format!("{} into {}", l, r),
            ));
        }
    }
    let left = left.broadcast(shape.to_vec().into())?;
    let right = right.broadcast(shape.into())?;
    Ok((left, right))
}
