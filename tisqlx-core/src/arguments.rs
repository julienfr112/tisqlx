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
            Ok(IsNull::No) => self.nulls.push(false),
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
