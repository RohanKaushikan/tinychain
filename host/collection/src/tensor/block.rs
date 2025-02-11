use std::fmt;

use ha_ndarray::*;
use safecast::CastInto;

use tc_error::*;
use tc_value::Number;

pub enum Block {
    F32(Array<f32>),
    F64(Array<f64>),
    I16(Array<i16>),
    I32(Array<i32>),
    I64(Array<i64>),
    U8(Array<u8>),
    U16(Array<u16>),
    U32(Array<u32>),
    U64(Array<u64>),
}

macro_rules! block_dispatch {
    ($this:ident, $var:ident, $call:expr) => {
        match $this {
            Block::F32($var) => $call,
            Block::F64($var) => $call,
            Block::I16($var) => $call,
            Block::I32($var) => $call,
            Block::I64($var) => $call,
            Block::U8($var) => $call,
            Block::U16($var) => $call,
            Block::U32($var) => $call,
            Block::U64($var) => $call,
        }
    };
}

macro_rules! block_cmp {
    ($self:ident, $other:ident, $this:ident, $that:ident, $call:expr) => {
        match ($self, $other) {
            (Self::F32($this), Self::F32($that)) => $call.map(Array::from).map_err(TCError::from),
            (Self::F64($this), Self::F64($that)) => $call.map(Array::from).map_err(TCError::from),
            (Self::I16($this), Self::I16($that)) => $call.map(Array::from).map_err(TCError::from),
            (Self::I32($this), Self::I32($that)) => $call.map(Array::from).map_err(TCError::from),
            (Self::I64($this), Self::I64($that)) => $call.map(Array::from).map_err(TCError::from),
            (Self::U8($this), Self::U8($that)) => $call.map(Array::from).map_err(TCError::from),
            (Self::U16($this), Self::U16($that)) => $call.map(Array::from).map_err(TCError::from),
            (Self::U32($this), Self::U32($that)) => $call.map(Array::from).map_err(TCError::from),
            (Self::U64($this), Self::U64($that)) => $call.map(Array::from).map_err(TCError::from),
            (this, that) => Err(bad_request!("cannot compare {this:?} with {that:?}")),
        }
    };
}

impl Block {
    pub fn cast<T: CDatatype>(self) -> TCResult<Array<T>> {
        block_dispatch!(
            self,
            this,
            this.cast().map(Array::from).map_err(TCError::from)
        )
    }

    pub fn and(self, other: Self) -> TCResult<Array<u8>> {
        block_cmp!(self, other, this, that, this.and(that))
    }

    pub fn and_scalar(self, other: Number) -> TCResult<Array<u8>> {
        block_dispatch!(
            self,
            this,
            this.and_scalar(other.cast_into())
                .map(Array::from)
                .map_err(TCError::from)
        )
    }

    pub fn not(self) -> TCResult<Array<u8>> {
        block_dispatch!(
            self,
            this,
            this.not().map(Array::from).map_err(TCError::from)
        )
    }

    pub fn or(self, other: Self) -> TCResult<Array<u8>> {
        block_cmp!(self, other, this, that, this.or(that))
    }

    pub fn or_scalar(self, other: Number) -> TCResult<Array<u8>> {
        block_dispatch!(
            self,
            this,
            this.or_scalar(other.cast_into())
                .map(Array::from)
                .map_err(TCError::from)
        )
    }

    pub fn xor(self, other: Self) -> TCResult<Array<u8>> {
        block_cmp!(self, other, this, that, this.xor(that))
    }

    pub fn xor_scalar(self, other: Number) -> TCResult<Array<u8>> {
        block_dispatch!(
            self,
            this,
            this.xor_scalar(other.cast_into())
                .map(Array::from)
                .map_err(TCError::from)
        )
    }

    pub fn eq(self, other: Self) -> TCResult<Array<u8>> {
        block_cmp!(self, other, this, that, this.eq(that))
    }

    pub fn eq_scalar(self, other: Number) -> TCResult<Array<u8>> {
        block_dispatch!(
            self,
            this,
            this.eq_scalar(other.cast_into())
                .map(Array::from)
                .map_err(TCError::from)
        )
    }

    pub fn gt(self, other: Self) -> TCResult<Array<u8>> {
        block_cmp!(self, other, this, that, this.gt(that))
    }

    pub fn gt_scalar(self, other: Number) -> TCResult<Array<u8>> {
        block_dispatch!(
            self,
            this,
            this.gt_scalar(other.cast_into())
                .map(Array::from)
                .map_err(TCError::from)
        )
    }

    pub fn ge(self, other: Self) -> TCResult<Array<u8>> {
        block_cmp!(self, other, this, that, this.ge(that))
    }

    pub fn ge_scalar(self, other: Number) -> TCResult<Array<u8>> {
        block_dispatch!(
            self,
            this,
            this.ge_scalar(other.cast_into())
                .map(Array::from)
                .map_err(TCError::from)
        )
    }

    pub fn lt(self, other: Self) -> TCResult<Array<u8>> {
        block_cmp!(self, other, this, that, this.lt(that))
    }

    pub fn lt_scalar(self, other: Number) -> TCResult<Array<u8>> {
        block_dispatch!(
            self,
            this,
            this.lt_scalar(other.cast_into())
                .map(Array::from)
                .map_err(TCError::from)
        )
    }

    pub fn le(self, other: Self) -> TCResult<Array<u8>> {
        block_cmp!(self, other, this, that, this.le(that))
    }

    pub fn le_scalar(self, other: Number) -> TCResult<Array<u8>> {
        block_dispatch!(
            self,
            this,
            this.le_scalar(other.cast_into())
                .map(Array::from)
                .map_err(TCError::from)
        )
    }

    pub fn ne(self, other: Self) -> TCResult<Array<u8>> {
        block_cmp!(self, other, this, that, this.ne(that))
    }

    pub fn ne_scalar(self, other: Number) -> TCResult<Array<u8>> {
        block_dispatch!(
            self,
            this,
            this.ne_scalar(other.cast_into())
                .map(Array::from)
                .map_err(TCError::from)
        )
    }
}

macro_rules! block_from {
    ($t:ty, $var:ident) => {
        impl From<Array<$t>> for Block {
            fn from(array: Array<$t>) -> Self {
                Self::$var(array)
            }
        }
    };
}

block_from!(f32, F32);
block_from!(f64, F64);
block_from!(i16, I16);
block_from!(i32, I32);
block_from!(i64, I64);
block_from!(u8, U8);
block_from!(u16, U16);
block_from!(u32, U32);
block_from!(u64, U64);

impl fmt::Debug for Block {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::F32(this) => this.fmt(f),
            Self::F64(this) => this.fmt(f),
            Self::I16(this) => this.fmt(f),
            Self::I32(this) => this.fmt(f),
            Self::I64(this) => this.fmt(f),
            Self::U8(this) => this.fmt(f),
            Self::U16(this) => this.fmt(f),
            Self::U32(this) => this.fmt(f),
            Self::U64(this) => this.fmt(f),
        }
    }
}
