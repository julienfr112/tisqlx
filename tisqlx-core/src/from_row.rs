//! Mapping rows to Rust types.

use crate::decode::Decode;
use crate::error::Result;
use crate::row::Row;
use crate::types::Type;

/// Build a value from a row. Derive it with `#[derive(tisqlx::FromRow)]`.
pub trait FromRow<'r>: Sized {
    fn from_row(row: &'r Row) -> Result<Self>;
}

macro_rules! impl_from_row_for_tuple {
    ($( ($idx:tt) -> $T:ident );+;) => {
        impl<'r, $($T,)+> FromRow<'r> for ($($T,)+)
        where
            $($T: Decode<'r> + Type,)+
        {
            #[inline]
            fn from_row(row: &'r Row) -> Result<Self> {
                Ok(($(row.try_get($idx as usize)?,)+))
            }
        }
    };
}

impl_from_row_for_tuple!((0) -> T1;);
impl_from_row_for_tuple!((0) -> T1; (1) -> T2;);
impl_from_row_for_tuple!((0) -> T1; (1) -> T2; (2) -> T3;);
impl_from_row_for_tuple!((0) -> T1; (1) -> T2; (2) -> T3; (3) -> T4;);
impl_from_row_for_tuple!((0) -> T1; (1) -> T2; (2) -> T3; (3) -> T4; (4) -> T5;);
impl_from_row_for_tuple!((0) -> T1; (1) -> T2; (2) -> T3; (3) -> T4; (4) -> T5; (5) -> T6;);
impl_from_row_for_tuple!((0) -> T1; (1) -> T2; (2) -> T3; (3) -> T4; (4) -> T5; (5) -> T6; (6) -> T7;);
impl_from_row_for_tuple!((0) -> T1; (1) -> T2; (2) -> T3; (3) -> T4; (4) -> T5; (5) -> T6; (6) -> T7; (7) -> T8;);
impl_from_row_for_tuple!((0) -> T1; (1) -> T2; (2) -> T3; (3) -> T4; (4) -> T5; (5) -> T6; (6) -> T7; (7) -> T8; (8) -> T9;);
impl_from_row_for_tuple!((0) -> T1; (1) -> T2; (2) -> T3; (3) -> T4; (4) -> T5; (5) -> T6; (6) -> T7; (7) -> T8; (8) -> T9; (9) -> T10;);
impl_from_row_for_tuple!((0) -> T1; (1) -> T2; (2) -> T3; (3) -> T4; (4) -> T5; (5) -> T6; (6) -> T7; (7) -> T8; (8) -> T9; (9) -> T10; (10) -> T11;);
impl_from_row_for_tuple!((0) -> T1; (1) -> T2; (2) -> T3; (3) -> T4; (4) -> T5; (5) -> T6; (6) -> T7; (7) -> T8; (8) -> T9; (9) -> T10; (10) -> T11; (11) -> T12;);
