//! Fixed size parameters list with optional subparameters.
// Flotilla modification: validated, canonical incremental parameter checkpoints.

use core::fmt::{self, Debug, Formatter};

pub(crate) const MAX_PARAMS: usize = 32;

#[derive(Default)]
pub struct Params {
    /// Number of subparameters for each parameter.
    ///
    /// For each entry in the `params` slice, this stores the length of the
    /// param as number of subparams at the same index as the param in the
    /// `params` slice.
    ///
    /// At the subparam positions the length will always be `0`.
    subparams: [u8; MAX_PARAMS],

    /// All parameters and subparameters.
    params: [u16; MAX_PARAMS],

    /// Number of suparameters in the current parameter.
    current_subparams: u8,

    /// Total number of parameters and subparameters.
    len: usize,
}

#[cfg(feature = "serde")]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ParamsSnapshot {
    subparams: [u8; MAX_PARAMS],
    params: [u16; MAX_PARAMS],
    current_subparams: u8,
    len: usize,
}

impl Params {
    #[cfg(feature = "serde")]
    pub(crate) fn snapshot(&self) -> ParamsSnapshot {
        let mut subparams = [0; MAX_PARAMS];
        let mut params = [0; MAX_PARAMS];
        params[..self.len].copy_from_slice(&self.params[..self.len]);
        let mut index = 0;
        while index < self.len {
            let width = self.subparams[index];
            subparams[index] = width;
            index += width as usize;
        }
        ParamsSnapshot { subparams, params, current_subparams: self.current_subparams, len: self.len }
    }

    #[cfg(feature = "serde")]
    pub(crate) fn from_snapshot(snapshot: ParamsSnapshot) -> Result<Self, alloc::string::String> {
        let invalid = || alloc::string::String::from("invalid parameter checkpoint");
        let ParamsSnapshot { subparams, params, current_subparams, len } = snapshot;
        if len > MAX_PARAMS || current_subparams as usize > len {
            return Err(invalid());
        }
        let pending_start = len - current_subparams as usize;
        let mut index = 0;
        while index < len {
            let width = subparams[index] as usize;
            if width == 0 || width > len - index {
                return Err(invalid());
            }
            if current_subparams != 0
                && ((index < pending_start && index + width > pending_start)
                    || (index == pending_start && width != current_subparams as usize))
            {
                return Err(invalid());
            }
            if subparams[index + 1..index + width].iter().any(|&width| width != 0) {
                return Err(invalid());
            }
            index += width;
        }
        if subparams[len..].iter().any(|&width| width != 0)
            || params[len..].iter().any(|&param| param != 0)
        {
            return Err(invalid());
        }
        Ok(Self { subparams, params, current_subparams, len })
    }

    /// Returns the number of parameters.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if there are no parameters present.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns an iterator over all parameters and subparameters.
    #[inline]
    pub fn iter(&self) -> ParamsIter<'_> {
        ParamsIter::new(self)
    }

    /// Returns `true` if there is no more space for additional parameters.
    #[inline]
    pub(crate) fn is_full(&self) -> bool {
        self.len == MAX_PARAMS
    }

    /// Clear all parameters.
    #[inline]
    pub(crate) fn clear(&mut self) {
        self.current_subparams = 0;
        self.len = 0;
    }

    /// Add an additional parameter.
    #[inline]
    pub(crate) fn push(&mut self, item: u16) {
        self.subparams[self.len - self.current_subparams as usize] = self.current_subparams + 1;
        self.params[self.len] = item;
        self.current_subparams = 0;
        self.len += 1;
    }

    /// Add an additional subparameter to the current parameter.
    #[inline]
    pub(crate) fn extend(&mut self, item: u16) {
        self.subparams[self.len - self.current_subparams as usize] = self.current_subparams + 1;
        self.params[self.len] = item;
        self.current_subparams += 1;
        self.len += 1;
    }
}

impl<'a> IntoIterator for &'a Params {
    type IntoIter = ParamsIter<'a>;
    type Item = &'a [u16];

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Immutable subparameter iterator.
pub struct ParamsIter<'a> {
    params: &'a Params,
    index: usize,
}

impl<'a> ParamsIter<'a> {
    fn new(params: &'a Params) -> Self {
        Self { params, index: 0 }
    }
}

impl<'a> Iterator for ParamsIter<'a> {
    type Item = &'a [u16];

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.params.len() {
            return None;
        }

        // Get all subparameters for the current parameter.
        let num_subparams = self.params.subparams[self.index];
        let param = &self.params.params[self.index..self.index + num_subparams as usize];

        // Jump to the next parameter.
        self.index += num_subparams as usize;

        Some(param)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.params.len() - self.index;
        (remaining, Some(remaining))
    }
}

impl Debug for Params {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "[")?;

        for (i, param) in self.iter().enumerate() {
            if i != 0 {
                write!(f, ";")?;
            }

            for (i, subparam) in param.iter().enumerate() {
                if i != 0 {
                    write!(f, ":")?;
                }

                subparam.fmt(f)?;
            }
        }

        write!(f, "]")
    }
}

#[cfg(all(test, feature = "serde"))]
mod checkpoint_tests {
    use super::*;

    #[test]
    fn rejects_malformed_parameter_groups() {
        let mut params = Params::default();
        params.push(1);
        let mut snapshot = params.snapshot();
        snapshot.len = MAX_PARAMS + 1;
        assert!(Params::from_snapshot(snapshot).is_err());

        let mut snapshot = params.snapshot();
        snapshot.subparams[0] = 0;
        assert!(Params::from_snapshot(snapshot).is_err());

        let mut snapshot = params.snapshot();
        snapshot.subparams[0] = MAX_PARAMS as u8;
        assert!(Params::from_snapshot(snapshot).is_err());

        let mut snapshot = params.snapshot();
        snapshot.current_subparams = 2;
        assert!(Params::from_snapshot(snapshot).is_err());
    }

    #[test]
    fn restores_a_full_pending_subparameter_group() {
        let mut params = Params::default();
        for value in 0..MAX_PARAMS {
            params.extend(value as u16);
        }
        let snapshot = params.snapshot();
        let restored = Params::from_snapshot(snapshot.clone()).unwrap();
        assert_eq!(snapshot, restored.snapshot());
        assert_eq!(params.iter().collect::<alloc::vec::Vec<_>>(),
            restored.iter().collect::<alloc::vec::Vec<_>>());
    }
}
