//! Extension's to Smithay's geometry module.

use std::cmp;

use smithay::utils::{Point, Size};

pub trait CatacombVector: Sized {
    /// Scale the size by a scaling factor.
    fn scale(&self, scale: f64) -> Self;

    /// Compare and return the smaller of each individual dimensions.
    fn min(&self, other: impl Into<Self>) -> Self;

    /// Compare and return the bigger of each individual dimensions.
    fn max(&self, other: impl Into<Self>) -> Self;
}

/// Helper trait for converting into a 2D vector.
pub trait IntoVector {
    fn as_vector(&self) -> (i32, i32);
}

impl<K> IntoVector for Point<i32, K> {
    fn as_vector(&self) -> (i32, i32) {
        (self.x, self.y)
    }
}

impl<K> IntoVector for Size<i32, K> {
    fn as_vector(&self) -> (i32, i32) {
        (self.w, self.h)
    }
}

impl<T> CatacombVector for T
where
    T: IntoVector,
    T: From<(i32, i32)>,
{
    fn scale(&self, scale: f64) -> Self {
        let tuple = self.as_vector();
        Self::from((
            (tuple.0 as f64 * scale).round() as i32,
            (tuple.1 as f64 * scale).round() as i32,
        ))
    }

    fn min(&self, other: impl Into<Self>) -> Self {
        let tuple = self.as_vector();
        let other = other.into().as_vector();
        Self::from((cmp::min(tuple.0, other.0), cmp::min(tuple.1, other.1)))
    }

    fn max(&self, other: impl Into<Self>) -> Self {
        let tuple = self.as_vector();
        let other = other.into().as_vector();
        Self::from((cmp::max(tuple.0, other.0), cmp::max(tuple.1, other.1)))
    }
}
