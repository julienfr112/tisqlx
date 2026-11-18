//! Text and binary result-row decoding.

use std::ops::Range;

use super::ColumnDefinition;
use super::buf::Reader;
use crate::error::{Error, Result};
use crate::type_info::ColumnType;

pub(crate) type Ranges = Vec<Option<Range<usize>>>;

pub(crate) fn decode_text_row(payload: &[u8], ncols: usize) -> Result<Ranges> {
    let mut r = Reader::new(payload);
    let mut values = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        match r.lenenc_int_opt()? {
            None => values.push(None),
            Some(len) => {
                let start = r.pos;
                r.skip(len as usize)?;
                values.push(Some(start..r.pos));
            }
        }
    }
    Ok(values)
}

pub(crate) fn decode_binary_row(payload: &[u8], columns: &[ColumnDefinition]) -> Result<Ranges> {
    let mut r = Reader::new(payload);
    let header = r.u8()?;
    if header != 0x00 {
        return Err(Error::protocol(format!("expected binary row header 0x00, got 0x{header:02x}")));
    }
    // The binary-row NULL bitmap is offset by 2 bits.
    let bitmap = r.bytes((columns.len() + 9) / 8)?;
    let mut values = Vec::with_capacity(columns.len());
    for (i, col) in columns.iter().enumerate() {
        let bit = i + 2;
        if bitmap[bit / 8] & (1 << (bit % 8)) != 0 {
            values.push(None);
            continue;
        }
        let size = match col.type_info.r#type {
            ColumnType::Tiny => 1,
            ColumnType::Short | ColumnType::Year => 2,
            ColumnType::Long | ColumnType::Int24 | ColumnType::Float => 4,
            ColumnType::LongLong | ColumnType::Double => 8,
            ColumnType::Date | ColumnType::Datetime | ColumnType::Timestamp | ColumnType::Time => {
                // Keep the length byte so decoders can tell the variant apart.
                1 + r.peek_u8()? as usize
            }
            ColumnType::Null => 0,
            _ => {
                let len = r.lenenc_int()? as usize;
                let start = r.pos;
                r.skip(len)?;
                values.push(Some(start..r.pos));
                continue;
            }
        };
        let start = r.pos;
        r.skip(size)?;
        values.push(Some(start..r.pos));
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::type_info::{ColumnFlags, TypeInfo};

    fn col(t: ColumnType) -> ColumnDefinition {
        ColumnDefinition {
            schema: String::new(),
            table: String::new(),
            org_table: String::new(),
            name: "c".into(),
            org_name: "c".into(),
            type_info: TypeInfo { r#type: t, flags: ColumnFlags(0), charset: 63, max_size: 0 },
            decimals: 0,
        }
    }

    #[test]
    fn binary_row_with_null() {
        let cols = [col(ColumnType::Long), col(ColumnType::VarString)];
        // first column NULL => bit 2 set
        let rows = decode_binary_row(&[0x00, 0x04, 2, b'h', b'i'], &cols).unwrap();
        assert_eq!(rows, vec![None, Some(3..5)]);
    }

    #[test]
    fn text_row() {
        let rows = decode_text_row(&[1, b'1', 0xFB, 2, b'a', b'b'], 3).unwrap();
        assert_eq!(rows, vec![Some(1..2), None, Some(4..6)]);
    }
}
