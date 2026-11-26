//! Result rows.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use crate::decode::Decode;
use crate::error::{Error, Result};
use crate::protocol::row::Ranges;
use crate::type_info::TypeInfo;
use crate::types::Type;
use crate::tz::SessionZone;

pub use crate::protocol::ColumnDefinition as Column;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowFormat {
    Text,
    Binary,
}

#[derive(Debug)]
pub(crate) struct ResultColumns {
    pub(crate) columns: Vec<Column>,
    names: HashMap<String, usize>,
}

impl ResultColumns {
    pub(crate) fn new(columns: Vec<Column>) -> Self {
        let mut names = HashMap::with_capacity(columns.len());
        for (i, c) in columns.iter().enumerate() {
            names.entry(c.name.clone()).or_insert(i);
        }
        Self { columns, names }
    }
}

/// A borrowed raw value from a row.
#[derive(Clone, Copy)]
pub struct ValueRef<'r> {
    pub(crate) bytes: Option<&'r [u8]>,
    pub(crate) format: RowFormat,
    pub(crate) type_info: &'r TypeInfo,
    pub(crate) zone: &'r SessionZone,
}

impl<'r> ValueRef<'r> {
    pub fn is_null(&self) -> bool {
        self.bytes.is_none()
    }

    pub fn format(&self) -> RowFormat {
        self.format
    }

    pub fn type_info(&self) -> &'r TypeInfo {
        self.type_info
    }

    /// Raw bytes; errors on NULL.
    pub fn as_bytes(&self) -> Result<&'r [u8], crate::error::BoxDynError> {
        self.bytes.ok_or_else(|| UnexpectedNull.into())
    }

    pub fn as_str(&self) -> Result<&'r str, crate::error::BoxDynError> {
        Ok(std::str::from_utf8(self.as_bytes()?)?)
    }
}

impl fmt::Debug for ValueRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValueRef").field("type", &self.type_info.name()).field("bytes", &self.bytes).finish()
    }
}

/// Error returned when decoding NULL into a non-`Option` type.
#[derive(Debug, thiserror::Error)]
#[error("unexpected NULL; try decoding as an `Option`")]
pub struct UnexpectedNull;

/// A row returned by a query.
pub struct Row {
    data: Vec<u8>,
    values: Ranges,
    format: RowFormat,
    columns: Arc<ResultColumns>,
    zone: SessionZone,
}

impl Row {
    pub(crate) fn new(
        data: Vec<u8>,
        values: Ranges,
        format: RowFormat,
        columns: Arc<ResultColumns>,
        zone: SessionZone,
    ) -> Self {
        Self { data, values, format, columns, zone }
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns.columns
    }

    pub fn column(&self, index: impl ColumnIndex) -> &Column {
        &self.columns.columns[index.index(self).expect("column not found")]
    }

    pub fn format(&self) -> RowFormat {
        self.format
    }

    pub fn try_get_raw<I: ColumnIndex>(&self, index: I) -> Result<ValueRef<'_>> {
        Ok(self.raw_at(index.index(self)?))
    }

    fn raw_at(&self, i: usize) -> ValueRef<'_> {
        ValueRef {
            bytes: self.values[i].clone().map(|r| &self.data[r]),
            format: self.format,
            type_info: &self.columns.columns[i].type_info,
            zone: &self.zone,
        }
    }

    /// Decode a column, checking its type compatibility first.
    pub fn try_get<'r, T, I>(&'r self, index: I) -> Result<T>
    where
        T: Decode<'r> + Type,
        I: ColumnIndex,
    {
        // The label is only built on error: this runs for every column of every row.
        let value = self.raw_at(index.index(self)?);
        if !value.is_null() && !T::compatible(value.type_info) {
            return Err(Error::ColumnDecode {
                index: index.label(),
                source: format!(
                    "mismatched types; Rust type `{}` is not compatible with SQL type `{}`",
                    std::any::type_name::<T>(),
                    value.type_info.name()
                )
                .into(),
            });
        }
        T::decode(value).map_err(|source| Error::ColumnDecode { index: index.label(), source })
    }

    /// Decode a column without the type-compatibility check.
    pub fn try_get_unchecked<'r, T, I>(&'r self, index: I) -> Result<T>
    where
        T: Decode<'r>,
        I: ColumnIndex,
    {
        let value = self.raw_at(index.index(self)?);
        T::decode(value).map_err(|source| Error::ColumnDecode { index: index.label(), source })
    }

    /// Like [`try_get`](Self::try_get) but panics on error.
    #[track_caller]
    pub fn get<'r, T, I>(&'r self, index: I) -> T
    where
        T: Decode<'r> + Type,
        I: ColumnIndex,
    {
        self.try_get(index).unwrap_or_else(|e| panic!("{e}"))
    }
}

impl fmt::Debug for Row {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut m = f.debug_map();
        for (i, c) in self.columns.columns.iter().enumerate() {
            let v = self.values[i].clone().map(|r| String::from_utf8_lossy(&self.data[r]).into_owned());
            m.entry(&c.name, &v);
        }
        m.finish()
    }
}

/// Something that can index into a row: a `usize` or a column name.
pub trait ColumnIndex: Copy {
    fn index(&self, row: &Row) -> Result<usize>;
    fn label(&self) -> String;
}

impl ColumnIndex for usize {
    fn index(&self, row: &Row) -> Result<usize> {
        if *self >= row.len() {
            return Err(Error::ColumnIndexOutOfBounds { index: *self, len: row.len() });
        }
        Ok(*self)
    }

    fn label(&self) -> String {
        self.to_string()
    }
}

impl ColumnIndex for &str {
    fn index(&self, row: &Row) -> Result<usize> {
        row.columns.names.get(*self).copied().ok_or_else(|| Error::ColumnNotFound((*self).to_owned()))
    }

    fn label(&self) -> String {
        format!("{self:?}")
    }
}
