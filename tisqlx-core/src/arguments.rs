//! Bind-parameter buffers.

use crate::encode::{Encode, IsNull};
use crate::error::Error;
use crate::type_info::TypeInfo;

/// Encoded bind parameters for one statement execution.
#[derive(Debug, Default)]
pub struct Arguments {
    pub(crate) types: Vec<TypeInfo>,
    pub(crate) values: Vec<u8>,
    nulls: Vec<bool>,
    /// Offsets in `values` of UTC instants, converted to the session zone at execute time.
    pub(crate) instants: Vec<usize>,
    pub(crate) error: Option<Error>,
}

impl Arguments {
    pub fn new() -> Self {
        Self::default()
    }

    /// Encode and append a value. Encoding errors are reported when the query runs.
    pub fn add<T: Encode>(&mut self, value: T) {
        let ty = value.produces();
        let start = self.values.len();
        match value.encode(&mut self.values) {
            Ok(IsNull::No) => {
                if crate::types::temporal::is_instant_param(&ty) {
                    self.instants.push(start);
                }
                self.nulls.push(false);
            }
            Ok(IsNull::Yes) => {
                self.values.truncate(start);
                self.nulls.push(true);
            }
            Err(e) => {
                self.values.truncate(start);
                self.nulls.push(true);
                self.error.get_or_insert(Error::Encode(e));
            }
        }
        self.types.push(ty);
    }

    pub fn len(&self) -> usize {
        self.types.len()
    }

    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }

    /// Append `other`'s values after these (e.g. one row of a multi-row INSERT).
    pub(crate) fn append(&mut self, other: &Arguments) {
        let shift = self.values.len();
        self.instants.extend(other.instants.iter().map(|at| at + shift));
        self.types.extend_from_slice(&other.types);
        self.nulls.extend_from_slice(&other.nulls);
        self.values.extend_from_slice(&other.values);
    }

    /// Size of these values in a COM_STMT_EXECUTE packet (NULL bitmap, types, values).
    pub(crate) fn wire_len(&self) -> usize {
        self.types.len().div_ceil(8) + 1 + 2 * self.types.len() + self.values.len()
    }

    pub(crate) fn null_bitmap(&self) -> Vec<u8> {
        let mut bitmap = vec![0u8; self.nulls.len().div_ceil(8)];
        for (i, &null) in self.nulls.iter().enumerate() {
            if null {
                bitmap[i / 8] |= 1 << (i % 8);
            }
        }
        bitmap
    }
}
