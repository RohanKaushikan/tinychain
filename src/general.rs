use std::collections::HashMap;
use std::fmt;
use std::iter::FromIterator;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;

use futures::Future;
use futures::Stream;

use crate::error;
use crate::scalar::Id;

pub type TCBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a + Send>>;
pub type TCBoxTryFuture<'a, T> = TCBoxFuture<'a, TCResult<T>>;
pub type TCResult<T> = Result<T, error::TCError>;
pub type TCStream<T> = Pin<Box<dyn Stream<Item = T> + Send + Unpin>>;
pub type TCTryStream<T> = TCStream<TCResult<T>>;

pub trait CastFrom<T> {
    fn cast_from(value: T) -> Self;
}

pub trait CastInto<T> {
    fn cast_into(self) -> T;
}

impl<F, T: From<F>> CastFrom<F> for T {
    fn cast_from(f: F) -> T {
        T::from(f)
    }
}

impl<T, F: CastFrom<T>> CastInto<F> for T {
    fn cast_into(self) -> F {
        F::cast_from(self)
    }
}

pub trait TryCastFrom<T>: Sized {
    fn can_cast_from(value: &T) -> bool;

    fn opt_cast_from(value: T) -> Option<Self>;

    fn try_cast_from<Err, E: FnOnce(&T) -> Err>(value: T, on_err: E) -> Result<Self, Err> {
        if Self::can_cast_from(&value) {
            Ok(Self::opt_cast_from(value).unwrap())
        } else {
            Err(on_err(&value))
        }
    }
}

pub trait TryCastInto<T>: Sized {
    fn can_cast_into(&self) -> bool;

    fn opt_cast_into(self) -> Option<T>;

    fn try_cast_into<Err, E: FnOnce(&Self) -> Err>(self, on_err: E) -> Result<T, Err> {
        if self.can_cast_into() {
            Ok(self.opt_cast_into().unwrap())
        } else {
            Err(on_err(&self))
        }
    }
}

impl<F, T: CastFrom<F>> TryCastFrom<F> for T {
    fn can_cast_from(_: &F) -> bool {
        true
    }

    fn opt_cast_from(f: F) -> Option<T> {
        Some(T::cast_from(f))
    }
}

impl<F, T: TryCastFrom<F>> TryCastInto<T> for F {
    fn can_cast_into(&self) -> bool {
        T::can_cast_from(self)
    }

    fn opt_cast_into(self) -> Option<T> {
        T::opt_cast_from(self)
    }
}

#[derive(Clone)]
pub struct Map<T: Clone> {
    inner: HashMap<Id, T>,
}

impl<T: Clone> Map<T> {
    pub fn into_inner(self) -> HashMap<Id, T> {
        self.inner
    }
}

impl<T: Clone> Default for Map<T> {
    fn default() -> Map<T> {
        HashMap::new().into()
    }
}

impl<T: Clone + PartialEq> PartialEq for Map<T> {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl<T: Clone + PartialEq + Eq> Eq for Map<T> {}

impl<T: Clone> AsRef<HashMap<Id, T>> for Map<T> {
    fn as_ref(&self) -> &HashMap<Id, T> {
        &self.inner
    }
}

impl<T: Clone> Deref for Map<T> {
    type Target = HashMap<Id, T>;

    fn deref(&'_ self) -> &'_ Self::Target {
        &self.inner
    }
}

impl<T: Clone> DerefMut for Map<T> {
    fn deref_mut(&'_ mut self) -> &'_ mut <Self as Deref>::Target {
        &mut self.inner
    }
}

impl<T: Clone> FromIterator<(Id, T)> for Map<T> {
    fn from_iter<I: IntoIterator<Item = (Id, T)>>(iter: I) -> Self {
        let inner = HashMap::from_iter(iter);
        Map { inner }
    }
}

impl<T: Clone> From<HashMap<Id, T>> for Map<T> {
    fn from(inner: HashMap<Id, T>) -> Self {
        Map { inner }
    }
}

impl<T: Clone + fmt::Display> fmt::Display for Map<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{{{}}}",
            self.inner
                .iter()
                .map(|(k, v)| format!("{}: {}", k, v))
                .collect::<Vec<String>>()
                .join(", ")
        )
    }
}

#[derive(Clone, Default, Eq, PartialEq)]
pub struct Tuple<T: Clone> {
    inner: Vec<T>,
}

impl<T: Clone> Tuple<T> {
    pub fn into_inner(self) -> Vec<T> {
        self.inner
    }
}

impl<T: Clone> AsRef<Vec<T>> for Tuple<T> {
    fn as_ref(&self) -> &Vec<T> {
        &self.inner
    }
}

impl<T: Clone> Deref for Tuple<T> {
    type Target = Vec<T>;

    fn deref(&'_ self) -> &'_ Self::Target {
        &self.inner
    }
}

impl<T: Clone> DerefMut for Tuple<T> {
    fn deref_mut(&'_ mut self) -> &'_ mut <Self as Deref>::Target {
        &mut self.inner
    }
}

impl<T: Clone> FromIterator<T> for Tuple<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let inner = Vec::from_iter(iter);
        Tuple { inner }
    }
}

impl<T: Clone> From<Vec<T>> for Tuple<T> {
    fn from(inner: Vec<T>) -> Self {
        Tuple { inner }
    }
}

impl<F: Clone, T: TryCastFrom<F>> TryCastFrom<Tuple<F>> for Vec<T> {
    fn can_cast_from(tuple: &Tuple<F>) -> bool {
        tuple.iter().all(T::can_cast_from)
    }

    fn opt_cast_from(tuple: Tuple<F>) -> Option<Self> {
        let mut cast: Vec<T> = Vec::with_capacity(tuple.len());
        for val in tuple.inner.into_iter() {
            if let Some(val) = val.opt_cast_into() {
                cast.push(val)
            } else {
                return None;
            }
        }

        Some(cast)
    }
}

impl<F: Clone, T: TryCastFrom<F>> TryCastFrom<Tuple<F>> for (T,) {
    fn can_cast_from(source: &Tuple<F>) -> bool {
        source.len() == 1 && T::can_cast_from(&source[0])
    }

    fn opt_cast_from(mut source: Tuple<F>) -> Option<(T,)> {
        if source.len() == 1 {
            source.pop().unwrap().opt_cast_into().map(|item| (item,))
        } else {
            None
        }
    }
}

impl<F: Clone, T1: TryCastFrom<F>, T2: TryCastFrom<F>> TryCastFrom<Tuple<F>> for (T1, T2) {
    fn can_cast_from(source: &Tuple<F>) -> bool {
        source.len() == 2 && T1::can_cast_from(&source[0]) && T2::can_cast_from(&source[1])
    }

    fn opt_cast_from(mut source: Tuple<F>) -> Option<(T1, T2)> {
        if source.len() == 2 {
            let second: Option<T2> = source.pop().unwrap().opt_cast_into();
            let first: Option<T1> = source.pop().unwrap().opt_cast_into();
            match (first, second) {
                (Some(first), Some(second)) => Some((first, second)),
                _ => None,
            }
        } else {
            None
        }
    }
}

impl<F: Clone, T1: TryCastFrom<F>, T2: TryCastFrom<F>, T3: TryCastFrom<F>> TryCastFrom<Tuple<F>>
    for (T1, T2, T3)
{
    fn can_cast_from(source: &Tuple<F>) -> bool {
        source.len() == 3
            && T1::can_cast_from(&source[0])
            && T2::can_cast_from(&source[1])
            && T3::can_cast_from(&source[2])
    }

    fn opt_cast_from(mut source: Tuple<F>) -> Option<(T1, T2, T3)> {
        if source.len() == 3 {
            let third: Option<T3> = source.pop().unwrap().opt_cast_into();
            let second: Option<T2> = source.pop().unwrap().opt_cast_into();
            let first: Option<T1> = source.pop().unwrap().opt_cast_into();
            match (first, second, third) {
                (Some(first), Some(second), Some(third)) => Some((first, second, third)),
                _ => None,
            }
        } else {
            None
        }
    }
}

impl<F: Clone, T1: TryCastFrom<F>, T2: TryCastFrom<F>, T3: TryCastFrom<F>, T4: TryCastFrom<F>>
    TryCastFrom<Tuple<F>> for (T1, T2, T3, T4)
{
    fn can_cast_from(source: &Tuple<F>) -> bool {
        source.len() == 4
            && T1::can_cast_from(&source[0])
            && T2::can_cast_from(&source[1])
            && T3::can_cast_from(&source[2])
            && T4::can_cast_from(&source[3])
    }

    fn opt_cast_from(mut source: Tuple<F>) -> Option<(T1, T2, T3, T4)> {
        if source.len() == 4 {
            let fourth: Option<T4> = source.pop().unwrap().opt_cast_into();
            let third: Option<T3> = source.pop().unwrap().opt_cast_into();
            let second: Option<T2> = source.pop().unwrap().opt_cast_into();
            let first: Option<T1> = source.pop().unwrap().opt_cast_into();
            match (first, second, third, fourth) {
                (Some(first), Some(second), Some(third), Some(fourth)) => {
                    Some((first, second, third, fourth))
                }
                _ => None,
            }
        } else {
            None
        }
    }
}

impl<T: Clone + fmt::Display> fmt::Display for Tuple<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "({})",
            self.inner
                .iter()
                .map(|item| item.to_string())
                .collect::<Vec<String>>()
                .join(", ")
        )
    }
}
