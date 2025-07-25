// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.
//
// Portions of this file are derived from the PostgreSQL project. The original
// source code is subject to the terms of the PostgreSQL license, a copy of
// which can be found in the LICENSE file at the root of this repository.

use std::borrow::Cow;
use std::cmp::{self, Ordering};
use std::convert::{TryFrom, TryInto};
use std::ops::Deref;
use std::str::FromStr;
use std::{fmt, iter, str};

use ::encoding::DecoderTrap;
use ::encoding::label::encoding_from_whatwg_label;
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc};
use chrono_tz::{OffsetComponents, OffsetName, Tz};
use dec::OrderedDecimal;
use fallible_iterator::FallibleIterator;
use hmac::{Hmac, Mac};
use itertools::Itertools;
use md5::{Digest, Md5};
use mz_expr_derive::sqlfunc;
use mz_lowertest::MzReflect;
use mz_ore::cast::{self, CastFrom, ReinterpretCast};
use mz_ore::fmt::FormatBuffer;
use mz_ore::lex::LexBuf;
use mz_ore::option::OptionExt;
use mz_ore::result::ResultExt;
use mz_ore::soft_assert_eq_or_log;
use mz_ore::str::StrExt;
use mz_pgrepr::Type;
use mz_pgtz::timezone::{Timezone, TimezoneSpec};
use mz_proto::chrono::any_naive_datetime;
use mz_proto::{IntoRustIfSome, ProtoType, RustType, TryFromProtoError};
use mz_repr::adt::array::ArrayDimension;
use mz_repr::adt::date::Date;
use mz_repr::adt::interval::{Interval, RoundBehavior};
use mz_repr::adt::jsonb::JsonbRef;
use mz_repr::adt::mz_acl_item::{AclItem, AclMode, MzAclItem};
use mz_repr::adt::numeric::{self, DecimalLike, Numeric, NumericMaxScale};
use mz_repr::adt::range::{self, Range, RangeBound, RangeOps};
use mz_repr::adt::regex::{Regex, any_regex};
use mz_repr::adt::system::Oid;
use mz_repr::adt::timestamp::{CheckedTimestamp, TimestampLike};
use mz_repr::role_id::RoleId;
use mz_repr::{ColumnName, ColumnType, Datum, DatumType, Row, RowArena, ScalarType, strconv};
use mz_sql_parser::ast::display::FormatMode;
use mz_sql_pretty::{PrettyConfig, pretty_str};
use num::traits::CheckedNeg;
use proptest::prelude::*;
use proptest::strategy::*;
use proptest_derive::Arbitrary;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Sha224, Sha256, Sha384, Sha512};
use subtle::ConstantTimeEq;

use crate::scalar::func::format::DateTimeFormat;
use crate::scalar::{
    ProtoBinaryFunc, ProtoUnaryFunc, ProtoUnmaterializableFunc, ProtoVariadicFunc,
};
use crate::{EvalError, MirScalarExpr, like_pattern};

#[macro_use]
mod macros;
mod binary;
mod encoding;
pub(crate) mod format;
pub(crate) mod impls;

pub use impls::*;

/// The maximum size of a newly allocated string. Chosen to be the smallest number to keep our tests
/// passing without changing. 100MiB is probably higher than what we want, but it's better than no
/// limit.
const MAX_STRING_BYTES: usize = 1024 * 1024 * 100;

#[derive(
    Arbitrary, Ord, PartialOrd, Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash, MzReflect,
)]
pub enum UnmaterializableFunc {
    CurrentDatabase,
    CurrentSchema,
    CurrentSchemasWithSystem,
    CurrentSchemasWithoutSystem,
    CurrentTimestamp,
    CurrentUser,
    IsRbacEnabled,
    MzEnvironmentId,
    MzIsSuperuser,
    MzNow,
    MzRoleOidMemberships,
    MzSessionId,
    MzUptime,
    MzVersion,
    MzVersionNum,
    PgBackendPid,
    PgPostmasterStartTime,
    SessionUser,
    Version,
    ViewableVariables,
}

impl UnmaterializableFunc {
    pub fn output_type(&self) -> ColumnType {
        match self {
            UnmaterializableFunc::CurrentDatabase => ScalarType::String.nullable(false),
            // TODO: The `CurrentSchema` function should return `name`. This is
            // tricky in Materialize because `name` truncates to 63 characters
            // but Materialize does not have a limit on identifier length.
            UnmaterializableFunc::CurrentSchema => ScalarType::String.nullable(true),
            // TODO: The `CurrentSchemas` function should return `name[]`. This
            // is tricky in Materialize because `name` truncates to 63
            // characters but Materialize does not have a limit on identifier
            // length.
            UnmaterializableFunc::CurrentSchemasWithSystem => {
                ScalarType::Array(Box::new(ScalarType::String)).nullable(false)
            }
            UnmaterializableFunc::CurrentSchemasWithoutSystem => {
                ScalarType::Array(Box::new(ScalarType::String)).nullable(false)
            }
            UnmaterializableFunc::CurrentTimestamp => {
                ScalarType::TimestampTz { precision: None }.nullable(false)
            }
            UnmaterializableFunc::CurrentUser => ScalarType::String.nullable(false),
            UnmaterializableFunc::IsRbacEnabled => ScalarType::Bool.nullable(false),
            UnmaterializableFunc::MzEnvironmentId => ScalarType::String.nullable(false),
            UnmaterializableFunc::MzIsSuperuser => ScalarType::Bool.nullable(false),
            UnmaterializableFunc::MzNow => ScalarType::MzTimestamp.nullable(false),
            UnmaterializableFunc::MzRoleOidMemberships => ScalarType::Map {
                value_type: Box::new(ScalarType::Array(Box::new(ScalarType::String))),
                custom_id: None,
            }
            .nullable(false),
            UnmaterializableFunc::MzSessionId => ScalarType::Uuid.nullable(false),
            UnmaterializableFunc::MzUptime => ScalarType::Interval.nullable(true),
            UnmaterializableFunc::MzVersion => ScalarType::String.nullable(false),
            UnmaterializableFunc::MzVersionNum => ScalarType::Int32.nullable(false),
            UnmaterializableFunc::PgBackendPid => ScalarType::Int32.nullable(false),
            UnmaterializableFunc::PgPostmasterStartTime => {
                ScalarType::TimestampTz { precision: None }.nullable(false)
            }
            UnmaterializableFunc::SessionUser => ScalarType::String.nullable(false),
            UnmaterializableFunc::Version => ScalarType::String.nullable(false),
            UnmaterializableFunc::ViewableVariables => ScalarType::Map {
                value_type: Box::new(ScalarType::String),
                custom_id: None,
            }
            .nullable(false),
        }
    }
}

impl fmt::Display for UnmaterializableFunc {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            UnmaterializableFunc::CurrentDatabase => f.write_str("current_database"),
            UnmaterializableFunc::CurrentSchema => f.write_str("current_schema"),
            UnmaterializableFunc::CurrentSchemasWithSystem => f.write_str("current_schemas(true)"),
            UnmaterializableFunc::CurrentSchemasWithoutSystem => {
                f.write_str("current_schemas(false)")
            }
            UnmaterializableFunc::CurrentTimestamp => f.write_str("current_timestamp"),
            UnmaterializableFunc::CurrentUser => f.write_str("current_user"),
            UnmaterializableFunc::IsRbacEnabled => f.write_str("is_rbac_enabled"),
            UnmaterializableFunc::MzEnvironmentId => f.write_str("mz_environment_id"),
            UnmaterializableFunc::MzIsSuperuser => f.write_str("mz_is_superuser"),
            UnmaterializableFunc::MzNow => f.write_str("mz_now"),
            UnmaterializableFunc::MzRoleOidMemberships => f.write_str("mz_role_oid_memberships"),
            UnmaterializableFunc::MzSessionId => f.write_str("mz_session_id"),
            UnmaterializableFunc::MzUptime => f.write_str("mz_uptime"),
            UnmaterializableFunc::MzVersion => f.write_str("mz_version"),
            UnmaterializableFunc::MzVersionNum => f.write_str("mz_version_num"),
            UnmaterializableFunc::PgBackendPid => f.write_str("pg_backend_pid"),
            UnmaterializableFunc::PgPostmasterStartTime => f.write_str("pg_postmaster_start_time"),
            UnmaterializableFunc::SessionUser => f.write_str("session_user"),
            UnmaterializableFunc::Version => f.write_str("version"),
            UnmaterializableFunc::ViewableVariables => f.write_str("viewable_variables"),
        }
    }
}

impl RustType<ProtoUnmaterializableFunc> for UnmaterializableFunc {
    fn into_proto(&self) -> ProtoUnmaterializableFunc {
        use crate::scalar::proto_unmaterializable_func::Kind::*;
        let kind = match self {
            UnmaterializableFunc::CurrentDatabase => CurrentDatabase(()),
            UnmaterializableFunc::CurrentSchema => CurrentSchema(()),
            UnmaterializableFunc::CurrentSchemasWithSystem => CurrentSchemasWithSystem(()),
            UnmaterializableFunc::CurrentSchemasWithoutSystem => CurrentSchemasWithoutSystem(()),
            UnmaterializableFunc::ViewableVariables => CurrentSetting(()),
            UnmaterializableFunc::CurrentTimestamp => CurrentTimestamp(()),
            UnmaterializableFunc::CurrentUser => CurrentUser(()),
            UnmaterializableFunc::IsRbacEnabled => IsRbacEnabled(()),
            UnmaterializableFunc::MzEnvironmentId => MzEnvironmentId(()),
            UnmaterializableFunc::MzIsSuperuser => MzIsSuperuser(()),
            UnmaterializableFunc::MzNow => MzNow(()),
            UnmaterializableFunc::MzRoleOidMemberships => MzRoleOidMemberships(()),
            UnmaterializableFunc::MzSessionId => MzSessionId(()),
            UnmaterializableFunc::MzUptime => MzUptime(()),
            UnmaterializableFunc::MzVersion => MzVersion(()),
            UnmaterializableFunc::MzVersionNum => MzVersionNum(()),
            UnmaterializableFunc::PgBackendPid => PgBackendPid(()),
            UnmaterializableFunc::PgPostmasterStartTime => PgPostmasterStartTime(()),
            UnmaterializableFunc::SessionUser => SessionUser(()),
            UnmaterializableFunc::Version => Version(()),
        };
        ProtoUnmaterializableFunc { kind: Some(kind) }
    }

    fn from_proto(proto: ProtoUnmaterializableFunc) -> Result<Self, TryFromProtoError> {
        use crate::scalar::proto_unmaterializable_func::Kind::*;
        if let Some(kind) = proto.kind {
            match kind {
                CurrentDatabase(()) => Ok(UnmaterializableFunc::CurrentDatabase),
                CurrentSchema(()) => Ok(UnmaterializableFunc::CurrentSchema),
                CurrentSchemasWithSystem(()) => Ok(UnmaterializableFunc::CurrentSchemasWithSystem),
                CurrentSchemasWithoutSystem(()) => {
                    Ok(UnmaterializableFunc::CurrentSchemasWithoutSystem)
                }
                CurrentTimestamp(()) => Ok(UnmaterializableFunc::CurrentTimestamp),
                CurrentSetting(()) => Ok(UnmaterializableFunc::ViewableVariables),
                CurrentUser(()) => Ok(UnmaterializableFunc::CurrentUser),
                IsRbacEnabled(()) => Ok(UnmaterializableFunc::IsRbacEnabled),
                MzEnvironmentId(()) => Ok(UnmaterializableFunc::MzEnvironmentId),
                MzIsSuperuser(()) => Ok(UnmaterializableFunc::MzIsSuperuser),
                MzNow(()) => Ok(UnmaterializableFunc::MzNow),
                MzRoleOidMemberships(()) => Ok(UnmaterializableFunc::MzRoleOidMemberships),
                MzSessionId(()) => Ok(UnmaterializableFunc::MzSessionId),
                MzUptime(()) => Ok(UnmaterializableFunc::MzUptime),
                MzVersion(()) => Ok(UnmaterializableFunc::MzVersion),
                MzVersionNum(()) => Ok(UnmaterializableFunc::MzVersionNum),
                PgBackendPid(()) => Ok(UnmaterializableFunc::PgBackendPid),
                PgPostmasterStartTime(()) => Ok(UnmaterializableFunc::PgPostmasterStartTime),
                SessionUser(()) => Ok(UnmaterializableFunc::SessionUser),
                Version(()) => Ok(UnmaterializableFunc::Version),
            }
        } else {
            Err(TryFromProtoError::missing_field(
                "`ProtoUnmaterializableFunc::kind`",
            ))
        }
    }
}

pub fn and<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
    exprs: &'a [MirScalarExpr],
) -> Result<Datum<'a>, EvalError> {
    // If any is false, then return false. Else, if any is null, then return null. Else, return true.
    let mut null = false;
    let mut err = None;
    for expr in exprs {
        match expr.eval(datums, temp_storage) {
            Ok(Datum::False) => return Ok(Datum::False), // short-circuit
            Ok(Datum::True) => {}
            // No return in these two cases, because we might still see a false
            Ok(Datum::Null) => null = true,
            Err(this_err) => err = std::cmp::max(err.take(), Some(this_err)),
            _ => unreachable!(),
        }
    }
    match (err, null) {
        (Some(err), _) => Err(err),
        (None, true) => Ok(Datum::Null),
        (None, false) => Ok(Datum::True),
    }
}

pub fn or<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
    exprs: &'a [MirScalarExpr],
) -> Result<Datum<'a>, EvalError> {
    // If any is true, then return true. Else, if any is null, then return null. Else, return false.
    let mut null = false;
    let mut err = None;
    for expr in exprs {
        match expr.eval(datums, temp_storage) {
            Ok(Datum::False) => {}
            Ok(Datum::True) => return Ok(Datum::True), // short-circuit
            // No return in these two cases, because we might still see a true
            Ok(Datum::Null) => null = true,
            Err(this_err) => err = std::cmp::max(err.take(), Some(this_err)),
            _ => unreachable!(),
        }
    }
    match (err, null) {
        (Some(err), _) => Err(err),
        (None, true) => Ok(Datum::Null),
        (None, false) => Ok(Datum::False),
    }
}

pub fn jsonb_stringify<'a>(a: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    match a {
        Datum::JsonNull => Datum::Null,
        Datum::String(_) => a,
        _ => {
            let s = cast_jsonb_to_string(JsonbRef::from_datum(a));
            Datum::String(temp_storage.push_string(s))
        }
    }
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "i16",
    is_infix_op = true,
    sqlname = "+",
    propagates_nulls = true
)]
fn add_int16<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_int16()
        .checked_add(b.unwrap_int16())
        .ok_or(EvalError::NumericFieldOverflow)
        .map(Datum::from)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "i32",
    is_infix_op = true,
    sqlname = "+",
    propagates_nulls = true
)]
fn add_int32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_int32()
        .checked_add(b.unwrap_int32())
        .ok_or(EvalError::NumericFieldOverflow)
        .map(Datum::from)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "i64",
    is_infix_op = true,
    sqlname = "+",
    propagates_nulls = true
)]
fn add_int64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_int64()
        .checked_add(b.unwrap_int64())
        .ok_or(EvalError::NumericFieldOverflow)
        .map(Datum::from)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "u16",
    is_infix_op = true,
    sqlname = "+",
    propagates_nulls = true
)]
fn add_uint16<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_uint16()
        .checked_add(b.unwrap_uint16())
        .ok_or_else(|| EvalError::UInt16OutOfRange(format!("{a} + {b}").into()))
        .map(Datum::from)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "u32",
    is_infix_op = true,
    sqlname = "+",
    propagates_nulls = true
)]
fn add_uint32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_uint32()
        .checked_add(b.unwrap_uint32())
        .ok_or_else(|| EvalError::UInt32OutOfRange(format!("{a} + {b}").into()))
        .map(Datum::from)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "u64",
    is_infix_op = true,
    sqlname = "+",
    propagates_nulls = true
)]
fn add_uint64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_uint64()
        .checked_add(b.unwrap_uint64())
        .ok_or_else(|| EvalError::UInt64OutOfRange(format!("{a} + {b}").into()))
        .map(Datum::from)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "f32",
    is_infix_op = true,
    sqlname = "+",
    propagates_nulls = true
)]
fn add_float32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let a = a.unwrap_float32();
    let b = b.unwrap_float32();
    let sum = a + b;
    if sum.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        Err(EvalError::FloatOverflow)
    } else {
        Ok(Datum::from(sum))
    }
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "f64",
    is_infix_op = true,
    sqlname = "+",
    propagates_nulls = true
)]
fn add_float64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let a = a.unwrap_float64();
    let b = b.unwrap_float64();
    let sum = a + b;
    if sum.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        Err(EvalError::FloatOverflow)
    } else {
        Ok(Datum::from(sum))
    }
}

fn add_timestamplike_interval<'a, T>(
    a: CheckedTimestamp<T>,
    b: Interval,
) -> Result<Datum<'a>, EvalError>
where
    T: TimestampLike,
{
    let dt = a.date_time();
    let dt = add_timestamp_months(&dt, b.months)?;
    let dt = dt
        .checked_add_signed(b.duration_as_chrono())
        .ok_or(EvalError::TimestampOutOfRange)?;
    T::from_date_time(dt).try_into().err_into()
}

fn sub_timestamplike_interval<'a, T>(
    a: CheckedTimestamp<T>,
    b: Datum,
) -> Result<Datum<'a>, EvalError>
where
    T: TimestampLike,
{
    neg_interval_inner(b).and_then(|i| add_timestamplike_interval(a, i))
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "CheckedTimestamp<NaiveDateTime>",
    is_infix_op = true,
    sqlname = "+",
    propagates_nulls = true
)]
fn add_date_time<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let date = a.unwrap_date();
    let time = b.unwrap_time();

    let dt = NaiveDate::from(date)
        .and_hms_nano_opt(time.hour(), time.minute(), time.second(), time.nanosecond())
        .unwrap();
    Ok(dt.try_into()?)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "CheckedTimestamp<NaiveDateTime>",
    is_infix_op = true,
    sqlname = "+",
    propagates_nulls = true
)]
fn add_date_interval<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let date = a.unwrap_date();
    let interval = b.unwrap_interval();

    let dt = NaiveDate::from(date).and_hms_opt(0, 0, 0).unwrap();
    let dt = add_timestamp_months(&dt, interval.months)?;
    let dt = dt
        .checked_add_signed(interval.duration_as_chrono())
        .ok_or(EvalError::TimestampOutOfRange)?;
    Ok(dt.try_into()?)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "CheckedTimestamp<chrono::DateTime<Utc>>",
    is_infix_op = true,
    sqlname = "+",
    propagates_nulls = true
)]
fn add_time_interval<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let time = a.unwrap_time();
    let interval = b.unwrap_interval();
    let (t, _) = time.overflowing_add_signed(interval.duration_as_chrono());
    Datum::Time(t)
}

#[sqlfunc(
    is_monotone = "(true, false)",
    output_type = "Numeric",
    sqlname = "round",
    propagates_nulls = true
)]
fn round_numeric_binary<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mut a = a.unwrap_numeric().0;
    let mut b = b.unwrap_int32();
    let mut cx = numeric::cx_datum();
    let a_exp = a.exponent();
    if a_exp > 0 && b > 0 || a_exp < 0 && -a_exp < b {
        // This condition indicates:
        // - a is a value without a decimal point, b is a positive number
        // - a has a decimal point, but b is larger than its scale
        // In both of these situations, right-pad the number with zeroes, which // is most easily done with rescale.

        // Ensure rescale doesn't exceed max precision by putting a ceiling on
        // b equal to the maximum remaining scale the value can support.
        let max_remaining_scale = u32::from(numeric::NUMERIC_DATUM_MAX_PRECISION)
            - (numeric::get_precision(&a) - numeric::get_scale(&a));
        b = match i32::try_from(max_remaining_scale) {
            Ok(max_remaining_scale) => std::cmp::min(b, max_remaining_scale),
            Err(_) => b,
        };
        cx.rescale(&mut a, &numeric::Numeric::from(-b));
    } else {
        // To avoid invalid operations, clamp b to be within 1 more than the
        // precision limit.
        const MAX_P_LIMIT: i32 = 1 + cast::u8_to_i32(numeric::NUMERIC_DATUM_MAX_PRECISION);
        b = std::cmp::min(MAX_P_LIMIT, b);
        b = std::cmp::max(-MAX_P_LIMIT, b);
        let mut b = numeric::Numeric::from(b);
        // Shift by 10^b; this put digit to round to in the one's place.
        cx.scaleb(&mut a, &b);
        cx.round(&mut a);
        // Negate exponent for shift back
        cx.neg(&mut b);
        cx.scaleb(&mut a, &b);
    }

    if cx.status().overflow() {
        Err(EvalError::FloatOverflow)
    } else if a.is_zero() {
        // simpler than handling cases where exponent has gotten set to some
        // value greater than the max precision, but all significant digits
        // were rounded away.
        Ok(Datum::from(numeric::Numeric::zero()))
    } else {
        numeric::munge_numeric(&mut a).unwrap();
        Ok(Datum::from(a))
    }
}

#[sqlfunc(
    output_type = "String",
    sqlname = "convert_from",
    propagates_nulls = true
)]
fn convert_from<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    // Convert PostgreSQL-style encoding names[1] to WHATWG-style encoding names[2],
    // which the encoding library uses[3].
    // [1]: https://www.postgresql.org/docs/9.5/multibyte.html
    // [2]: https://encoding.spec.whatwg.org/
    // [3]: https://github.com/lifthrasiir/rust-encoding/blob/4e79c35ab6a351881a86dbff565c4db0085cc113/src/label.rs
    let encoding_name = b
        .unwrap_str()
        .to_lowercase()
        .replace('_', "-")
        .into_boxed_str();

    // Supporting other encodings is tracked by database-issues#797.
    if encoding_from_whatwg_label(&encoding_name).map(|e| e.name()) != Some("utf-8") {
        return Err(EvalError::InvalidEncodingName(encoding_name));
    }

    match str::from_utf8(a.unwrap_bytes()) {
        Ok(from) => Ok(Datum::String(from)),
        Err(e) => Err(EvalError::InvalidByteSequence {
            byte_sequence: e.to_string().into(),
            encoding_name,
        }),
    }
}

#[sqlfunc(output_type = "String", propagates_nulls = true)]
fn encode<'a>(
    bytes: Datum<'a>,
    format: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let format = encoding::lookup_format(format.unwrap_str())?;
    let out = format.encode(bytes.unwrap_bytes());
    Ok(Datum::from(temp_storage.push_string(out)))
}

#[sqlfunc(output_type = "Vec<u8>", propagates_nulls = true)]
fn decode<'a>(
    string: Datum<'a>,
    format: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let format = encoding::lookup_format(format.unwrap_str())?;
    let out = format.decode(string.unwrap_str())?;
    Ok(Datum::from(temp_storage.push_bytes(out)))
}

#[sqlfunc(output_type = "i32", sqlname = "length", propagates_nulls = true)]
fn encoded_bytes_char_length<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    // Convert PostgreSQL-style encoding names[1] to WHATWG-style encoding names[2],
    // which the encoding library uses[3].
    // [1]: https://www.postgresql.org/docs/9.5/multibyte.html
    // [2]: https://encoding.spec.whatwg.org/
    // [3]: https://github.com/lifthrasiir/rust-encoding/blob/4e79c35ab6a351881a86dbff565c4db0085cc113/src/label.rs
    let encoding_name = b
        .unwrap_str()
        .to_lowercase()
        .replace('_', "-")
        .into_boxed_str();

    let enc = match encoding_from_whatwg_label(&encoding_name) {
        Some(enc) => enc,
        None => return Err(EvalError::InvalidEncodingName(encoding_name)),
    };

    let decoded_string = match enc.decode(a.unwrap_bytes(), DecoderTrap::Strict) {
        Ok(s) => s,
        Err(e) => {
            return Err(EvalError::InvalidByteSequence {
                byte_sequence: e.into(),
                encoding_name,
            });
        }
    };

    let count = decoded_string.chars().count();
    match i32::try_from(count) {
        Ok(l) => Ok(Datum::from(l)),
        Err(_) => Err(EvalError::Int32OutOfRange(count.to_string().into())),
    }
}

// TODO(benesch): remove potentially dangerous usage of `as`.
#[allow(clippy::as_conversions)]
pub fn add_timestamp_months<T: TimestampLike>(
    dt: &T,
    mut months: i32,
) -> Result<CheckedTimestamp<T>, EvalError> {
    if months == 0 {
        return Ok(CheckedTimestamp::from_timestamplike(dt.clone())?);
    }

    let (mut year, mut month, mut day) = (dt.year(), dt.month0() as i32, dt.day());
    let years = months / 12;
    year = year
        .checked_add(years)
        .ok_or(EvalError::TimestampOutOfRange)?;

    months %= 12;
    // positive modulus is easier to reason about
    if months < 0 {
        year -= 1;
        months += 12;
    }
    year += (month + months) / 12;
    month = (month + months) % 12;
    // account for dt.month0
    month += 1;

    // handle going from January 31st to February by saturation
    let mut new_d = chrono::NaiveDate::from_ymd_opt(year, month as u32, day);
    while new_d.is_none() {
        // If we have decremented day past 28 and are still receiving `None`,
        // then we have generally overflowed `NaiveDate`.
        if day < 28 {
            return Err(EvalError::TimestampOutOfRange);
        }
        day -= 1;
        new_d = chrono::NaiveDate::from_ymd_opt(year, month as u32, day);
    }
    let new_d = new_d.unwrap();

    // Neither postgres nor mysql support leap seconds, so this should be safe.
    //
    // Both my testing and https://dba.stackexchange.com/a/105829 support the
    // idea that we should ignore leap seconds
    let new_dt = new_d
        .and_hms_nano_opt(dt.hour(), dt.minute(), dt.second(), dt.nanosecond())
        .unwrap();
    let new_dt = T::from_date_time(new_dt);
    Ok(CheckedTimestamp::from_timestamplike(new_dt)?)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "Numeric",
    is_infix_op = true,
    sqlname = "+",
    propagates_nulls = true
)]
fn add_numeric<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mut cx = numeric::cx_datum();
    let mut a = a.unwrap_numeric().0;
    cx.add(&mut a, &b.unwrap_numeric().0);
    if cx.status().overflow() {
        Err(EvalError::FloatOverflow)
    } else {
        Ok(Datum::from(a))
    }
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "Interval",
    is_infix_op = true,
    sqlname = "+",
    propagates_nulls = true
)]
fn add_interval<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_interval()
        .checked_add(&b.unwrap_interval())
        .ok_or_else(|| EvalError::IntervalOutOfRange(format!("{a} + {b}").into()))
        .map(Datum::from)
}

#[sqlfunc(
    output_type = "i16",
    is_infix_op = true,
    sqlname = "&",
    propagates_nulls = true
)]
fn bit_and_int16<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int16() & b.unwrap_int16())
}

#[sqlfunc(
    output_type = "i32",
    is_infix_op = true,
    sqlname = "&",
    propagates_nulls = true
)]
fn bit_and_int32<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int32() & b.unwrap_int32())
}

#[sqlfunc(
    output_type = "i64",
    is_infix_op = true,
    sqlname = "&",
    propagates_nulls = true
)]
fn bit_and_int64<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int64() & b.unwrap_int64())
}

#[sqlfunc(
    output_type = "u16",
    is_infix_op = true,
    sqlname = "&",
    propagates_nulls = true
)]
fn bit_and_uint16<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_uint16() & b.unwrap_uint16())
}

#[sqlfunc(
    output_type = "u32",
    is_infix_op = true,
    sqlname = "&",
    propagates_nulls = true
)]
fn bit_and_uint32<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_uint32() & b.unwrap_uint32())
}

#[sqlfunc(
    output_type = "u64",
    is_infix_op = true,
    sqlname = "&",
    propagates_nulls = true
)]
fn bit_and_uint64<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_uint64() & b.unwrap_uint64())
}

#[sqlfunc(
    output_type = "i16",
    is_infix_op = true,
    sqlname = "|",
    propagates_nulls = true
)]
fn bit_or_int16<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int16() | b.unwrap_int16())
}

#[sqlfunc(
    output_type = "i32",
    is_infix_op = true,
    sqlname = "|",
    propagates_nulls = true
)]
fn bit_or_int32<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int32() | b.unwrap_int32())
}

#[sqlfunc(
    output_type = "i64",
    is_infix_op = true,
    sqlname = "|",
    propagates_nulls = true
)]
fn bit_or_int64<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int64() | b.unwrap_int64())
}

#[sqlfunc(
    output_type = "u16",
    is_infix_op = true,
    sqlname = "|",
    propagates_nulls = true
)]
fn bit_or_uint16<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_uint16() | b.unwrap_uint16())
}

#[sqlfunc(
    output_type = "u32",
    is_infix_op = true,
    sqlname = "|",
    propagates_nulls = true
)]
fn bit_or_uint32<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_uint32() | b.unwrap_uint32())
}

#[sqlfunc(
    output_type = "u64",
    is_infix_op = true,
    sqlname = "|",
    propagates_nulls = true
)]
fn bit_or_uint64<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_uint64() | b.unwrap_uint64())
}

#[sqlfunc(
    output_type = "i16",
    is_infix_op = true,
    sqlname = "#",
    propagates_nulls = true
)]
fn bit_xor_int16<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int16() ^ b.unwrap_int16())
}

#[sqlfunc(
    output_type = "i32",
    is_infix_op = true,
    sqlname = "#",
    propagates_nulls = true
)]
fn bit_xor_int32<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int32() ^ b.unwrap_int32())
}

#[sqlfunc(
    output_type = "i64",
    is_infix_op = true,
    sqlname = "#",
    propagates_nulls = true
)]
fn bit_xor_int64<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_int64() ^ b.unwrap_int64())
}

#[sqlfunc(
    output_type = "u16",
    is_infix_op = true,
    sqlname = "#",
    propagates_nulls = true
)]
fn bit_xor_uint16<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_uint16() ^ b.unwrap_uint16())
}

#[sqlfunc(
    output_type = "u32",
    is_infix_op = true,
    sqlname = "#",
    propagates_nulls = true
)]
fn bit_xor_uint32<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_uint32() ^ b.unwrap_uint32())
}

#[sqlfunc(
    output_type = "u64",
    is_infix_op = true,
    sqlname = "#",
    propagates_nulls = true
)]
fn bit_xor_uint64<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_uint64() ^ b.unwrap_uint64())
}

#[sqlfunc(
    output_type = "i16",
    is_infix_op = true,
    sqlname = "<<",
    propagates_nulls = true
)]
// TODO(benesch): remove potentially dangerous usage of `as`.
#[allow(clippy::as_conversions)]
fn bit_shift_left_int16<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    // widen to i32 and then cast back to i16 in order emulate the C promotion rules used in by Postgres
    // when the rhs in the 16-31 range, e.g. (1 << 17 should evaluate to 0)
    // see https://github.com/postgres/postgres/blob/REL_14_STABLE/src/backend/utils/adt/int.c#L1460-L1476
    let lhs: i32 = a.unwrap_int16() as i32;
    let rhs: u32 = b.unwrap_int32() as u32;
    Datum::from(lhs.wrapping_shl(rhs) as i16)
}

#[sqlfunc(
    output_type = "i32",
    is_infix_op = true,
    sqlname = "<<",
    propagates_nulls = true
)]
// TODO(benesch): remove potentially dangerous usage of `as`.
#[allow(clippy::as_conversions)]
fn bit_shift_left_int32<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let lhs = a.unwrap_int32();
    let rhs = b.unwrap_int32() as u32;
    Datum::from(lhs.wrapping_shl(rhs))
}

#[sqlfunc(
    output_type = "i64",
    is_infix_op = true,
    sqlname = "<<",
    propagates_nulls = true
)]
// TODO(benesch): remove potentially dangerous usage of `as`.
#[allow(clippy::as_conversions)]
fn bit_shift_left_int64<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let lhs = a.unwrap_int64();
    let rhs = b.unwrap_int32() as u32;
    Datum::from(lhs.wrapping_shl(rhs))
}

#[sqlfunc(
    output_type = "u16",
    is_infix_op = true,
    sqlname = "<<",
    propagates_nulls = true
)]
// TODO(benesch): remove potentially dangerous usage of `as`.
#[allow(clippy::as_conversions)]
fn bit_shift_left_uint16<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    // widen to u32 and then cast back to u16 in order emulate the C promotion rules used in by Postgres
    // when the rhs in the 16-31 range, e.g. (1 << 17 should evaluate to 0)
    // see https://github.com/postgres/postgres/blob/REL_14_STABLE/src/backend/utils/adt/int.c#L1460-L1476
    let lhs: u32 = a.unwrap_uint16() as u32;
    let rhs: u32 = b.unwrap_uint32();
    Datum::from(lhs.wrapping_shl(rhs) as u16)
}

#[sqlfunc(
    output_type = "u32",
    is_infix_op = true,
    sqlname = "<<",
    propagates_nulls = true
)]
fn bit_shift_left_uint32<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let lhs = a.unwrap_uint32();
    let rhs = b.unwrap_uint32();
    Datum::from(lhs.wrapping_shl(rhs))
}

#[sqlfunc(
    output_type = "u64",
    is_infix_op = true,
    sqlname = "<<",
    propagates_nulls = true
)]
fn bit_shift_left_uint64<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let lhs = a.unwrap_uint64();
    let rhs = b.unwrap_uint32();
    Datum::from(lhs.wrapping_shl(rhs))
}

#[sqlfunc(
    output_type = "i16",
    is_infix_op = true,
    sqlname = ">>",
    propagates_nulls = true
)]
// TODO(benesch): remove potentially dangerous usage of `as`.
#[allow(clippy::as_conversions)]
fn bit_shift_right_int16<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    // widen to i32 and then cast back to i16 in order emulate the C promotion rules used in by Postgres
    // when the rhs in the 16-31 range, e.g. (-32767 >> 17 should evaluate to -1)
    // see https://github.com/postgres/postgres/blob/REL_14_STABLE/src/backend/utils/adt/int.c#L1460-L1476
    let lhs = a.unwrap_int16() as i32;
    let rhs = b.unwrap_int32() as u32;
    Datum::from(lhs.wrapping_shr(rhs) as i16)
}

#[sqlfunc(
    output_type = "i32",
    is_infix_op = true,
    sqlname = ">>",
    propagates_nulls = true
)]
// TODO(benesch): remove potentially dangerous usage of `as`.
#[allow(clippy::as_conversions)]
fn bit_shift_right_int32<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let lhs = a.unwrap_int32();
    let rhs = b.unwrap_int32() as u32;
    Datum::from(lhs.wrapping_shr(rhs))
}

#[sqlfunc(
    output_type = "i64",
    is_infix_op = true,
    sqlname = ">>",
    propagates_nulls = true
)]
// TODO(benesch): remove potentially dangerous usage of `as`.
#[allow(clippy::as_conversions)]
fn bit_shift_right_int64<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let lhs = a.unwrap_int64();
    let rhs = b.unwrap_int32() as u32;
    Datum::from(lhs.wrapping_shr(rhs))
}

#[sqlfunc(
    output_type = "u16",
    is_infix_op = true,
    sqlname = ">>",
    propagates_nulls = true
)]
// TODO(benesch): remove potentially dangerous usage of `as`.
#[allow(clippy::as_conversions)]
fn bit_shift_right_uint16<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    // widen to u32 and then cast back to u16 in order emulate the C promotion rules used in by Postgres
    // when the rhs in the 16-31 range, e.g. (-32767 >> 17 should evaluate to -1)
    // see https://github.com/postgres/postgres/blob/REL_14_STABLE/src/backend/utils/adt/int.c#L1460-L1476
    let lhs = a.unwrap_uint16() as u32;
    let rhs = b.unwrap_uint32();
    Datum::from(lhs.wrapping_shr(rhs) as u16)
}

#[sqlfunc(
    output_type = "u32",
    is_infix_op = true,
    sqlname = ">>",
    propagates_nulls = true
)]
fn bit_shift_right_uint32<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let lhs = a.unwrap_uint32();
    let rhs = b.unwrap_uint32();
    Datum::from(lhs.wrapping_shr(rhs))
}

#[sqlfunc(
    output_type = "u64",
    is_infix_op = true,
    sqlname = ">>",
    propagates_nulls = true
)]
fn bit_shift_right_uint64<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let lhs = a.unwrap_uint64();
    let rhs = b.unwrap_uint32();
    Datum::from(lhs.wrapping_shr(rhs))
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "i16",
    is_infix_op = true,
    sqlname = "-",
    propagates_nulls = true
)]
fn sub_int16<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_int16()
        .checked_sub(b.unwrap_int16())
        .ok_or(EvalError::NumericFieldOverflow)
        .map(Datum::from)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "i32",
    is_infix_op = true,
    sqlname = "-",
    propagates_nulls = true
)]
fn sub_int32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_int32()
        .checked_sub(b.unwrap_int32())
        .ok_or(EvalError::NumericFieldOverflow)
        .map(Datum::from)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "i64",
    is_infix_op = true,
    sqlname = "-",
    propagates_nulls = true
)]
fn sub_int64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_int64()
        .checked_sub(b.unwrap_int64())
        .ok_or(EvalError::NumericFieldOverflow)
        .map(Datum::from)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "u16",
    is_infix_op = true,
    sqlname = "-",
    propagates_nulls = true
)]
fn sub_uint16<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_uint16()
        .checked_sub(b.unwrap_uint16())
        .ok_or_else(|| EvalError::UInt16OutOfRange(format!("{a} - {b}").into()))
        .map(Datum::from)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "u32",
    is_infix_op = true,
    sqlname = "-",
    propagates_nulls = true
)]
fn sub_uint32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_uint32()
        .checked_sub(b.unwrap_uint32())
        .ok_or_else(|| EvalError::UInt32OutOfRange(format!("{a} - {b}").into()))
        .map(Datum::from)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "u64",
    is_infix_op = true,
    sqlname = "-",
    propagates_nulls = true
)]
fn sub_uint64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_uint64()
        .checked_sub(b.unwrap_uint64())
        .ok_or_else(|| EvalError::UInt64OutOfRange(format!("{a} - {b}").into()))
        .map(Datum::from)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "f32",
    is_infix_op = true,
    sqlname = "-",
    propagates_nulls = true
)]
fn sub_float32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let a = a.unwrap_float32();
    let b = b.unwrap_float32();
    let difference = a - b;
    if difference.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        Err(EvalError::FloatOverflow)
    } else {
        Ok(Datum::from(difference))
    }
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "f64",
    is_infix_op = true,
    sqlname = "-",
    propagates_nulls = true
)]
fn sub_float64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let a = a.unwrap_float64();
    let b = b.unwrap_float64();
    let difference = a - b;
    if difference.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        Err(EvalError::FloatOverflow)
    } else {
        Ok(Datum::from(difference))
    }
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "Numeric",
    is_infix_op = true,
    sqlname = "-",
    propagates_nulls = true
)]
fn sub_numeric<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mut cx = numeric::cx_datum();
    let mut a = a.unwrap_numeric().0;
    cx.sub(&mut a, &b.unwrap_numeric().0);
    if cx.status().overflow() {
        Err(EvalError::FloatOverflow)
    } else {
        Ok(Datum::from(a))
    }
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "Interval",
    sqlname = "age",
    propagates_nulls = true
)]
fn age_timestamp<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let a_ts = a.unwrap_timestamp();
    let b_ts = b.unwrap_timestamp();
    let age = a_ts.age(&b_ts)?;

    Ok(Datum::from(age))
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "Interval",
    sqlname = "age",
    propagates_nulls = true
)]
fn age_timestamptz<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let a_ts = a.unwrap_timestamptz();
    let b_ts = b.unwrap_timestamptz();
    let age = a_ts.age(&b_ts)?;

    Ok(Datum::from(age))
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "Interval",
    is_infix_op = true,
    sqlname = "-",
    propagates_nulls = true
)]
fn sub_timestamp<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_timestamp() - b.unwrap_timestamp())
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "Interval",
    is_infix_op = true,
    sqlname = "-",
    propagates_nulls = true
)]
fn sub_timestamptz<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_timestamptz() - b.unwrap_timestamptz())
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "i32",
    is_infix_op = true,
    sqlname = "-",
    propagates_nulls = true
)]
fn sub_date<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_date() - b.unwrap_date())
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "Interval",
    is_infix_op = true,
    sqlname = "-",
    propagates_nulls = true
)]
fn sub_time<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a.unwrap_time() - b.unwrap_time())
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "Interval",
    is_infix_op = true,
    sqlname = "-",
    propagates_nulls = true
)]
fn sub_interval<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    b.unwrap_interval()
        .checked_neg()
        .and_then(|b| b.checked_add(&a.unwrap_interval()))
        .ok_or_else(|| EvalError::IntervalOutOfRange(format!("{a} - {b}").into()))
        .map(Datum::from)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "CheckedTimestamp<NaiveDateTime>",
    is_infix_op = true,
    sqlname = "-",
    propagates_nulls = true
)]
fn sub_date_interval<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let date = a.unwrap_date();
    let interval = b.unwrap_interval();

    let dt = NaiveDate::from(date).and_hms_opt(0, 0, 0).unwrap();
    let dt = interval
        .months
        .checked_neg()
        .ok_or_else(|| EvalError::IntervalOutOfRange(interval.months.to_string().into()))
        .and_then(|months| add_timestamp_months(&dt, months))?;
    let dt = dt
        .checked_sub_signed(interval.duration_as_chrono())
        .ok_or(EvalError::TimestampOutOfRange)?;
    Ok(dt.try_into()?)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "chrono::NaiveTime",
    is_infix_op = true,
    sqlname = "-",
    propagates_nulls = true
)]
fn sub_time_interval<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let time = a.unwrap_time();
    let interval = b.unwrap_interval();
    let (t, _) = time.overflowing_sub_signed(interval.duration_as_chrono());
    Datum::Time(t)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "i16",
    is_infix_op = true,
    sqlname = "*",
    propagates_nulls = true
)]
fn mul_int16<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_int16()
        .checked_mul(b.unwrap_int16())
        .ok_or(EvalError::NumericFieldOverflow)
        .map(Datum::from)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "i32",
    is_infix_op = true,
    sqlname = "*",
    propagates_nulls = true
)]
fn mul_int32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_int32()
        .checked_mul(b.unwrap_int32())
        .ok_or(EvalError::NumericFieldOverflow)
        .map(Datum::from)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "i64",
    is_infix_op = true,
    sqlname = "*",
    propagates_nulls = true
)]
fn mul_int64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_int64()
        .checked_mul(b.unwrap_int64())
        .ok_or(EvalError::NumericFieldOverflow)
        .map(Datum::from)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "u16",
    is_infix_op = true,
    sqlname = "*",
    propagates_nulls = true
)]
fn mul_uint16<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_uint16()
        .checked_mul(b.unwrap_uint16())
        .ok_or_else(|| EvalError::UInt16OutOfRange(format!("{a} * {b}").into()))
        .map(Datum::from)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "u32",
    is_infix_op = true,
    sqlname = "*",
    propagates_nulls = true
)]
fn mul_uint32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_uint32()
        .checked_mul(b.unwrap_uint32())
        .ok_or_else(|| EvalError::UInt32OutOfRange(format!("{a} * {b}").into()))
        .map(Datum::from)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "u64",
    is_infix_op = true,
    sqlname = "*",
    propagates_nulls = true
)]
fn mul_uint64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_uint64()
        .checked_mul(b.unwrap_uint64())
        .ok_or_else(|| EvalError::UInt64OutOfRange(format!("{a} * {b}").into()))
        .map(Datum::from)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "f32",
    is_infix_op = true,
    sqlname = "*",
    propagates_nulls = true
)]
fn mul_float32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let a = a.unwrap_float32();
    let b = b.unwrap_float32();
    let product = a * b;
    if product.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        Err(EvalError::FloatOverflow)
    } else if product == 0.0f32 && a != 0.0f32 && b != 0.0f32 {
        Err(EvalError::FloatUnderflow)
    } else {
        Ok(Datum::from(product))
    }
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "f64",
    is_infix_op = true,
    sqlname = "*",
    propagates_nulls = true
)]
fn mul_float64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let a = a.unwrap_float64();
    let b = b.unwrap_float64();
    let product = a * b;
    if product.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        Err(EvalError::FloatOverflow)
    } else if product == 0.0f64 && a != 0.0f64 && b != 0.0f64 {
        Err(EvalError::FloatUnderflow)
    } else {
        Ok(Datum::from(product))
    }
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "Numeric",
    is_infix_op = true,
    sqlname = "*",
    propagates_nulls = true
)]
fn mul_numeric<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mut cx = numeric::cx_datum();
    let mut a = a.unwrap_numeric().0;
    cx.mul(&mut a, &b.unwrap_numeric().0);
    let cx_status = cx.status();
    if cx_status.overflow() {
        Err(EvalError::FloatOverflow)
    } else if cx_status.subnormal() {
        Err(EvalError::FloatUnderflow)
    } else {
        numeric::munge_numeric(&mut a).unwrap();
        Ok(Datum::from(a))
    }
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "Interval",
    is_infix_op = true,
    sqlname = "*",
    propagates_nulls = true
)]
fn mul_interval<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    a.unwrap_interval()
        .checked_mul(b.unwrap_float64())
        .ok_or_else(|| EvalError::IntervalOutOfRange(format!("{a} * {b}").into()))
        .map(Datum::from)
}

#[sqlfunc(
    is_monotone = "(true, false)",
    output_type = "i16",
    is_infix_op = true,
    sqlname = "/",
    propagates_nulls = true
)]
fn div_int16<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_int16();
    if b == 0 {
        Err(EvalError::DivisionByZero)
    } else {
        a.unwrap_int16()
            .checked_div(b)
            .map(Datum::from)
            .ok_or_else(|| EvalError::Int16OutOfRange(format!("{a} / {b}").into()))
    }
}

#[sqlfunc(
    is_monotone = "(true, false)",
    output_type = "i32",
    is_infix_op = true,
    sqlname = "/",
    propagates_nulls = true
)]
fn div_int32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_int32();
    if b == 0 {
        Err(EvalError::DivisionByZero)
    } else {
        a.unwrap_int32()
            .checked_div(b)
            .map(Datum::from)
            .ok_or_else(|| EvalError::Int32OutOfRange(format!("{a} / {b}").into()))
    }
}

#[sqlfunc(
    is_monotone = "(true, false)",
    output_type = "i64",
    is_infix_op = true,
    sqlname = "/",
    propagates_nulls = true
)]
fn div_int64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_int64();
    if b == 0 {
        Err(EvalError::DivisionByZero)
    } else {
        a.unwrap_int64()
            .checked_div(b)
            .map(Datum::from)
            .ok_or_else(|| EvalError::Int64OutOfRange(format!("{a} / {b}").into()))
    }
}

#[sqlfunc(
    is_monotone = "(true, false)",
    output_type = "u16",
    is_infix_op = true,
    sqlname = "/",
    propagates_nulls = true
)]
fn div_uint16<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_uint16();
    if b == 0 {
        Err(EvalError::DivisionByZero)
    } else {
        Ok(Datum::from(a.unwrap_uint16() / b))
    }
}

#[sqlfunc(
    is_monotone = "(true, false)",
    output_type = "u32",
    is_infix_op = true,
    sqlname = "/",
    propagates_nulls = true
)]
fn div_uint32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_uint32();
    if b == 0 {
        Err(EvalError::DivisionByZero)
    } else {
        Ok(Datum::from(a.unwrap_uint32() / b))
    }
}

#[sqlfunc(
    is_monotone = "(true, false)",
    output_type = "u64",
    is_infix_op = true,
    sqlname = "/",
    propagates_nulls = true
)]
fn div_uint64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_uint64();
    if b == 0 {
        Err(EvalError::DivisionByZero)
    } else {
        Ok(Datum::from(a.unwrap_uint64() / b))
    }
}

#[sqlfunc(
    is_monotone = "(true, false)",
    output_type = "f32",
    is_infix_op = true,
    sqlname = "/",
    propagates_nulls = true
)]
fn div_float32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let a = a.unwrap_float32();
    let b = b.unwrap_float32();
    if b == 0.0f32 && !a.is_nan() {
        Err(EvalError::DivisionByZero)
    } else {
        let quotient = a / b;
        if quotient.is_infinite() && !a.is_infinite() {
            Err(EvalError::FloatOverflow)
        } else if quotient == 0.0f32 && a != 0.0f32 && !b.is_infinite() {
            Err(EvalError::FloatUnderflow)
        } else {
            Ok(Datum::from(quotient))
        }
    }
}

#[sqlfunc(
    is_monotone = "(true, false)",
    output_type = "f64",
    is_infix_op = true,
    sqlname = "/",
    propagates_nulls = true
)]
fn div_float64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let a = a.unwrap_float64();
    let b = b.unwrap_float64();
    if b == 0.0f64 && !a.is_nan() {
        Err(EvalError::DivisionByZero)
    } else {
        let quotient = a / b;
        if quotient.is_infinite() && !a.is_infinite() {
            Err(EvalError::FloatOverflow)
        } else if quotient == 0.0f64 && a != 0.0f64 && !b.is_infinite() {
            Err(EvalError::FloatUnderflow)
        } else {
            Ok(Datum::from(quotient))
        }
    }
}

#[sqlfunc(
    is_monotone = "(true, false)",
    output_type = "Numeric",
    is_infix_op = true,
    sqlname = "/",
    propagates_nulls = true
)]
fn div_numeric<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mut cx = numeric::cx_datum();
    let mut a = a.unwrap_numeric().0;
    let b = b.unwrap_numeric().0;

    cx.div(&mut a, &b);
    let cx_status = cx.status();

    // checking the status for division by zero errors is insufficient because
    // the underlying library treats 0/0 as undefined and not division by zero.
    if b.is_zero() {
        Err(EvalError::DivisionByZero)
    } else if cx_status.overflow() {
        Err(EvalError::FloatOverflow)
    } else if cx_status.subnormal() {
        Err(EvalError::FloatUnderflow)
    } else {
        numeric::munge_numeric(&mut a).unwrap();
        Ok(Datum::from(a))
    }
}

#[sqlfunc(
    is_monotone = "(true, false)",
    output_type = "Interval",
    is_infix_op = true,
    sqlname = "/",
    propagates_nulls = true
)]
fn div_interval<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_float64();
    if b == 0.0 {
        Err(EvalError::DivisionByZero)
    } else {
        a.unwrap_interval()
            .checked_div(b)
            .ok_or_else(|| EvalError::IntervalOutOfRange(format!("{a} / {b}").into()))
            .map(Datum::from)
    }
}

#[sqlfunc(
    output_type = "i16",
    is_infix_op = true,
    sqlname = "%",
    propagates_nulls = true
)]
fn mod_int16<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_int16();
    if b == 0 {
        Err(EvalError::DivisionByZero)
    } else {
        Ok(Datum::from(a.unwrap_int16().checked_rem(b).unwrap_or(0)))
    }
}

#[sqlfunc(
    output_type = "i32",
    is_infix_op = true,
    sqlname = "%",
    propagates_nulls = true
)]
fn mod_int32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_int32();
    if b == 0 {
        Err(EvalError::DivisionByZero)
    } else {
        Ok(Datum::from(a.unwrap_int32().checked_rem(b).unwrap_or(0)))
    }
}

#[sqlfunc(
    output_type = "i64",
    is_infix_op = true,
    sqlname = "%",
    propagates_nulls = true
)]
fn mod_int64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_int64();
    if b == 0 {
        Err(EvalError::DivisionByZero)
    } else {
        Ok(Datum::from(a.unwrap_int64().checked_rem(b).unwrap_or(0)))
    }
}

#[sqlfunc(
    output_type = "u16",
    is_infix_op = true,
    sqlname = "%",
    propagates_nulls = true
)]
fn mod_uint16<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_uint16();
    if b == 0 {
        Err(EvalError::DivisionByZero)
    } else {
        Ok(Datum::from(a.unwrap_uint16() % b))
    }
}

#[sqlfunc(
    output_type = "u32",
    is_infix_op = true,
    sqlname = "%",
    propagates_nulls = true
)]
fn mod_uint32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_uint32();
    if b == 0 {
        Err(EvalError::DivisionByZero)
    } else {
        Ok(Datum::from(a.unwrap_uint32() % b))
    }
}

#[sqlfunc(
    output_type = "u64",
    is_infix_op = true,
    sqlname = "%",
    propagates_nulls = true
)]
fn mod_uint64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_uint64();
    if b == 0 {
        Err(EvalError::DivisionByZero)
    } else {
        Ok(Datum::from(a.unwrap_uint64() % b))
    }
}

#[sqlfunc(
    output_type = "f32",
    is_infix_op = true,
    sqlname = "%",
    propagates_nulls = true
)]
fn mod_float32<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_float32();
    if b == 0.0 {
        Err(EvalError::DivisionByZero)
    } else {
        Ok(Datum::from(a.unwrap_float32() % b))
    }
}

#[sqlfunc(
    output_type = "f64",
    is_infix_op = true,
    sqlname = "%",
    propagates_nulls = true
)]
fn mod_float64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let b = b.unwrap_float64();
    if b == 0.0 {
        Err(EvalError::DivisionByZero)
    } else {
        Ok(Datum::from(a.unwrap_float64() % b))
    }
}

#[sqlfunc(
    output_type = "Numeric",
    is_infix_op = true,
    sqlname = "%",
    propagates_nulls = true
)]
fn mod_numeric<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mut a = a.unwrap_numeric();
    let b = b.unwrap_numeric();
    if b.0.is_zero() {
        return Err(EvalError::DivisionByZero);
    }
    let mut cx = numeric::cx_datum();
    // Postgres does _not_ use IEEE 754-style remainder
    cx.rem(&mut a.0, &b.0);
    numeric::munge_numeric(&mut a.0).unwrap();
    Ok(Datum::Numeric(a))
}

fn neg_interval_inner(a: Datum) -> Result<Interval, EvalError> {
    a.unwrap_interval()
        .checked_neg()
        .ok_or_else(|| EvalError::IntervalOutOfRange(a.to_string().into()))
}

fn log_guard_numeric(val: &Numeric, function_name: &str) -> Result<(), EvalError> {
    if val.is_negative() {
        return Err(EvalError::NegativeOutOfDomain(function_name.into()));
    }
    if val.is_zero() {
        return Err(EvalError::ZeroOutOfDomain(function_name.into()));
    }
    Ok(())
}

#[sqlfunc(output_type = "Numeric", sqlname = "log", propagates_nulls = true)]
fn log_base_numeric<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mut a = a.unwrap_numeric().0;
    log_guard_numeric(&a, "log")?;
    let mut b = b.unwrap_numeric().0;
    log_guard_numeric(&b, "log")?;
    let mut cx = numeric::cx_datum();
    cx.ln(&mut a);
    cx.ln(&mut b);
    cx.div(&mut b, &a);
    if a.is_zero() {
        Err(EvalError::DivisionByZero)
    } else {
        // This division can result in slightly wrong answers due to the
        // limitation of dividing irrational numbers. To correct that, see if
        // rounding off the value from its `numeric::NUMERIC_DATUM_MAX_PRECISION
        // - 1`th position results in an integral value.
        cx.set_precision(usize::from(numeric::NUMERIC_DATUM_MAX_PRECISION - 1))
            .expect("reducing precision below max always succeeds");
        let mut integral_check = b.clone();

        // `reduce` rounds to the context's final digit when the number of
        // digits in its argument exceeds its precision. We've contrived that to
        // happen by shrinking the context's precision by 1.
        cx.reduce(&mut integral_check);

        // Reduced integral values always have a non-negative exponent.
        let mut b = if integral_check.exponent() >= 0 {
            // We believe our result should have been an integral
            integral_check
        } else {
            b
        };

        numeric::munge_numeric(&mut b).unwrap();
        Ok(Datum::from(b))
    }
}

#[sqlfunc(output_type = "f64", propagates_nulls = true)]
fn power<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let a = a.unwrap_float64();
    let b = b.unwrap_float64();
    if a == 0.0 && b.is_sign_negative() {
        return Err(EvalError::Undefined(
            "zero raised to a negative power".into(),
        ));
    }
    if a.is_sign_negative() && b.fract() != 0.0 {
        // Equivalent to PG error:
        // > a negative number raised to a non-integer power yields a complex result
        return Err(EvalError::ComplexOutOfRange("pow".into()));
    }
    let res = a.powf(b);
    if res.is_infinite() {
        return Err(EvalError::FloatOverflow);
    }
    if res == 0.0 && a != 0.0 {
        return Err(EvalError::FloatUnderflow);
    }
    Ok(Datum::from(res))
}

#[sqlfunc(output_type = "uuid::Uuid", propagates_nulls = true)]
fn uuid_generate_v5<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let a = a.unwrap_uuid();
    let b = b.unwrap_str();
    let res = uuid::Uuid::new_v5(&a, b.as_bytes());
    Datum::Uuid(res)
}

#[sqlfunc(output_type = "Numeric", propagates_nulls = true)]
fn power_numeric<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mut a = a.unwrap_numeric().0;
    let b = b.unwrap_numeric().0;
    if a.is_zero() {
        if b.is_zero() {
            return Ok(Datum::from(Numeric::from(1)));
        }
        if b.is_negative() {
            return Err(EvalError::Undefined(
                "zero raised to a negative power".into(),
            ));
        }
    }
    if a.is_negative() && b.exponent() < 0 {
        // Equivalent to PG error:
        // > a negative number raised to a non-integer power yields a complex result
        return Err(EvalError::ComplexOutOfRange("pow".into()));
    }
    let mut cx = numeric::cx_datum();
    cx.pow(&mut a, &b);
    let cx_status = cx.status();
    if cx_status.overflow() || (cx_status.invalid_operation() && !b.is_negative()) {
        Err(EvalError::FloatOverflow)
    } else if cx_status.subnormal() || cx_status.invalid_operation() {
        Err(EvalError::FloatUnderflow)
    } else {
        numeric::munge_numeric(&mut a).unwrap();
        Ok(Datum::from(a))
    }
}

#[sqlfunc(output_type = "i32", propagates_nulls = true)]
fn get_bit<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let bytes = a.unwrap_bytes();
    let index = b.unwrap_int32();
    let err = EvalError::IndexOutOfRange {
        provided: index,
        valid_end: i32::try_from(bytes.len().saturating_mul(8)).unwrap() - 1,
    };

    let index = usize::try_from(index).map_err(|_| err.clone())?;

    let byte_index = index / 8;
    let bit_index = index % 8;

    let i = bytes
        .get(byte_index)
        .map(|b| (*b >> bit_index) & 1)
        .ok_or(err)?;
    assert!(i == 0 || i == 1);
    Ok(Datum::from(i32::from(i)))
}

#[sqlfunc(output_type = "i32", propagates_nulls = true)]
fn get_byte<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let bytes = a.unwrap_bytes();
    let index = b.unwrap_int32();
    let err = EvalError::IndexOutOfRange {
        provided: index,
        valid_end: i32::try_from(bytes.len()).unwrap() - 1,
    };
    let i: &u8 = bytes
        .get(usize::try_from(index).map_err(|_| err.clone())?)
        .ok_or(err)?;
    Ok(Datum::from(i32::from(*i)))
}

#[sqlfunc(
    output_type = "bool",
    sqlname = "constant_time_compare_bytes",
    propagates_nulls = true
)]
pub fn constant_time_eq_bytes<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let a_bytes = a.unwrap_bytes();
    let b_bytes = b.unwrap_bytes();
    Ok(Datum::from(bool::from(a_bytes.ct_eq(b_bytes))))
}

#[sqlfunc(
    output_type = "bool",
    sqlname = "constant_time_compare_strings",
    propagates_nulls = true
)]
pub fn constant_time_eq_string<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let a = a.unwrap_str();
    let b = b.unwrap_str();
    Ok(Datum::from(bool::from(a.as_bytes().ct_eq(b.as_bytes()))))
}

fn contains_range_elem<'a, R: RangeOps<'a>>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a>
where
    <R as TryFrom<Datum<'a>>>::Error: std::fmt::Debug,
{
    let range = a.unwrap_range();
    let elem = R::try_from(b).expect("type checking must produce correct R");
    Datum::from(range.contains_elem(&elem))
}

#[sqlfunc(is_infix_op = true, sqlname = "@>", propagates_nulls = true)]
fn range_contains_i32<'a>(a: Range<Datum<'a>>, b: i32) -> bool {
    a.contains_elem(&b)
}

#[sqlfunc(is_infix_op = true, sqlname = "@>", propagates_nulls = true)]
fn range_contains_i64<'a>(a: Range<Datum<'a>>, elem: i64) -> bool {
    a.contains_elem(&elem)
}

#[sqlfunc(is_infix_op = true, sqlname = "@>", propagates_nulls = true)]
fn range_contains_date<'a>(a: Range<Datum<'a>>, elem: Date) -> bool {
    a.contains_elem(&elem)
}

#[sqlfunc(is_infix_op = true, sqlname = "@>", propagates_nulls = true)]
fn range_contains_numeric<'a>(a: Range<Datum<'a>>, elem: OrderedDecimal<Numeric>) -> bool {
    a.contains_elem(&elem)
}

#[sqlfunc(is_infix_op = true, sqlname = "@>", propagates_nulls = true)]
fn range_contains_timestamp<'a>(
    a: Range<Datum<'a>>,
    elem: CheckedTimestamp<NaiveDateTime>,
) -> bool {
    a.contains_elem(&elem)
}

#[sqlfunc(is_infix_op = true, sqlname = "@>", propagates_nulls = true)]
fn range_contains_timestamp_tz<'a>(
    a: Range<Datum<'a>>,
    elem: CheckedTimestamp<DateTime<Utc>>,
) -> bool {
    a.contains_elem(&elem)
}

#[sqlfunc(is_infix_op = true, sqlname = "<@", propagates_nulls = true)]
fn range_contains_i32_rev<'a>(a: Range<Datum<'a>>, b: i32) -> bool {
    a.contains_elem(&b)
}

#[sqlfunc(is_infix_op = true, sqlname = "<@", propagates_nulls = true)]
fn range_contains_i64_rev<'a>(a: Range<Datum<'a>>, elem: i64) -> bool {
    a.contains_elem(&elem)
}

#[sqlfunc(is_infix_op = true, sqlname = "<@", propagates_nulls = true)]
fn range_contains_date_rev<'a>(a: Range<Datum<'a>>, elem: Date) -> bool {
    a.contains_elem(&elem)
}

#[sqlfunc(is_infix_op = true, sqlname = "<@", propagates_nulls = true)]
fn range_contains_numeric_rev<'a>(a: Range<Datum<'a>>, elem: OrderedDecimal<Numeric>) -> bool {
    a.contains_elem(&elem)
}

#[sqlfunc(is_infix_op = true, sqlname = "<@", propagates_nulls = true)]
fn range_contains_timestamp_rev<'a>(
    a: Range<Datum<'a>>,
    elem: CheckedTimestamp<NaiveDateTime>,
) -> bool {
    a.contains_elem(&elem)
}

#[sqlfunc(is_infix_op = true, sqlname = "<@", propagates_nulls = true)]
fn range_contains_timestamp_tz_rev<'a>(
    a: Range<Datum<'a>>,
    elem: CheckedTimestamp<DateTime<Utc>>,
) -> bool {
    a.contains_elem(&elem)
}

/// Macro to define binary function for various range operations.
/// Parameters:
/// 1. Unique binary function symbol.
/// 2. Range function symbol.
/// 3. SQL name for the function.
macro_rules! range_fn {
    ($fn:expr, $range_fn:expr, $sqlname:expr) => {
        paste::paste! {

            #[sqlfunc(
                output_type = "bool",
                is_infix_op = true,
                sqlname = $sqlname,
                propagates_nulls = true
            )]
            fn [< range_ $fn >]<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a>
            {
                let l = a.unwrap_range();
                let r = b.unwrap_range();
                Datum::from(Range::<Datum<'a>>::$range_fn(&l, &r))
            }
        }
    };
}

// RangeContainsRange is either @> or <@ depending on the order of the arguments.
// It doesn't influence the result, but it does influence the display string.
range_fn!(contains_range, contains_range, "@>");
range_fn!(contains_range_rev, contains_range, "<@");
range_fn!(overlaps, overlaps, "&&");
range_fn!(after, after, ">>");
range_fn!(before, before, "<<");
range_fn!(overleft, overleft, "&<");
range_fn!(overright, overright, "&>");
range_fn!(adjacent, adjacent, "-|-");

#[sqlfunc(
    output_type_expr = "input_type_a.scalar_type.without_modifiers().nullable(true)",
    is_infix_op = true,
    sqlname = "+",
    propagates_nulls = true,
    introduces_nulls = false
)]
fn range_union<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let l = a.unwrap_range();
    let r = b.unwrap_range();
    l.union(&r)?.into_result(temp_storage)
}

#[sqlfunc(
    output_type_expr = "input_type_a.scalar_type.without_modifiers().nullable(true)",
    is_infix_op = true,
    sqlname = "*",
    propagates_nulls = true,
    introduces_nulls = false
)]
fn range_intersection<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let l = a.unwrap_range();
    let r = b.unwrap_range();
    l.intersection(&r).into_result(temp_storage)
}

#[sqlfunc(
    output_type_expr = "input_type_a.scalar_type.without_modifiers().nullable(true)",
    is_infix_op = true,
    sqlname = "-",
    propagates_nulls = true,
    introduces_nulls = false
)]
fn range_difference<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let l = a.unwrap_range();
    let r = b.unwrap_range();
    l.difference(&r)?.into_result(temp_storage)
}

#[sqlfunc(
    output_type = "bool",
    is_infix_op = true,
    sqlname = "=",
    propagates_nulls = true
)]
fn eq<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    // SQL equality demands that if either input is null, then the result should be null. However,
    // we don't need to handle this case here; it is handled when `BinaryFunc::eval` checks
    // `propagates_nulls`.
    Datum::from(a == b)
}

#[sqlfunc(
    output_type = "bool",
    is_infix_op = true,
    sqlname = "!=",
    propagates_nulls = true
)]
fn not_eq<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a != b)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "bool",
    is_infix_op = true,
    sqlname = "<",
    propagates_nulls = true
)]
fn lt<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a < b)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "bool",
    is_infix_op = true,
    sqlname = "<=",
    propagates_nulls = true
)]
fn lte<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a <= b)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "bool",
    is_infix_op = true,
    sqlname = ">",
    propagates_nulls = true
)]
fn gt<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a > b)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "bool",
    is_infix_op = true,
    sqlname = ">=",
    propagates_nulls = true
)]
fn gte<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    Datum::from(a >= b)
}

fn to_char_timestamplike<'a, T>(ts: &T, format: &str, temp_storage: &'a RowArena) -> Datum<'a>
where
    T: TimestampLike,
{
    let fmt = DateTimeFormat::compile(format);
    Datum::String(temp_storage.push_string(fmt.render(ts)))
}

#[sqlfunc(output_type = "String", sqlname = "tocharts", propagates_nulls = true)]
fn to_char_timestamp_format<'a>(a: Datum<'a>, format: &str) -> String {
    let ts = a.unwrap_timestamp();
    let fmt = DateTimeFormat::compile(format);
    fmt.render(&*ts)
}

#[sqlfunc(
    output_type = "String",
    sqlname = "tochartstz",
    propagates_nulls = true
)]
fn to_char_timestamp_tz_format<'a>(a: Datum<'a>, format: &str) -> String {
    let ts = a.unwrap_timestamptz();
    let fmt = DateTimeFormat::compile(format);
    fmt.render(&*ts)
}

fn jsonb_get_int64<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    temp_storage: &'a RowArena,
    stringify: bool,
) -> Datum<'a> {
    let i = b.unwrap_int64();
    match a {
        Datum::List(list) => {
            let i = if i >= 0 {
                usize::cast_from(i.unsigned_abs())
            } else {
                // index backwards from the end
                let i = usize::cast_from(i.unsigned_abs());
                (list.iter().count()).wrapping_sub(i)
            };
            match list.iter().nth(i) {
                Some(d) if stringify => jsonb_stringify(d, temp_storage),
                Some(d) => d,
                None => Datum::Null,
            }
        }
        Datum::Map(_) => Datum::Null,
        _ => {
            if i == 0 || i == -1 {
                // I have no idea why postgres does this, but we're stuck with it
                if stringify {
                    jsonb_stringify(a, temp_storage)
                } else {
                    a
                }
            } else {
                Datum::Null
            }
        }
    }
}

fn jsonb_get_string<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    temp_storage: &'a RowArena,
    stringify: bool,
) -> Datum<'a> {
    let k = b.unwrap_str();
    match a {
        Datum::Map(dict) => match dict.iter().find(|(k2, _v)| k == *k2) {
            Some((_k, v)) if stringify => jsonb_stringify(v, temp_storage),
            Some((_k, v)) => v,
            None => Datum::Null,
        },
        _ => Datum::Null,
    }
}

fn jsonb_get_path<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    temp_storage: &'a RowArena,
    stringify: bool,
) -> Datum<'a> {
    let mut json = a;
    let path = b.unwrap_array().elements();
    for key in path.iter() {
        let key = match key {
            Datum::String(s) => s,
            Datum::Null => return Datum::Null,
            _ => unreachable!("keys in jsonb_get_path known to be strings"),
        };
        json = match json {
            Datum::Map(map) => match map.iter().find(|(k, _)| key == *k) {
                Some((_k, v)) => v,
                None => return Datum::Null,
            },
            Datum::List(list) => match strconv::parse_int64(key) {
                Ok(i) => {
                    let i = if i >= 0 {
                        usize::cast_from(i.unsigned_abs())
                    } else {
                        // index backwards from the end
                        let i = usize::cast_from(i.unsigned_abs());
                        (list.iter().count()).wrapping_sub(i)
                    };
                    match list.iter().nth(i) {
                        Some(e) => e,
                        None => return Datum::Null,
                    }
                }
                Err(_) => return Datum::Null,
            },
            _ => return Datum::Null,
        }
    }
    if stringify {
        jsonb_stringify(json, temp_storage)
    } else {
        json
    }
}

#[sqlfunc(
    output_type = "bool",
    is_infix_op = true,
    sqlname = "?",
    propagates_nulls = true
)]
fn jsonb_contains_string<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let k = b.unwrap_str();
    // https://www.postgresql.org/docs/current/datatype-json.html#JSON-CONTAINMENT
    match a {
        Datum::List(list) => list.iter().any(|k2| b == k2).into(),
        Datum::Map(dict) => dict.iter().any(|(k2, _v)| k == k2).into(),
        Datum::String(string) => (string == k).into(),
        _ => false.into(),
    }
}

#[sqlfunc(
    output_type = "bool",
    is_infix_op = true,
    sqlname = "?",
    propagates_nulls = true
)]
fn map_contains_key<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let map = a.unwrap_map();
    let k = b.unwrap_str(); // Map keys are always text.
    map.iter().any(|(k2, _v)| k == k2).into()
}

#[sqlfunc(
    output_type = "bool",
    is_infix_op = true,
    sqlname = "?&",
    propagates_nulls = true
)]
fn map_contains_all_keys<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let map = a.unwrap_map();
    let keys = b.unwrap_array();

    keys.elements()
        .iter()
        .all(|key| !key.is_null() && map.iter().any(|(k, _v)| k == key.unwrap_str()))
        .into()
}

#[sqlfunc(
    output_type = "bool",
    is_infix_op = true,
    sqlname = "?|",
    propagates_nulls = true
)]
fn map_contains_any_keys<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let map = a.unwrap_map();
    let keys = b.unwrap_array();

    keys.elements()
        .iter()
        .any(|key| !key.is_null() && map.iter().any(|(k, _v)| k == key.unwrap_str()))
        .into()
}

#[sqlfunc(
    output_type = "bool",
    is_infix_op = true,
    sqlname = "@>",
    propagates_nulls = true
)]
fn map_contains_map<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let map_a = a.unwrap_map();
    b.unwrap_map()
        .iter()
        .all(|(b_key, b_val)| {
            map_a
                .iter()
                .any(|(a_key, a_val)| (a_key == b_key) && (a_val == b_val))
        })
        .into()
}

#[sqlfunc(
    output_type_expr = "input_type_a.scalar_type.unwrap_map_value_type().clone().nullable(true)",
    is_infix_op = true,
    sqlname = "->",
    propagates_nulls = true,
    introduces_nulls = true
)]
fn map_get_value<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let target_key = b.unwrap_str();
    match a.unwrap_map().iter().find(|(key, _v)| target_key == *key) {
        Some((_k, v)) => v,
        None => Datum::Null,
    }
}

#[sqlfunc(
    output_type = "bool",
    is_infix_op = true,
    sqlname = "@>",
    propagates_nulls = true,
    introduces_nulls = false
)]
fn list_contains_list<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let a = a.unwrap_list();
    let b = b.unwrap_list();

    // NULL is never equal to NULL. If NULL is an element of b, b cannot be contained in a, even if a contains NULL.
    if b.iter().contains(&Datum::Null) {
        Datum::False
    } else {
        b.iter()
            .all(|item_b| a.iter().any(|item_a| item_a == item_b))
            .into()
    }
}

#[sqlfunc(
    output_type = "bool",
    is_infix_op = true,
    sqlname = "<@",
    propagates_nulls = true,
    introduces_nulls = false
)]
fn list_contains_list_rev<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    list_contains_list(b, a)
}

// TODO(jamii) nested loops are possibly not the fastest way to do this
#[sqlfunc(
    output_type = "bool",
    is_infix_op = true,
    sqlname = "@>",
    propagates_nulls = true
)]
fn jsonb_contains_jsonb<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    // https://www.postgresql.org/docs/current/datatype-json.html#JSON-CONTAINMENT
    fn contains(a: Datum, b: Datum, at_top_level: bool) -> bool {
        match (a, b) {
            (Datum::JsonNull, Datum::JsonNull) => true,
            (Datum::False, Datum::False) => true,
            (Datum::True, Datum::True) => true,
            (Datum::Numeric(a), Datum::Numeric(b)) => a == b,
            (Datum::String(a), Datum::String(b)) => a == b,
            (Datum::List(a), Datum::List(b)) => b
                .iter()
                .all(|b_elem| a.iter().any(|a_elem| contains(a_elem, b_elem, false))),
            (Datum::Map(a), Datum::Map(b)) => b.iter().all(|(b_key, b_val)| {
                a.iter()
                    .any(|(a_key, a_val)| (a_key == b_key) && contains(a_val, b_val, false))
            }),

            // fun special case
            (Datum::List(a), b) => {
                at_top_level && a.iter().any(|a_elem| contains(a_elem, b, false))
            }

            _ => false,
        }
    }
    contains(a, b, true).into()
}

#[sqlfunc(
    output_type_expr = "ScalarType::Jsonb.nullable(true)",
    is_infix_op = true,
    sqlname = "||",
    propagates_nulls = true,
    introduces_nulls = true
)]
fn jsonb_concat<'a>(a: Datum<'a>, b: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    match (a, b) {
        (Datum::Map(dict_a), Datum::Map(dict_b)) => {
            let mut pairs = dict_b.iter().chain(dict_a.iter()).collect::<Vec<_>>();
            // stable sort, so if keys collide dedup prefers dict_b
            pairs.sort_by(|(k1, _v1), (k2, _v2)| k1.cmp(k2));
            pairs.dedup_by(|(k1, _v1), (k2, _v2)| k1 == k2);
            temp_storage.make_datum(|packer| packer.push_dict(pairs))
        }
        (Datum::List(list_a), Datum::List(list_b)) => {
            let elems = list_a.iter().chain(list_b.iter());
            temp_storage.make_datum(|packer| packer.push_list(elems))
        }
        (Datum::List(list_a), b) => {
            let elems = list_a.iter().chain(Some(b));
            temp_storage.make_datum(|packer| packer.push_list(elems))
        }
        (a, Datum::List(list_b)) => {
            let elems = Some(a).into_iter().chain(list_b.iter());
            temp_storage.make_datum(|packer| packer.push_list(elems))
        }
        _ => Datum::Null,
    }
}

#[sqlfunc(
    output_type_expr = "ScalarType::Jsonb.nullable(true)",
    is_infix_op = true,
    sqlname = "-",
    propagates_nulls = true,
    introduces_nulls = true
)]
fn jsonb_delete_int64<'a>(a: Datum<'a>, b: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    let i = b.unwrap_int64();
    match a {
        Datum::List(list) => {
            let i = if i >= 0 {
                usize::cast_from(i.unsigned_abs())
            } else {
                // index backwards from the end
                let i = usize::cast_from(i.unsigned_abs());
                (list.iter().count()).wrapping_sub(i)
            };
            let elems = list
                .iter()
                .enumerate()
                .filter(|(i2, _e)| i != *i2)
                .map(|(_, e)| e);
            temp_storage.make_datum(|packer| packer.push_list(elems))
        }
        _ => Datum::Null,
    }
}

#[sqlfunc(
    output_type_expr = "ScalarType::Jsonb.nullable(true)",
    is_infix_op = true,
    sqlname = "-",
    propagates_nulls = true,
    introduces_nulls = true
)]
fn jsonb_delete_string<'a>(a: Datum<'a>, b: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    match a {
        Datum::List(list) => {
            let elems = list.iter().filter(|e| b != *e);
            temp_storage.make_datum(|packer| packer.push_list(elems))
        }
        Datum::Map(dict) => {
            let k = b.unwrap_str();
            let pairs = dict.iter().filter(|(k2, _v)| k != *k2);
            temp_storage.make_datum(|packer| packer.push_dict(pairs))
        }
        _ => Datum::Null,
    }
}

fn date_part_interval<'a, D>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError>
where
    D: DecimalLike + Into<Datum<'static>>,
{
    let units = a.unwrap_str();
    match units.parse() {
        Ok(units) => Ok(date_part_interval_inner::<D>(units, b.unwrap_interval())?.into()),
        Err(_) => Err(EvalError::UnknownUnits(units.into())),
    }
}

#[sqlfunc(
    output_type = "Numeric",
    sqlname = "extractiv",
    propagates_nulls = true,
    introduces_nulls = false
)]
fn date_part_interval_numeric<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let units = a.unwrap_str();
    match units.parse() {
        Ok(units) => Ok(date_part_interval_inner::<Numeric>(units, b.unwrap_interval())?.into()),
        Err(_) => Err(EvalError::UnknownUnits(units.into())),
    }
}

#[sqlfunc(
    output_type = "f64",
    sqlname = "date_partiv",
    propagates_nulls = true,
    introduces_nulls = false
)]
fn date_part_interval_f64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let units = a.unwrap_str();
    match units.parse() {
        Ok(units) => Ok(date_part_interval_inner::<f64>(units, b.unwrap_interval())?.into()),
        Err(_) => Err(EvalError::UnknownUnits(units.into())),
    }
}

fn date_part_time<'a, D>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError>
where
    D: DecimalLike + Into<Datum<'a>>,
{
    let units = a.unwrap_str();
    match units.parse() {
        Ok(units) => Ok(date_part_time_inner::<D>(units, b.unwrap_time())?.into()),
        Err(_) => Err(EvalError::UnknownUnits(units.into())),
    }
}

#[sqlfunc(
    output_type = "Numeric",
    sqlname = "extractt",
    propagates_nulls = true,
    introduces_nulls = false
)]
fn date_part_time_numeric<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let units = a.unwrap_str();
    match units.parse() {
        Ok(units) => Ok(date_part_time_inner::<Numeric>(units, b.unwrap_time())?.into()),
        Err(_) => Err(EvalError::UnknownUnits(units.into())),
    }
}

#[sqlfunc(
    output_type = "f64",
    sqlname = "date_partt",
    propagates_nulls = true,
    introduces_nulls = false
)]
fn date_part_time_f64<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let units = a.unwrap_str();
    match units.parse() {
        Ok(units) => Ok(date_part_time_inner::<f64>(units, b.unwrap_time())?.into()),
        Err(_) => Err(EvalError::UnknownUnits(units.into())),
    }
}

fn date_part_timestamp<'a, T, D>(a: Datum<'a>, ts: &T) -> Result<Datum<'a>, EvalError>
where
    T: TimestampLike,
    D: DecimalLike + Into<Datum<'a>>,
{
    let units = a.unwrap_str();
    match units.parse() {
        Ok(units) => Ok(date_part_timestamp_inner::<_, D>(units, ts)?.into()),
        Err(_) => Err(EvalError::UnknownUnits(units.into())),
    }
}

#[sqlfunc(
    output_type = "Numeric",
    sqlname = "extractts",
    propagates_nulls = true
)]
fn date_part_timestamp_timestamp_numeric<'a>(
    units: &str,
    ts: CheckedTimestamp<NaiveDateTime>,
) -> Result<Datum<'a>, EvalError> {
    match units.parse() {
        Ok(units) => Ok(date_part_timestamp_inner::<_, Numeric>(units, &*ts)?.into()),
        Err(_) => Err(EvalError::UnknownUnits(units.into())),
    }
}

#[sqlfunc(
    output_type = "Numeric",
    sqlname = "extracttstz",
    propagates_nulls = true
)]
fn date_part_timestamp_timestamp_tz_numeric<'a>(
    units: &str,
    ts: CheckedTimestamp<DateTime<Utc>>,
) -> Result<Datum<'a>, EvalError> {
    match units.parse() {
        Ok(units) => Ok(date_part_timestamp_inner::<_, Numeric>(units, &*ts)?.into()),
        Err(_) => Err(EvalError::UnknownUnits(units.into())),
    }
}

#[sqlfunc(sqlname = "date_partts", propagates_nulls = true)]
fn date_part_timestamp_timestamp_f64(
    units: &str,
    ts: CheckedTimestamp<NaiveDateTime>,
) -> Result<f64, EvalError> {
    match units.parse() {
        Ok(units) => date_part_timestamp_inner(units, &*ts),
        Err(_) => Err(EvalError::UnknownUnits(units.into())),
    }
}

#[sqlfunc(sqlname = "date_parttstz", propagates_nulls = true)]
fn date_part_timestamp_timestamp_tz_f64(
    units: &str,
    ts: CheckedTimestamp<DateTime<Utc>>,
) -> Result<f64, EvalError> {
    match units.parse() {
        Ok(units) => date_part_timestamp_inner(units, &*ts),
        Err(_) => Err(EvalError::UnknownUnits(units.into())),
    }
}

#[sqlfunc(output_type = "Numeric", sqlname = "extractd", propagates_nulls = true)]
fn extract_date_units<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let units = a.unwrap_str();
    match units.parse() {
        Ok(units) => Ok(extract_date_inner(units, b.unwrap_date().into())?.into()),
        Err(_) => Err(EvalError::UnknownUnits(units.into())),
    }
}

pub fn date_bin<'a, T>(
    stride: Interval,
    source: CheckedTimestamp<T>,
    origin: CheckedTimestamp<T>,
) -> Result<Datum<'a>, EvalError>
where
    T: TimestampLike,
{
    if stride.months != 0 {
        return Err(EvalError::DateBinOutOfRange(
            "timestamps cannot be binned into intervals containing months or years".into(),
        ));
    }

    let stride_ns = match stride.duration_as_chrono().num_nanoseconds() {
        Some(ns) if ns <= 0 => Err(EvalError::DateBinOutOfRange(
            "stride must be greater than zero".into(),
        )),
        Some(ns) => Ok(ns),
        None => Err(EvalError::DateBinOutOfRange(
            format!("stride cannot exceed {}/{} nanoseconds", i64::MAX, i64::MIN,).into(),
        )),
    }?;

    // Make sure the returned timestamp is at the start of the bin, even if the
    // origin is in the future. We do this here because `T` is not `Copy` and
    // gets moved by its subtraction operation.
    let sub_stride = origin > source;

    let tm_diff = (source - origin.clone()).num_nanoseconds().ok_or_else(|| {
        EvalError::DateBinOutOfRange(
            "source and origin must not differ more than 2^63 nanoseconds".into(),
        )
    })?;

    let mut tm_delta = tm_diff - tm_diff % stride_ns;

    if sub_stride {
        tm_delta -= stride_ns;
    }

    let res = origin
        .checked_add_signed(Duration::nanoseconds(tm_delta))
        .ok_or(EvalError::TimestampOutOfRange)?;
    Ok(res.try_into()?)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "CheckedTimestamp<NaiveDateTime>",
    sqlname = "bin_unix_epoch_timestamp",
    propagates_nulls = true
)]
fn date_bin_timestamp<'a>(
    stride: Interval,
    source: CheckedTimestamp<NaiveDateTime>,
) -> Result<Datum<'a>, EvalError> {
    let origin =
        CheckedTimestamp::from_timestamplike(DateTime::from_timestamp(0, 0).unwrap().naive_utc())
            .expect("must fit");
    date_bin(stride, source, origin)
}

#[sqlfunc(
    is_monotone = "(true, true)",
    output_type = "CheckedTimestamp<DateTime<Utc>>",
    sqlname = "bin_unix_epoch_timestamptz",
    propagates_nulls = true
)]
fn date_bin_timestamp_tz<'a>(
    stride: Interval,
    source: CheckedTimestamp<DateTime<Utc>>,
) -> Result<Datum<'a>, EvalError> {
    let origin = CheckedTimestamp::from_timestamplike(DateTime::from_timestamp(0, 0).unwrap())
        .expect("must fit");
    date_bin(stride, source, origin)
}

fn date_trunc<'a, T>(a: Datum<'a>, ts: &T) -> Result<Datum<'a>, EvalError>
where
    T: TimestampLike,
{
    let units = a.unwrap_str();
    match units.parse() {
        Ok(units) => Ok(date_trunc_inner(units, ts)?.try_into()?),
        Err(_) => Err(EvalError::UnknownUnits(units.into())),
    }
}

#[sqlfunc(sqlname = "date_truncts", propagates_nulls = true)]
fn date_trunc_units_timestamp(
    units: &str,
    ts: CheckedTimestamp<NaiveDateTime>,
) -> Result<CheckedTimestamp<NaiveDateTime>, EvalError> {
    match units.parse() {
        Ok(units) => Ok(date_trunc_inner(units, &*ts)?.try_into()?),
        Err(_) => Err(EvalError::UnknownUnits(units.into())),
    }
}

#[sqlfunc(sqlname = "date_trunctstz", propagates_nulls = true)]
fn date_trunc_units_timestamp_tz(
    units: &str,
    ts: CheckedTimestamp<DateTime<Utc>>,
) -> Result<CheckedTimestamp<DateTime<Utc>>, EvalError> {
    match units.parse() {
        Ok(units) => Ok(date_trunc_inner(units, &*ts)?.try_into()?),
        Err(_) => Err(EvalError::UnknownUnits(units.into())),
    }
}

#[sqlfunc(
    output_type = "Interval",
    sqlname = "date_trunciv",
    propagates_nulls = true
)]
fn date_trunc_interval<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mut interval = b.unwrap_interval();
    let units = a.unwrap_str();
    let dtf = units
        .parse()
        .map_err(|_| EvalError::UnknownUnits(units.into()))?;

    interval
        .truncate_low_fields(dtf, Some(0), RoundBehavior::Truncate)
        .expect(
            "truncate_low_fields should not fail with max_precision 0 and RoundBehavior::Truncate",
        );
    Ok(interval.into())
}

fn date_diff_timestamp<'a>(unit: Datum, a: Datum, b: Datum) -> Result<Datum<'a>, EvalError> {
    let unit = unit.unwrap_str();
    let unit = unit
        .parse()
        .map_err(|_| EvalError::InvalidDatePart(unit.into()))?;

    let a = a.unwrap_timestamp();
    let b = b.unwrap_timestamp();
    let diff = b.diff_as(&a, unit)?;

    Ok(Datum::Int64(diff))
}

fn date_diff_timestamptz<'a>(unit: Datum, a: Datum, b: Datum) -> Result<Datum<'a>, EvalError> {
    let unit = unit.unwrap_str();
    let unit = unit
        .parse()
        .map_err(|_| EvalError::InvalidDatePart(unit.into()))?;

    let a = a.unwrap_timestamptz();
    let b = b.unwrap_timestamptz();
    let diff = b.diff_as(&a, unit)?;

    Ok(Datum::Int64(diff))
}

fn date_diff_date<'a>(unit: Datum, a: Datum, b: Datum) -> Result<Datum<'a>, EvalError> {
    let unit = unit.unwrap_str();
    let unit = unit
        .parse()
        .map_err(|_| EvalError::InvalidDatePart(unit.into()))?;

    let a = a.unwrap_date();
    let b = b.unwrap_date();

    // Convert the Date into a timestamp so we can calculate age.
    let a_ts = CheckedTimestamp::try_from(NaiveDate::from(a).and_hms_opt(0, 0, 0).unwrap())?;
    let b_ts = CheckedTimestamp::try_from(NaiveDate::from(b).and_hms_opt(0, 0, 0).unwrap())?;
    let diff = b_ts.diff_as(&a_ts, unit)?;

    Ok(Datum::Int64(diff))
}

fn date_diff_time<'a>(unit: Datum, a: Datum, b: Datum) -> Result<Datum<'a>, EvalError> {
    let unit = unit.unwrap_str();
    let unit = unit
        .parse()
        .map_err(|_| EvalError::InvalidDatePart(unit.into()))?;

    let a = a.unwrap_time();
    let b = b.unwrap_time();

    // Convert the Time into a timestamp so we can calculate age.
    let a_ts =
        CheckedTimestamp::try_from(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap().and_time(a))?;
    let b_ts =
        CheckedTimestamp::try_from(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap().and_time(b))?;
    let diff = b_ts.diff_as(&a_ts, unit)?;

    Ok(Datum::Int64(diff))
}

/// Parses a named timezone like `EST` or `America/New_York`, or a fixed-offset timezone like `-05:00`.
///
/// The interpretation of fixed offsets depend on whether the POSIX or ISO 8601 standard is being
/// used.
pub(crate) fn parse_timezone(tz: &str, spec: TimezoneSpec) -> Result<Timezone, EvalError> {
    Timezone::parse(tz, spec).map_err(|_| EvalError::InvalidTimezone(tz.into()))
}

/// Converts the time datum `b`, which is assumed to be in UTC, to the timezone that the interval datum `a` is assumed
/// to represent. The interval is not allowed to hold months, but there are no limits on the amount of seconds.
/// The interval acts like a `chrono::FixedOffset`, without the `-86,400 < x < 86,400` limitation.
fn timezone_interval_time(a: Datum<'_>, b: Datum<'_>) -> Result<Datum<'static>, EvalError> {
    let interval = a.unwrap_interval();
    if interval.months != 0 {
        Err(EvalError::InvalidTimezoneInterval)
    } else {
        Ok(b.unwrap_time()
            .overflowing_add_signed(interval.duration_as_chrono())
            .0
            .into())
    }
}

/// Converts the timestamp datum `b`, which is assumed to be in the time of the timezone datum `a` to a timestamptz
/// in UTC. The interval is not allowed to hold months, but there are no limits on the amount of seconds.
/// The interval acts like a `chrono::FixedOffset`, without the `-86,400 < x < 86,400` limitation.
fn timezone_interval_timestamp(a: Datum<'_>, b: Datum<'_>) -> Result<Datum<'static>, EvalError> {
    let interval = a.unwrap_interval();
    if interval.months != 0 {
        Err(EvalError::InvalidTimezoneInterval)
    } else {
        match b
            .unwrap_timestamp()
            .checked_sub_signed(interval.duration_as_chrono())
        {
            Some(sub) => Ok(DateTime::from_naive_utc_and_offset(sub, Utc).try_into()?),
            None => Err(EvalError::TimestampOutOfRange),
        }
    }
}

/// Converts the UTC timestamptz datum `b`, to the local timestamp of the timezone datum `a`.
/// The interval is not allowed to hold months, but there are no limits on the amount of seconds.
/// The interval acts like a `chrono::FixedOffset`, without the `-86,400 < x < 86,400` limitation.
fn timezone_interval_timestamptz(a: Datum<'_>, b: Datum<'_>) -> Result<Datum<'static>, EvalError> {
    let interval = a.unwrap_interval();
    if interval.months != 0 {
        return Err(EvalError::InvalidTimezoneInterval);
    }
    match b
        .unwrap_timestamptz()
        .naive_utc()
        .checked_add_signed(interval.duration_as_chrono())
    {
        Some(dt) => Ok(dt.try_into()?),
        None => Err(EvalError::TimestampOutOfRange),
    }
}

#[sqlfunc(
    output_type_expr = r#"ScalarType::Record {
                fields: [
                    ("abbrev".into(), ScalarType::String.nullable(false)),
                    ("base_utc_offset".into(), ScalarType::Interval.nullable(false)),
                    ("dst_offset".into(), ScalarType::Interval.nullable(false)),
                ].into(),
                custom_id: None,
            }.nullable(true)"#,
    propagates_nulls = true,
    introduces_nulls = false
)]
fn timezone_offset<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let tz_str = a.unwrap_str();
    let tz = match Tz::from_str_insensitive(tz_str) {
        Ok(tz) => tz,
        Err(_) => return Err(EvalError::InvalidIanaTimezoneId(tz_str.into())),
    };
    let offset = tz.offset_from_utc_datetime(&b.unwrap_timestamptz().naive_utc());
    Ok(temp_storage.make_datum(|packer| {
        packer.push_list_with(|packer| {
            packer.push(Datum::from(offset.abbreviation()));
            packer.push(Datum::from(offset.base_utc_offset()));
            packer.push(Datum::from(offset.dst_offset()));
        });
    }))
}

/// Determines if an mz_aclitem contains one of the specified privileges. This will return true if
/// any of the listed privileges are contained in the mz_aclitem.
#[sqlfunc(
    sqlname = "mz_aclitem_contains_privilege",
    output_type = "bool",
    propagates_nulls = true
)]
fn mz_acl_item_contains_privilege<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let mz_acl_item = a.unwrap_mz_acl_item();
    let privileges = b.unwrap_str();
    let acl_mode = AclMode::parse_multiple_privileges(privileges)
        .map_err(|e: anyhow::Error| EvalError::InvalidPrivileges(e.to_string().into()))?;
    let contains = !mz_acl_item.acl_mode.intersection(acl_mode).is_empty();
    Ok(contains.into())
}

#[sqlfunc(
    output_type = "mz_repr::ArrayRustType<String>",
    propagates_nulls = true
)]
// transliterated from postgres/src/backend/utils/adt/misc.c
fn parse_ident<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    fn is_ident_start(c: char) -> bool {
        matches!(c, 'A'..='Z' | 'a'..='z' | '_' | '\u{80}'..=char::MAX)
    }

    fn is_ident_cont(c: char) -> bool {
        matches!(c, '0'..='9' | '$') || is_ident_start(c)
    }

    let ident = a.unwrap_str();
    let strict = b.unwrap_bool();

    let mut elems = vec![];
    let buf = &mut LexBuf::new(ident);

    let mut after_dot = false;

    buf.take_while(|ch| ch.is_ascii_whitespace());

    loop {
        let mut missing_ident = true;

        let c = buf.next();

        if c == Some('"') {
            let s = buf.take_while(|ch| !matches!(ch, '"'));

            if buf.next() != Some('"') {
                return Err(EvalError::InvalidIdentifier {
                    ident: ident.into(),
                    detail: Some("String has unclosed double quotes.".into()),
                });
            }
            elems.push(Datum::String(s));
            missing_ident = false;
        } else if c.map(is_ident_start).unwrap_or(false) {
            buf.prev();
            let s = buf.take_while(is_ident_cont);
            let s = temp_storage.push_string(s.to_ascii_lowercase());
            elems.push(Datum::String(s));
            missing_ident = false;
        }

        if missing_ident {
            if c == Some('.') {
                return Err(EvalError::InvalidIdentifier {
                    ident: ident.into(),
                    detail: Some("No valid identifier before \".\".".into()),
                });
            } else if after_dot {
                return Err(EvalError::InvalidIdentifier {
                    ident: ident.into(),
                    detail: Some("No valid identifier after \".\".".into()),
                });
            } else {
                return Err(EvalError::InvalidIdentifier {
                    ident: ident.into(),
                    detail: None,
                });
            }
        }

        buf.take_while(|ch| ch.is_ascii_whitespace());

        match buf.next() {
            Some('.') => {
                after_dot = true;

                buf.take_while(|ch| ch.is_ascii_whitespace());
            }
            Some(_) if strict => {
                return Err(EvalError::InvalidIdentifier {
                    ident: ident.into(),
                    detail: None,
                });
            }
            _ => break,
        }
    }

    Ok(temp_storage.try_make_datum(|packer| {
        packer.try_push_array(
            &[ArrayDimension {
                lower_bound: 1,
                length: elems.len(),
            }],
            elems,
        )
    })?)
}

fn string_to_array<'a>(
    string_datum: Datum<'a>,
    delimiter: Datum<'a>,
    null_string: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    if string_datum.is_null() {
        return Ok(Datum::Null);
    }

    let string = string_datum.unwrap_str();

    if string.is_empty() {
        let mut row = Row::default();
        let mut packer = row.packer();
        packer.try_push_array(&[], std::iter::empty::<Datum>())?;

        return Ok(temp_storage.push_unary_row(row));
    }

    if delimiter.is_null() {
        let split_all_chars_delimiter = "";
        return string_to_array_impl(string, split_all_chars_delimiter, null_string, temp_storage);
    }

    let delimiter = delimiter.unwrap_str();

    if delimiter.is_empty() {
        let mut row = Row::default();
        let mut packer = row.packer();
        packer.try_push_array(
            &[ArrayDimension {
                lower_bound: 1,
                length: 1,
            }],
            vec![string].into_iter().map(Datum::String),
        )?;

        Ok(temp_storage.push_unary_row(row))
    } else {
        string_to_array_impl(string, delimiter, null_string, temp_storage)
    }
}

fn string_to_array_impl<'a>(
    string: &str,
    delimiter: &str,
    null_string: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let mut row = Row::default();
    let mut packer = row.packer();

    let result = string.split(delimiter);
    let found: Vec<&str> = if delimiter.is_empty() {
        result.filter(|s| !s.is_empty()).collect()
    } else {
        result.collect()
    };
    let array_dimensions = [ArrayDimension {
        lower_bound: 1,
        length: found.len(),
    }];

    if null_string.is_null() {
        packer.try_push_array(&array_dimensions, found.into_iter().map(Datum::String))?;
    } else {
        let null_string = null_string.unwrap_str();
        let found_datums = found.into_iter().map(|chunk| {
            if chunk.eq(null_string) {
                Datum::Null
            } else {
                Datum::String(chunk)
            }
        });

        packer.try_push_array(&array_dimensions, found_datums)?;
    }

    Ok(temp_storage.push_unary_row(row))
}

fn regexp_split_to_array<'a>(
    text: Datum<'a>,
    regexp: Datum<'a>,
    flags: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let text = text.unwrap_str();
    let regexp = regexp.unwrap_str();
    let flags = flags.unwrap_str();
    let regexp = build_regex(regexp, flags)?;
    regexp_split_to_array_re(text, &regexp, temp_storage)
}

fn regexp_split_to_array_re<'a>(
    text: &str,
    regexp: &Regex,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let found = mz_regexp::regexp_split_to_array(text, regexp);
    let mut row = Row::default();
    let mut packer = row.packer();
    packer.try_push_array(
        &[ArrayDimension {
            lower_bound: 1,
            length: found.len(),
        }],
        found.into_iter().map(Datum::String),
    )?;
    Ok(temp_storage.push_unary_row(row))
}

#[sqlfunc(output_type = "String", propagates_nulls = true)]
fn pretty_sql<'a>(
    sql: Datum<'a>,
    width: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let sql = sql.unwrap_str();
    let width = width.unwrap_int32();
    let width =
        usize::try_from(width).map_err(|_| EvalError::PrettyError("invalid width".into()))?;
    let pretty = pretty_str(
        sql,
        PrettyConfig {
            width,
            format_mode: FormatMode::Simple,
        },
    )
    .map_err(|e| EvalError::PrettyError(e.to_string().into()))?;
    let pretty = temp_storage.push_string(pretty);
    Ok(Datum::String(pretty))
}

#[sqlfunc(output_type = "bool", propagates_nulls = true)]
fn starts_with<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let a = a.unwrap_str();
    let b = b.unwrap_str();
    Datum::from(a.starts_with(b))
}

#[derive(Ord, PartialOrd, Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash, MzReflect)]
pub enum BinaryFunc {
    AddInt16,
    AddInt32,
    AddInt64,
    AddUInt16,
    AddUInt32,
    AddUInt64,
    AddFloat32,
    AddFloat64,
    AddInterval,
    AddTimestampInterval,
    AddTimestampTzInterval,
    AddDateInterval,
    AddDateTime,
    AddTimeInterval,
    AddNumeric,
    AgeTimestamp,
    AgeTimestampTz,
    BitAndInt16,
    BitAndInt32,
    BitAndInt64,
    BitAndUInt16,
    BitAndUInt32,
    BitAndUInt64,
    BitOrInt16,
    BitOrInt32,
    BitOrInt64,
    BitOrUInt16,
    BitOrUInt32,
    BitOrUInt64,
    BitXorInt16,
    BitXorInt32,
    BitXorInt64,
    BitXorUInt16,
    BitXorUInt32,
    BitXorUInt64,
    BitShiftLeftInt16,
    BitShiftLeftInt32,
    BitShiftLeftInt64,
    BitShiftLeftUInt16,
    BitShiftLeftUInt32,
    BitShiftLeftUInt64,
    BitShiftRightInt16,
    BitShiftRightInt32,
    BitShiftRightInt64,
    BitShiftRightUInt16,
    BitShiftRightUInt32,
    BitShiftRightUInt64,
    SubInt16,
    SubInt32,
    SubInt64,
    SubUInt16,
    SubUInt32,
    SubUInt64,
    SubFloat32,
    SubFloat64,
    SubInterval,
    SubTimestamp,
    SubTimestampTz,
    SubTimestampInterval,
    SubTimestampTzInterval,
    SubDate,
    SubDateInterval,
    SubTime,
    SubTimeInterval,
    SubNumeric,
    MulInt16,
    MulInt32,
    MulInt64,
    MulUInt16,
    MulUInt32,
    MulUInt64,
    MulFloat32,
    MulFloat64,
    MulNumeric,
    MulInterval,
    DivInt16,
    DivInt32,
    DivInt64,
    DivUInt16,
    DivUInt32,
    DivUInt64,
    DivFloat32,
    DivFloat64,
    DivNumeric,
    DivInterval,
    ModInt16,
    ModInt32,
    ModInt64,
    ModUInt16,
    ModUInt32,
    ModUInt64,
    ModFloat32,
    ModFloat64,
    ModNumeric,
    RoundNumeric,
    Eq,
    NotEq,
    Lt,
    Lte,
    Gt,
    Gte,
    LikeEscape,
    IsLikeMatch { case_insensitive: bool },
    IsRegexpMatch { case_insensitive: bool },
    ToCharTimestamp,
    ToCharTimestampTz,
    DateBinTimestamp,
    DateBinTimestampTz,
    ExtractInterval,
    ExtractTime,
    ExtractTimestamp,
    ExtractTimestampTz,
    ExtractDate,
    DatePartInterval,
    DatePartTime,
    DatePartTimestamp,
    DatePartTimestampTz,
    DateTruncTimestamp,
    DateTruncTimestampTz,
    DateTruncInterval,
    TimezoneTimestamp,
    TimezoneTimestampTz,
    TimezoneIntervalTimestamp,
    TimezoneIntervalTimestampTz,
    TimezoneIntervalTime,
    TimezoneOffset,
    TextConcat,
    JsonbGetInt64,
    JsonbGetInt64Stringify,
    JsonbGetString,
    JsonbGetStringStringify,
    JsonbGetPath,
    JsonbGetPathStringify,
    JsonbContainsString,
    JsonbConcat,
    JsonbContainsJsonb,
    JsonbDeleteInt64,
    JsonbDeleteString,
    MapContainsKey,
    MapGetValue,
    MapContainsAllKeys,
    MapContainsAnyKeys,
    MapContainsMap,
    ConvertFrom,
    Left,
    Position,
    Right,
    RepeatString,
    Trim,
    TrimLeading,
    TrimTrailing,
    EncodedBytesCharLength,
    ListLengthMax { max_layer: usize },
    ArrayContains,
    ArrayContainsArray { rev: bool },
    ArrayLength,
    ArrayLower,
    ArrayRemove,
    ArrayUpper,
    ArrayArrayConcat,
    ListListConcat,
    ListElementConcat,
    ElementListConcat,
    ListRemove,
    ListContainsList { rev: bool },
    DigestString,
    DigestBytes,
    MzRenderTypmod,
    Encode,
    Decode,
    LogNumeric,
    Power,
    PowerNumeric,
    GetBit,
    GetByte,
    ConstantTimeEqBytes,
    ConstantTimeEqString,
    RangeContainsElem { elem_type: ScalarType, rev: bool },
    RangeContainsRange { rev: bool },
    RangeOverlaps,
    RangeAfter,
    RangeBefore,
    RangeOverleft,
    RangeOverright,
    RangeAdjacent,
    RangeUnion,
    RangeIntersection,
    RangeDifference,
    UuidGenerateV5,
    MzAclItemContainsPrivilege,
    ParseIdent,
    PrettySql,
    RegexpReplace { regex: Regex, limit: usize },
    StartsWith,
}

impl BinaryFunc {
    pub fn eval<'a>(
        &'a self,
        datums: &[Datum<'a>],
        temp_storage: &'a RowArena,
        a_expr: &'a MirScalarExpr,
        b_expr: &'a MirScalarExpr,
    ) -> Result<Datum<'a>, EvalError> {
        let a = a_expr.eval(datums, temp_storage)?;
        let b = b_expr.eval(datums, temp_storage)?;
        if self.propagates_nulls() && (a.is_null() || b.is_null()) {
            return Ok(Datum::Null);
        }
        match self {
            BinaryFunc::AddInt16 => add_int16(a, b),
            BinaryFunc::AddInt32 => add_int32(a, b),
            BinaryFunc::AddInt64 => add_int64(a, b),
            BinaryFunc::AddUInt16 => add_uint16(a, b),
            BinaryFunc::AddUInt32 => add_uint32(a, b),
            BinaryFunc::AddUInt64 => add_uint64(a, b),
            BinaryFunc::AddFloat32 => add_float32(a, b),
            BinaryFunc::AddFloat64 => add_float64(a, b),
            BinaryFunc::AddTimestampInterval => {
                add_timestamplike_interval(a.unwrap_timestamp(), b.unwrap_interval())
            }
            BinaryFunc::AddTimestampTzInterval => {
                add_timestamplike_interval(a.unwrap_timestamptz(), b.unwrap_interval())
            }
            BinaryFunc::AddDateTime => add_date_time(a, b),
            BinaryFunc::AddDateInterval => add_date_interval(a, b),
            BinaryFunc::AddTimeInterval => Ok(add_time_interval(a, b)),
            BinaryFunc::AddNumeric => add_numeric(a, b),
            BinaryFunc::AddInterval => add_interval(a, b),
            BinaryFunc::AgeTimestamp => age_timestamp(a, b),
            BinaryFunc::AgeTimestampTz => age_timestamptz(a, b),
            BinaryFunc::BitAndInt16 => Ok(bit_and_int16(a, b)),
            BinaryFunc::BitAndInt32 => Ok(bit_and_int32(a, b)),
            BinaryFunc::BitAndInt64 => Ok(bit_and_int64(a, b)),
            BinaryFunc::BitAndUInt16 => Ok(bit_and_uint16(a, b)),
            BinaryFunc::BitAndUInt32 => Ok(bit_and_uint32(a, b)),
            BinaryFunc::BitAndUInt64 => Ok(bit_and_uint64(a, b)),
            BinaryFunc::BitOrInt16 => Ok(bit_or_int16(a, b)),
            BinaryFunc::BitOrInt32 => Ok(bit_or_int32(a, b)),
            BinaryFunc::BitOrInt64 => Ok(bit_or_int64(a, b)),
            BinaryFunc::BitOrUInt16 => Ok(bit_or_uint16(a, b)),
            BinaryFunc::BitOrUInt32 => Ok(bit_or_uint32(a, b)),
            BinaryFunc::BitOrUInt64 => Ok(bit_or_uint64(a, b)),
            BinaryFunc::BitXorInt16 => Ok(bit_xor_int16(a, b)),
            BinaryFunc::BitXorInt32 => Ok(bit_xor_int32(a, b)),
            BinaryFunc::BitXorInt64 => Ok(bit_xor_int64(a, b)),
            BinaryFunc::BitXorUInt16 => Ok(bit_xor_uint16(a, b)),
            BinaryFunc::BitXorUInt32 => Ok(bit_xor_uint32(a, b)),
            BinaryFunc::BitXorUInt64 => Ok(bit_xor_uint64(a, b)),
            BinaryFunc::BitShiftLeftInt16 => Ok(bit_shift_left_int16(a, b)),
            BinaryFunc::BitShiftLeftInt32 => Ok(bit_shift_left_int32(a, b)),
            BinaryFunc::BitShiftLeftInt64 => Ok(bit_shift_left_int64(a, b)),
            BinaryFunc::BitShiftLeftUInt16 => Ok(bit_shift_left_uint16(a, b)),
            BinaryFunc::BitShiftLeftUInt32 => Ok(bit_shift_left_uint32(a, b)),
            BinaryFunc::BitShiftLeftUInt64 => Ok(bit_shift_left_uint64(a, b)),
            BinaryFunc::BitShiftRightInt16 => Ok(bit_shift_right_int16(a, b)),
            BinaryFunc::BitShiftRightInt32 => Ok(bit_shift_right_int32(a, b)),
            BinaryFunc::BitShiftRightInt64 => Ok(bit_shift_right_int64(a, b)),
            BinaryFunc::BitShiftRightUInt16 => Ok(bit_shift_right_uint16(a, b)),
            BinaryFunc::BitShiftRightUInt32 => Ok(bit_shift_right_uint32(a, b)),
            BinaryFunc::BitShiftRightUInt64 => Ok(bit_shift_right_uint64(a, b)),
            BinaryFunc::SubInt16 => sub_int16(a, b),
            BinaryFunc::SubInt32 => sub_int32(a, b),
            BinaryFunc::SubInt64 => sub_int64(a, b),
            BinaryFunc::SubUInt16 => sub_uint16(a, b),
            BinaryFunc::SubUInt32 => sub_uint32(a, b),
            BinaryFunc::SubUInt64 => sub_uint64(a, b),
            BinaryFunc::SubFloat32 => sub_float32(a, b),
            BinaryFunc::SubFloat64 => sub_float64(a, b),
            BinaryFunc::SubTimestamp => Ok(sub_timestamp(a, b)),
            BinaryFunc::SubTimestampTz => Ok(sub_timestamptz(a, b)),
            BinaryFunc::SubTimestampInterval => sub_timestamplike_interval(a.unwrap_timestamp(), b),
            BinaryFunc::SubTimestampTzInterval => {
                sub_timestamplike_interval(a.unwrap_timestamptz(), b)
            }
            BinaryFunc::SubInterval => sub_interval(a, b),
            BinaryFunc::SubDate => Ok(sub_date(a, b)),
            BinaryFunc::SubDateInterval => sub_date_interval(a, b),
            BinaryFunc::SubTime => Ok(sub_time(a, b)),
            BinaryFunc::SubTimeInterval => Ok(sub_time_interval(a, b)),
            BinaryFunc::SubNumeric => sub_numeric(a, b),
            BinaryFunc::MulInt16 => mul_int16(a, b),
            BinaryFunc::MulInt32 => mul_int32(a, b),
            BinaryFunc::MulInt64 => mul_int64(a, b),
            BinaryFunc::MulUInt16 => mul_uint16(a, b),
            BinaryFunc::MulUInt32 => mul_uint32(a, b),
            BinaryFunc::MulUInt64 => mul_uint64(a, b),
            BinaryFunc::MulFloat32 => mul_float32(a, b),
            BinaryFunc::MulFloat64 => mul_float64(a, b),
            BinaryFunc::MulNumeric => mul_numeric(a, b),
            BinaryFunc::MulInterval => mul_interval(a, b),
            BinaryFunc::DivInt16 => div_int16(a, b),
            BinaryFunc::DivInt32 => div_int32(a, b),
            BinaryFunc::DivInt64 => div_int64(a, b),
            BinaryFunc::DivUInt16 => div_uint16(a, b),
            BinaryFunc::DivUInt32 => div_uint32(a, b),
            BinaryFunc::DivUInt64 => div_uint64(a, b),
            BinaryFunc::DivFloat32 => div_float32(a, b),
            BinaryFunc::DivFloat64 => div_float64(a, b),
            BinaryFunc::DivNumeric => div_numeric(a, b),
            BinaryFunc::DivInterval => div_interval(a, b),
            BinaryFunc::ModInt16 => mod_int16(a, b),
            BinaryFunc::ModInt32 => mod_int32(a, b),
            BinaryFunc::ModInt64 => mod_int64(a, b),
            BinaryFunc::ModUInt16 => mod_uint16(a, b),
            BinaryFunc::ModUInt32 => mod_uint32(a, b),
            BinaryFunc::ModUInt64 => mod_uint64(a, b),
            BinaryFunc::ModFloat32 => mod_float32(a, b),
            BinaryFunc::ModFloat64 => mod_float64(a, b),
            BinaryFunc::ModNumeric => mod_numeric(a, b),
            BinaryFunc::Eq => Ok(eq(a, b)),
            BinaryFunc::NotEq => Ok(not_eq(a, b)),
            BinaryFunc::Lt => Ok(lt(a, b)),
            BinaryFunc::Lte => Ok(lte(a, b)),
            BinaryFunc::Gt => Ok(gt(a, b)),
            BinaryFunc::Gte => Ok(gte(a, b)),
            BinaryFunc::LikeEscape => like_escape(a, b, temp_storage),
            BinaryFunc::IsLikeMatch { case_insensitive } => {
                is_like_match_dynamic(a, b, *case_insensitive)
            }
            BinaryFunc::IsRegexpMatch { case_insensitive } => {
                is_regexp_match_dynamic(a, b, *case_insensitive)
            }
            BinaryFunc::ToCharTimestamp => Ok(to_char_timestamplike(
                a.unwrap_timestamp().deref(),
                b.unwrap_str(),
                temp_storage,
            )),
            BinaryFunc::ToCharTimestampTz => Ok(to_char_timestamplike(
                a.unwrap_timestamptz().deref(),
                b.unwrap_str(),
                temp_storage,
            )),
            BinaryFunc::DateBinTimestamp => date_bin(
                a.unwrap_interval(),
                b.unwrap_timestamp(),
                CheckedTimestamp::from_timestamplike(
                    DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
                )
                .expect("must fit"),
            ),
            BinaryFunc::DateBinTimestampTz => date_bin(
                a.unwrap_interval(),
                b.unwrap_timestamptz(),
                CheckedTimestamp::from_timestamplike(DateTime::from_timestamp(0, 0).unwrap())
                    .expect("must fit"),
            ),
            BinaryFunc::ExtractInterval => date_part_interval::<Numeric>(a, b),
            BinaryFunc::ExtractTime => date_part_time::<Numeric>(a, b),
            BinaryFunc::ExtractTimestamp => {
                date_part_timestamp::<_, Numeric>(a, b.unwrap_timestamp().deref())
            }
            BinaryFunc::ExtractTimestampTz => {
                date_part_timestamp::<_, Numeric>(a, b.unwrap_timestamptz().deref())
            }
            BinaryFunc::ExtractDate => extract_date_units(a, b),
            BinaryFunc::DatePartInterval => date_part_interval::<f64>(a, b),
            BinaryFunc::DatePartTime => date_part_time::<f64>(a, b),
            BinaryFunc::DatePartTimestamp => {
                date_part_timestamp::<_, f64>(a, b.unwrap_timestamp().deref())
            }
            BinaryFunc::DatePartTimestampTz => {
                date_part_timestamp::<_, f64>(a, b.unwrap_timestamptz().deref())
            }
            BinaryFunc::DateTruncTimestamp => date_trunc(a, b.unwrap_timestamp().deref()),
            BinaryFunc::DateTruncInterval => date_trunc_interval(a, b),
            BinaryFunc::DateTruncTimestampTz => date_trunc(a, b.unwrap_timestamptz().deref()),
            BinaryFunc::TimezoneTimestamp => parse_timezone(a.unwrap_str(), TimezoneSpec::Posix)
                .and_then(|tz| timezone_timestamp(tz, b.unwrap_timestamp().into()).map(Into::into)),
            BinaryFunc::TimezoneTimestampTz => parse_timezone(a.unwrap_str(), TimezoneSpec::Posix)
                .and_then(|tz| {
                    Ok(timezone_timestamptz(tz, b.unwrap_timestamptz().into())?.try_into()?)
                }),
            BinaryFunc::TimezoneIntervalTimestamp => timezone_interval_timestamp(a, b),
            BinaryFunc::TimezoneIntervalTimestampTz => timezone_interval_timestamptz(a, b),
            BinaryFunc::TimezoneIntervalTime => timezone_interval_time(a, b),
            BinaryFunc::TimezoneOffset => timezone_offset(a, b, temp_storage),
            BinaryFunc::TextConcat => Ok(text_concat_binary(a, b, temp_storage)),
            BinaryFunc::JsonbGetInt64 => Ok(jsonb_get_int64(a, b, temp_storage, false)),
            BinaryFunc::JsonbGetInt64Stringify => Ok(jsonb_get_int64(a, b, temp_storage, true)),
            BinaryFunc::JsonbGetString => Ok(jsonb_get_string(a, b, temp_storage, false)),
            BinaryFunc::JsonbGetStringStringify => Ok(jsonb_get_string(a, b, temp_storage, true)),
            BinaryFunc::JsonbGetPath => Ok(jsonb_get_path(a, b, temp_storage, false)),
            BinaryFunc::JsonbGetPathStringify => Ok(jsonb_get_path(a, b, temp_storage, true)),
            BinaryFunc::JsonbContainsString => Ok(jsonb_contains_string(a, b)),
            BinaryFunc::JsonbConcat => Ok(jsonb_concat(a, b, temp_storage)),
            BinaryFunc::JsonbContainsJsonb => Ok(jsonb_contains_jsonb(a, b)),
            BinaryFunc::JsonbDeleteInt64 => Ok(jsonb_delete_int64(a, b, temp_storage)),
            BinaryFunc::JsonbDeleteString => Ok(jsonb_delete_string(a, b, temp_storage)),
            BinaryFunc::MapContainsKey => Ok(map_contains_key(a, b)),
            BinaryFunc::MapGetValue => Ok(map_get_value(a, b)),
            BinaryFunc::MapContainsAllKeys => Ok(map_contains_all_keys(a, b)),
            BinaryFunc::MapContainsAnyKeys => Ok(map_contains_any_keys(a, b)),
            BinaryFunc::MapContainsMap => Ok(map_contains_map(a, b)),
            BinaryFunc::RoundNumeric => round_numeric_binary(a, b),
            BinaryFunc::ConvertFrom => convert_from(a, b),
            BinaryFunc::Encode => encode(a, b, temp_storage),
            BinaryFunc::Decode => decode(a, b, temp_storage),
            BinaryFunc::Left => left(a, b),
            BinaryFunc::Position => position(a, b),
            BinaryFunc::Right => right(a, b),
            BinaryFunc::Trim => Ok(trim(a, b)),
            BinaryFunc::TrimLeading => Ok(trim_leading(a, b)),
            BinaryFunc::TrimTrailing => Ok(trim_trailing(a, b)),
            BinaryFunc::EncodedBytesCharLength => encoded_bytes_char_length(a, b),
            BinaryFunc::ListLengthMax { max_layer } => list_length_max(a, b, *max_layer),
            BinaryFunc::ArrayLength => array_length(a, b),
            BinaryFunc::ArrayContains => Ok(array_contains(a, b)),
            BinaryFunc::ArrayContainsArray { rev: false } => Ok(array_contains_array(a, b)),
            BinaryFunc::ArrayContainsArray { rev: true } => Ok(array_contains_array(b, a)),
            BinaryFunc::ArrayLower => Ok(array_lower(a, b)),
            BinaryFunc::ArrayRemove => array_remove(a, b, temp_storage),
            BinaryFunc::ArrayUpper => array_upper(a, b),
            BinaryFunc::ArrayArrayConcat => array_array_concat(a, b, temp_storage),
            BinaryFunc::ListListConcat => Ok(list_list_concat(a, b, temp_storage)),
            BinaryFunc::ListElementConcat => Ok(list_element_concat(a, b, temp_storage)),
            BinaryFunc::ElementListConcat => Ok(element_list_concat(a, b, temp_storage)),
            BinaryFunc::ListRemove => Ok(list_remove(a, b, temp_storage)),
            BinaryFunc::ListContainsList { rev: false } => Ok(list_contains_list(a, b)),
            BinaryFunc::ListContainsList { rev: true } => Ok(list_contains_list(b, a)),
            BinaryFunc::DigestString => digest_string(a, b, temp_storage),
            BinaryFunc::DigestBytes => digest_bytes(a, b, temp_storage),
            BinaryFunc::MzRenderTypmod => mz_render_typmod(a, b, temp_storage),
            BinaryFunc::LogNumeric => log_base_numeric(a, b),
            BinaryFunc::Power => power(a, b),
            BinaryFunc::PowerNumeric => power_numeric(a, b),
            BinaryFunc::RepeatString => repeat_string(a, b, temp_storage),
            BinaryFunc::GetBit => get_bit(a, b),
            BinaryFunc::GetByte => get_byte(a, b),
            BinaryFunc::ConstantTimeEqBytes => constant_time_eq_bytes(a, b),
            BinaryFunc::ConstantTimeEqString => constant_time_eq_string(a, b),
            BinaryFunc::RangeContainsElem { elem_type, rev: _ } => Ok(match elem_type {
                ScalarType::Int32 => contains_range_elem::<i32>(a, b),
                ScalarType::Int64 => contains_range_elem::<i64>(a, b),
                ScalarType::Date => contains_range_elem::<Date>(a, b),
                ScalarType::Numeric { .. } => contains_range_elem::<OrderedDecimal<Numeric>>(a, b),
                ScalarType::Timestamp { .. } => {
                    contains_range_elem::<CheckedTimestamp<NaiveDateTime>>(a, b)
                }
                ScalarType::TimestampTz { .. } => {
                    contains_range_elem::<CheckedTimestamp<DateTime<Utc>>>(a, b)
                }
                _ => unreachable!(),
            }),
            BinaryFunc::RangeContainsRange { rev: false } => Ok(range_contains_range(a, b)),
            BinaryFunc::RangeContainsRange { rev: true } => Ok(range_contains_range_rev(a, b)),
            BinaryFunc::RangeOverlaps => Ok(range_overlaps(a, b)),
            BinaryFunc::RangeAfter => Ok(range_after(a, b)),
            BinaryFunc::RangeBefore => Ok(range_before(a, b)),
            BinaryFunc::RangeOverleft => Ok(range_overleft(a, b)),
            BinaryFunc::RangeOverright => Ok(range_overright(a, b)),
            BinaryFunc::RangeAdjacent => Ok(range_adjacent(a, b)),
            BinaryFunc::RangeUnion => range_union(a, b, temp_storage),
            BinaryFunc::RangeIntersection => range_intersection(a, b, temp_storage),
            BinaryFunc::RangeDifference => range_difference(a, b, temp_storage),
            BinaryFunc::UuidGenerateV5 => Ok(uuid_generate_v5(a, b)),
            BinaryFunc::MzAclItemContainsPrivilege => mz_acl_item_contains_privilege(a, b),
            BinaryFunc::ParseIdent => parse_ident(a, b, temp_storage),
            BinaryFunc::PrettySql => pretty_sql(a, b, temp_storage),
            BinaryFunc::RegexpReplace { regex, limit } => {
                regexp_replace_static(a, b, regex, *limit, temp_storage)
            }
            BinaryFunc::StartsWith => Ok(starts_with(a, b)),
        }
    }

    pub fn output_type(&self, input1_type: ColumnType, input2_type: ColumnType) -> ColumnType {
        use BinaryFunc::*;
        let in_nullable = input1_type.nullable || input2_type.nullable;
        match self {
            Eq
            | NotEq
            | Lt
            | Lte
            | Gt
            | Gte
            | ArrayContains
            | ArrayContainsArray { .. }
            // like and regexp produce errors on invalid like-strings or regexes
            | IsLikeMatch { .. }
            | IsRegexpMatch { .. } => ScalarType::Bool.nullable(in_nullable),

            ToCharTimestamp | ToCharTimestampTz | ConvertFrom | Left | Right | Trim
            | TrimLeading | TrimTrailing | LikeEscape => ScalarType::String.nullable(in_nullable),

            AddInt16 | SubInt16 | MulInt16 | DivInt16 | ModInt16 | BitAndInt16 | BitOrInt16
            | BitXorInt16 | BitShiftLeftInt16 | BitShiftRightInt16 => {
                ScalarType::Int16.nullable(in_nullable)
            }

            AddInt32
            | SubInt32
            | MulInt32
            | DivInt32
            | ModInt32
            | BitAndInt32
            | BitOrInt32
            | BitXorInt32
            | BitShiftLeftInt32
            | BitShiftRightInt32
            | EncodedBytesCharLength
            | SubDate => ScalarType::Int32.nullable(in_nullable),

            AddInt64 | SubInt64 | MulInt64 | DivInt64 | ModInt64 | BitAndInt64 | BitOrInt64
            | BitXorInt64 | BitShiftLeftInt64 | BitShiftRightInt64 => {
                ScalarType::Int64.nullable(in_nullable)
            }

            AddUInt16 | SubUInt16 | MulUInt16 | DivUInt16 | ModUInt16 | BitAndUInt16
            | BitOrUInt16 | BitXorUInt16 | BitShiftLeftUInt16 | BitShiftRightUInt16 => {
                ScalarType::UInt16.nullable(in_nullable)
            }

            AddUInt32 | SubUInt32 | MulUInt32 | DivUInt32 | ModUInt32 | BitAndUInt32
            | BitOrUInt32 | BitXorUInt32 | BitShiftLeftUInt32 | BitShiftRightUInt32 => {
                ScalarType::UInt32.nullable(in_nullable)
            }

            AddUInt64 | SubUInt64 | MulUInt64 | DivUInt64 | ModUInt64 | BitAndUInt64
            | BitOrUInt64 | BitXorUInt64 | BitShiftLeftUInt64 | BitShiftRightUInt64 => {
                ScalarType::UInt64.nullable(in_nullable)
            }

            AddFloat32 | SubFloat32 | MulFloat32 | DivFloat32 | ModFloat32 => {
                ScalarType::Float32.nullable(in_nullable)
            }

            AddFloat64 | SubFloat64 | MulFloat64 | DivFloat64 | ModFloat64 => {
                ScalarType::Float64.nullable(in_nullable)
            }

            AddInterval | SubInterval | SubTimestamp | SubTimestampTz | MulInterval
            | DivInterval => ScalarType::Interval.nullable(in_nullable),

            AgeTimestamp | AgeTimestampTz => ScalarType::Interval.nullable(in_nullable),

            AddTimestampInterval
            | SubTimestampInterval
            | AddTimestampTzInterval
            | SubTimestampTzInterval
            | AddTimeInterval
            | SubTimeInterval => input1_type.nullable(in_nullable),

            AddDateInterval | SubDateInterval | AddDateTime | DateBinTimestamp
            | DateTruncTimestamp => ScalarType::Timestamp { precision: None }.nullable(in_nullable),

            DateTruncInterval => ScalarType::Interval.nullable(in_nullable),

            TimezoneTimestampTz | TimezoneIntervalTimestampTz => {
                ScalarType::Timestamp { precision: None }.nullable(in_nullable)
            }

            ExtractInterval | ExtractTime | ExtractTimestamp | ExtractTimestampTz | ExtractDate => {
                ScalarType::Numeric { max_scale: None }.nullable(in_nullable)
            }

            DatePartInterval | DatePartTime | DatePartTimestamp | DatePartTimestampTz => {
                ScalarType::Float64.nullable(in_nullable)
            }

            DateBinTimestampTz | DateTruncTimestampTz => ScalarType::TimestampTz { precision: None }.nullable(in_nullable),

            TimezoneTimestamp | TimezoneIntervalTimestamp => {
                ScalarType::TimestampTz { precision: None }.nullable(in_nullable)
            }

            TimezoneIntervalTime => ScalarType::Time.nullable(in_nullable),

            TimezoneOffset => ScalarType::Record {
                fields: [
                    ("abbrev".into(), ScalarType::String.nullable(false)),
                    ("base_utc_offset".into(), ScalarType::Interval.nullable(false)),
                    ("dst_offset".into(), ScalarType::Interval.nullable(false)),
                ].into(),
                custom_id: None,
            }.nullable(true),

            SubTime => ScalarType::Interval.nullable(in_nullable),

            MzRenderTypmod | TextConcat => ScalarType::String.nullable(in_nullable),

            JsonbGetInt64Stringify
            | JsonbGetStringStringify
            | JsonbGetPathStringify => ScalarType::String.nullable(true),

            JsonbGetInt64
            | JsonbGetString
            | JsonbGetPath
            | JsonbConcat
            | JsonbDeleteInt64
            | JsonbDeleteString => ScalarType::Jsonb.nullable(true),

            JsonbContainsString | JsonbContainsJsonb | MapContainsKey | MapContainsAllKeys
            | MapContainsAnyKeys | MapContainsMap => ScalarType::Bool.nullable(in_nullable),

            MapGetValue => input1_type
                .scalar_type
                .unwrap_map_value_type()
                .clone()
                .nullable(true),

            ArrayLength | ArrayLower | ArrayUpper => ScalarType::Int32.nullable(true),

            ListLengthMax { .. } => ScalarType::Int32.nullable(true),

            ArrayArrayConcat | ArrayRemove | ListListConcat | ListElementConcat | ListRemove => {
                input1_type.scalar_type.without_modifiers().nullable(true)
            }

            ElementListConcat => input2_type.scalar_type.without_modifiers().nullable(true),

            ListContainsList { .. } =>  ScalarType::Bool.nullable(in_nullable),

            DigestString | DigestBytes => ScalarType::Bytes.nullable(in_nullable),
            Position => ScalarType::Int32.nullable(in_nullable),
            Encode => ScalarType::String.nullable(in_nullable),
            Decode => ScalarType::Bytes.nullable(in_nullable),
            Power => ScalarType::Float64.nullable(in_nullable),
            RepeatString => input1_type.scalar_type.nullable(in_nullable),

            AddNumeric | DivNumeric | LogNumeric | ModNumeric | MulNumeric | PowerNumeric
            | RoundNumeric | SubNumeric => {
                ScalarType::Numeric { max_scale: None }.nullable(in_nullable)
            }

            GetBit => ScalarType::Int32.nullable(in_nullable),
            GetByte => ScalarType::Int32.nullable(in_nullable),

            ConstantTimeEqBytes | ConstantTimeEqString => {
                ScalarType::Bool.nullable(in_nullable)
            },

            UuidGenerateV5 => ScalarType::Uuid.nullable(in_nullable),

            RangeContainsElem { .. }
            | RangeContainsRange { .. }
            | RangeOverlaps
            | RangeAfter
            | RangeBefore
            | RangeOverleft
            | RangeOverright
            | RangeAdjacent => ScalarType::Bool.nullable(in_nullable),

            RangeUnion | RangeIntersection | RangeDifference => {
                soft_assert_eq_or_log!(
                    input1_type.scalar_type.without_modifiers(),
                    input2_type.scalar_type.without_modifiers()
                );
                input1_type.scalar_type.without_modifiers().nullable(true)
            }

            MzAclItemContainsPrivilege => ScalarType::Bool.nullable(in_nullable),

            ParseIdent => ScalarType::Array(Box::new(ScalarType::String)).nullable(in_nullable),
            PrettySql => ScalarType::String.nullable(in_nullable),
            RegexpReplace { .. } => ScalarType::String.nullable(in_nullable),

            StartsWith => ScalarType::Bool.nullable(in_nullable),
        }
    }

    /// Whether the function output is NULL if any of its inputs are NULL.
    pub fn propagates_nulls(&self) -> bool {
        // NOTE: The following is a list of the binary functions
        // that **DO NOT** propagate nulls.
        !matches!(
            self,
            BinaryFunc::ArrayArrayConcat
                | BinaryFunc::ListListConcat
                | BinaryFunc::ListElementConcat
                | BinaryFunc::ElementListConcat
                | BinaryFunc::ArrayRemove
                | BinaryFunc::ListRemove
        )
    }

    /// Whether the function might return NULL even if none of its inputs are
    /// NULL.
    ///
    /// This is presently conservative, and may indicate that a function
    /// introduces nulls even when it does not.
    pub fn introduces_nulls(&self) -> bool {
        use BinaryFunc::*;
        match self {
            AddInt16
            | AddInt32
            | AddInt64
            | AddUInt16
            | AddUInt32
            | AddUInt64
            | AddFloat32
            | AddFloat64
            | AddInterval
            | AddTimestampInterval
            | AddTimestampTzInterval
            | AddDateInterval
            | AddDateTime
            | AddTimeInterval
            | AddNumeric
            | AgeTimestamp
            | AgeTimestampTz
            | BitAndInt16
            | BitAndInt32
            | BitAndInt64
            | BitAndUInt16
            | BitAndUInt32
            | BitAndUInt64
            | BitOrInt16
            | BitOrInt32
            | BitOrInt64
            | BitOrUInt16
            | BitOrUInt32
            | BitOrUInt64
            | BitXorInt16
            | BitXorInt32
            | BitXorInt64
            | BitXorUInt16
            | BitXorUInt32
            | BitXorUInt64
            | BitShiftLeftInt16
            | BitShiftLeftInt32
            | BitShiftLeftInt64
            | BitShiftLeftUInt16
            | BitShiftLeftUInt32
            | BitShiftLeftUInt64
            | BitShiftRightInt16
            | BitShiftRightInt32
            | BitShiftRightInt64
            | BitShiftRightUInt16
            | BitShiftRightUInt32
            | BitShiftRightUInt64
            | SubInt16
            | SubInt32
            | SubInt64
            | SubUInt16
            | SubUInt32
            | SubUInt64
            | SubFloat32
            | SubFloat64
            | SubInterval
            | SubTimestamp
            | SubTimestampTz
            | SubTimestampInterval
            | SubTimestampTzInterval
            | SubDate
            | SubDateInterval
            | SubTime
            | SubTimeInterval
            | SubNumeric
            | MulInt16
            | MulInt32
            | MulInt64
            | MulUInt16
            | MulUInt32
            | MulUInt64
            | MulFloat32
            | MulFloat64
            | MulNumeric
            | MulInterval
            | DivInt16
            | DivInt32
            | DivInt64
            | DivUInt16
            | DivUInt32
            | DivUInt64
            | DivFloat32
            | DivFloat64
            | DivNumeric
            | DivInterval
            | ModInt16
            | ModInt32
            | ModInt64
            | ModUInt16
            | ModUInt32
            | ModUInt64
            | ModFloat32
            | ModFloat64
            | ModNumeric
            | RoundNumeric
            | Eq
            | NotEq
            | Lt
            | Lte
            | Gt
            | Gte
            | LikeEscape
            | IsLikeMatch { .. }
            | IsRegexpMatch { .. }
            | ToCharTimestamp
            | ToCharTimestampTz
            | ConstantTimeEqBytes
            | ConstantTimeEqString
            | DateBinTimestamp
            | DateBinTimestampTz
            | ExtractInterval
            | ExtractTime
            | ExtractTimestamp
            | ExtractTimestampTz
            | ExtractDate
            | DatePartInterval
            | DatePartTime
            | DatePartTimestamp
            | DatePartTimestampTz
            | DateTruncTimestamp
            | DateTruncTimestampTz
            | DateTruncInterval
            | TimezoneTimestamp
            | TimezoneTimestampTz
            | TimezoneIntervalTimestamp
            | TimezoneIntervalTimestampTz
            | TimezoneIntervalTime
            | TimezoneOffset
            | TextConcat
            | JsonbContainsString
            | JsonbContainsJsonb
            | MapContainsKey
            | MapContainsAllKeys
            | MapContainsAnyKeys
            | MapContainsMap
            | ConvertFrom
            | Left
            | Position
            | Right
            | RepeatString
            | Trim
            | TrimLeading
            | TrimTrailing
            | EncodedBytesCharLength
            | ArrayContains
            | ArrayRemove
            | ArrayContainsArray { .. }
            | ArrayArrayConcat
            | ListListConcat
            | ListElementConcat
            | ElementListConcat
            | ListContainsList { .. }
            | ListRemove
            | DigestString
            | DigestBytes
            | MzRenderTypmod
            | Encode
            | Decode
            | LogNumeric
            | Power
            | PowerNumeric
            | GetBit
            | GetByte
            | RangeContainsElem { .. }
            | RangeContainsRange { .. }
            | RangeOverlaps
            | RangeAfter
            | RangeBefore
            | RangeOverleft
            | RangeOverright
            | RangeAdjacent
            | RangeUnion
            | RangeIntersection
            | RangeDifference
            | UuidGenerateV5
            | MzAclItemContainsPrivilege
            | ParseIdent
            | PrettySql
            | RegexpReplace { .. }
            | StartsWith => false,

            JsonbGetInt64
            | JsonbGetInt64Stringify
            | JsonbGetString
            | JsonbGetStringStringify
            | JsonbGetPath
            | JsonbGetPathStringify
            | JsonbConcat
            | JsonbDeleteInt64
            | JsonbDeleteString
            | MapGetValue
            | ListLengthMax { .. }
            | ArrayLength
            | ArrayLower
            | ArrayUpper => true,
        }
    }

    pub fn is_infix_op(&self) -> bool {
        use BinaryFunc::*;
        match self {
            AddInt16
            | AddInt32
            | AddInt64
            | AddUInt16
            | AddUInt32
            | AddUInt64
            | AddFloat32
            | AddFloat64
            | AddTimestampInterval
            | AddTimestampTzInterval
            | AddDateTime
            | AddDateInterval
            | AddTimeInterval
            | AddInterval
            | BitAndInt16
            | BitAndInt32
            | BitAndInt64
            | BitAndUInt16
            | BitAndUInt32
            | BitAndUInt64
            | BitOrInt16
            | BitOrInt32
            | BitOrInt64
            | BitOrUInt16
            | BitOrUInt32
            | BitOrUInt64
            | BitXorInt16
            | BitXorInt32
            | BitXorInt64
            | BitXorUInt16
            | BitXorUInt32
            | BitXorUInt64
            | BitShiftLeftInt16
            | BitShiftLeftInt32
            | BitShiftLeftInt64
            | BitShiftLeftUInt16
            | BitShiftLeftUInt32
            | BitShiftLeftUInt64
            | BitShiftRightInt16
            | BitShiftRightInt32
            | BitShiftRightInt64
            | BitShiftRightUInt16
            | BitShiftRightUInt32
            | BitShiftRightUInt64
            | SubInterval
            | MulInterval
            | DivInterval
            | AddNumeric
            | SubInt16
            | SubInt32
            | SubInt64
            | SubUInt16
            | SubUInt32
            | SubUInt64
            | SubFloat32
            | SubFloat64
            | SubTimestamp
            | SubTimestampTz
            | SubTimestampInterval
            | SubTimestampTzInterval
            | SubDate
            | SubDateInterval
            | SubTime
            | SubTimeInterval
            | SubNumeric
            | MulInt16
            | MulInt32
            | MulInt64
            | MulUInt16
            | MulUInt32
            | MulUInt64
            | MulFloat32
            | MulFloat64
            | MulNumeric
            | DivInt16
            | DivInt32
            | DivInt64
            | DivUInt16
            | DivUInt32
            | DivUInt64
            | DivFloat32
            | DivFloat64
            | DivNumeric
            | ModInt16
            | ModInt32
            | ModInt64
            | ModUInt16
            | ModUInt32
            | ModUInt64
            | ModFloat32
            | ModFloat64
            | ModNumeric
            | Eq
            | NotEq
            | Lt
            | Lte
            | Gt
            | Gte
            | JsonbConcat
            | JsonbContainsJsonb
            | JsonbGetInt64
            | JsonbGetInt64Stringify
            | JsonbGetString
            | JsonbGetStringStringify
            | JsonbGetPath
            | JsonbGetPathStringify
            | JsonbContainsString
            | JsonbDeleteInt64
            | JsonbDeleteString
            | MapContainsKey
            | MapGetValue
            | MapContainsAllKeys
            | MapContainsAnyKeys
            | MapContainsMap
            | TextConcat
            | IsLikeMatch { .. }
            | IsRegexpMatch { .. }
            | ArrayContains
            | ArrayContainsArray { .. }
            | ArrayLength
            | ArrayLower
            | ArrayUpper
            | ArrayArrayConcat
            | ListListConcat
            | ListElementConcat
            | ElementListConcat
            | ListContainsList { .. }
            | RangeContainsElem { .. }
            | RangeContainsRange { .. }
            | RangeOverlaps
            | RangeAfter
            | RangeBefore
            | RangeOverleft
            | RangeOverright
            | RangeAdjacent
            | RangeUnion
            | RangeIntersection
            | RangeDifference => true,
            ToCharTimestamp
            | ToCharTimestampTz
            | AgeTimestamp
            | AgeTimestampTz
            | DateBinTimestamp
            | DateBinTimestampTz
            | ExtractInterval
            | ExtractTime
            | ExtractTimestamp
            | ExtractTimestampTz
            | ExtractDate
            | DatePartInterval
            | DatePartTime
            | DatePartTimestamp
            | DatePartTimestampTz
            | DateTruncInterval
            | DateTruncTimestamp
            | DateTruncTimestampTz
            | TimezoneTimestamp
            | TimezoneTimestampTz
            | TimezoneIntervalTimestamp
            | TimezoneIntervalTimestampTz
            | TimezoneIntervalTime
            | TimezoneOffset
            | RoundNumeric
            | ConvertFrom
            | Left
            | Position
            | Right
            | Trim
            | TrimLeading
            | TrimTrailing
            | EncodedBytesCharLength
            | ListLengthMax { .. }
            | DigestString
            | DigestBytes
            | MzRenderTypmod
            | Encode
            | Decode
            | LogNumeric
            | Power
            | PowerNumeric
            | RepeatString
            | ArrayRemove
            | ListRemove
            | LikeEscape
            | UuidGenerateV5
            | GetBit
            | GetByte
            | MzAclItemContainsPrivilege
            | ConstantTimeEqBytes
            | ConstantTimeEqString
            | ParseIdent
            | PrettySql
            | RegexpReplace { .. }
            | StartsWith => false,
        }
    }

    /// Returns the negation of the given binary function, if it exists.
    pub fn negate(&self) -> Option<Self> {
        match self {
            BinaryFunc::Eq => Some(BinaryFunc::NotEq),
            BinaryFunc::NotEq => Some(BinaryFunc::Eq),
            BinaryFunc::Lt => Some(BinaryFunc::Gte),
            BinaryFunc::Gte => Some(BinaryFunc::Lt),
            BinaryFunc::Gt => Some(BinaryFunc::Lte),
            BinaryFunc::Lte => Some(BinaryFunc::Gt),
            _ => None,
        }
    }

    /// Returns true if the function could introduce an error on non-error inputs.
    pub fn could_error(&self) -> bool {
        match self {
            BinaryFunc::Eq
            | BinaryFunc::NotEq
            | BinaryFunc::Lt
            | BinaryFunc::Gte
            | BinaryFunc::Gt
            | BinaryFunc::Lte => false,
            BinaryFunc::BitAndInt16
            | BinaryFunc::BitAndInt32
            | BinaryFunc::BitAndInt64
            | BinaryFunc::BitAndUInt16
            | BinaryFunc::BitAndUInt32
            | BinaryFunc::BitAndUInt64
            | BinaryFunc::BitOrInt16
            | BinaryFunc::BitOrInt32
            | BinaryFunc::BitOrInt64
            | BinaryFunc::BitOrUInt16
            | BinaryFunc::BitOrUInt32
            | BinaryFunc::BitOrUInt64
            | BinaryFunc::BitXorInt16
            | BinaryFunc::BitXorInt32
            | BinaryFunc::BitXorInt64
            | BinaryFunc::BitXorUInt16
            | BinaryFunc::BitXorUInt32
            | BinaryFunc::BitXorUInt64
            | BinaryFunc::BitShiftLeftInt16
            | BinaryFunc::BitShiftLeftInt32
            | BinaryFunc::BitShiftLeftInt64
            | BinaryFunc::BitShiftLeftUInt16
            | BinaryFunc::BitShiftLeftUInt32
            | BinaryFunc::BitShiftLeftUInt64
            | BinaryFunc::BitShiftRightInt16
            | BinaryFunc::BitShiftRightInt32
            | BinaryFunc::BitShiftRightInt64
            | BinaryFunc::BitShiftRightUInt16
            | BinaryFunc::BitShiftRightUInt32
            | BinaryFunc::BitShiftRightUInt64 => false,
            BinaryFunc::JsonbGetInt64
            | BinaryFunc::JsonbGetInt64Stringify
            | BinaryFunc::JsonbGetString
            | BinaryFunc::JsonbGetStringStringify
            | BinaryFunc::JsonbGetPath
            | BinaryFunc::JsonbGetPathStringify
            | BinaryFunc::JsonbContainsString
            | BinaryFunc::JsonbConcat
            | BinaryFunc::JsonbContainsJsonb
            | BinaryFunc::JsonbDeleteInt64
            | BinaryFunc::JsonbDeleteString => false,
            BinaryFunc::MapContainsKey
            | BinaryFunc::MapGetValue
            | BinaryFunc::MapContainsAllKeys
            | BinaryFunc::MapContainsAnyKeys
            | BinaryFunc::MapContainsMap => false,
            BinaryFunc::AddTimeInterval
            | BinaryFunc::SubTimestamp
            | BinaryFunc::SubTimestampTz
            | BinaryFunc::SubDate
            | BinaryFunc::SubTime
            | BinaryFunc::SubTimeInterval
            | BinaryFunc::UuidGenerateV5
            | BinaryFunc::RangeContainsRange { .. }
            | BinaryFunc::RangeContainsElem { .. }
            | BinaryFunc::RangeOverlaps
            | BinaryFunc::RangeAfter
            | BinaryFunc::RangeBefore
            | BinaryFunc::RangeOverleft
            | BinaryFunc::RangeOverright
            | BinaryFunc::RangeAdjacent
            | BinaryFunc::ArrayLower
            | BinaryFunc::ArrayContains
            | BinaryFunc::ArrayContainsArray { rev: _ }
            | BinaryFunc::ListListConcat
            | BinaryFunc::ListElementConcat
            | BinaryFunc::ElementListConcat
            | BinaryFunc::ListRemove
            | BinaryFunc::ToCharTimestamp
            | BinaryFunc::ToCharTimestampTz
            | BinaryFunc::ListContainsList { rev: _ }
            | BinaryFunc::Trim
            | BinaryFunc::TrimLeading
            | BinaryFunc::TrimTrailing
            | BinaryFunc::TextConcat
            | BinaryFunc::StartsWith => false,

            _ => true,
        }
    }

    /// Returns true if the function is monotone. (Non-strict; either increasing or decreasing.)
    /// Monotone functions map ranges to ranges: ie. given a range of possible inputs, we can
    /// determine the range of possible outputs just by mapping the endpoints.
    ///
    /// This describes the *pointwise* behaviour of the function:
    /// ie. the behaviour of any specific argument as the others are held constant. (For example, `a - b` is
    /// monotone in the first argument because for any particular value of `b`, increasing `a` will
    /// always cause the result to increase... and in the second argument because for any specific `a`,
    /// increasing `b` will always cause the result to _decrease_.)
    ///
    /// This property describes the behaviour of the function over ranges where the function is defined:
    /// ie. the arguments and the result are non-error datums.
    pub fn is_monotone(&self) -> (bool, bool) {
        match self {
            BinaryFunc::AddInt16
            | BinaryFunc::AddInt32
            | BinaryFunc::AddInt64
            | BinaryFunc::AddUInt16
            | BinaryFunc::AddUInt32
            | BinaryFunc::AddUInt64
            | BinaryFunc::AddFloat32
            | BinaryFunc::AddFloat64
            | BinaryFunc::AddInterval
            | BinaryFunc::AddTimestampInterval
            | BinaryFunc::AddTimestampTzInterval
            | BinaryFunc::AddDateInterval
            | BinaryFunc::AddDateTime
            | BinaryFunc::AddTimeInterval
            | BinaryFunc::AddNumeric => (true, true),
            BinaryFunc::BitAndInt16
            | BinaryFunc::BitAndInt32
            | BinaryFunc::BitAndInt64
            | BinaryFunc::BitAndUInt16
            | BinaryFunc::BitAndUInt32
            | BinaryFunc::BitAndUInt64
            | BinaryFunc::BitOrInt16
            | BinaryFunc::BitOrInt32
            | BinaryFunc::BitOrInt64
            | BinaryFunc::BitOrUInt16
            | BinaryFunc::BitOrUInt32
            | BinaryFunc::BitOrUInt64
            | BinaryFunc::BitXorInt16
            | BinaryFunc::BitXorInt32
            | BinaryFunc::BitXorInt64
            | BinaryFunc::BitXorUInt16
            | BinaryFunc::BitXorUInt32
            | BinaryFunc::BitXorUInt64 => (false, false),
            // The shift functions wrap, which means they are monotonic in neither argument.
            BinaryFunc::BitShiftLeftInt16
            | BinaryFunc::BitShiftLeftInt32
            | BinaryFunc::BitShiftLeftInt64
            | BinaryFunc::BitShiftLeftUInt16
            | BinaryFunc::BitShiftLeftUInt32
            | BinaryFunc::BitShiftLeftUInt64
            | BinaryFunc::BitShiftRightInt16
            | BinaryFunc::BitShiftRightInt32
            | BinaryFunc::BitShiftRightInt64
            | BinaryFunc::BitShiftRightUInt16
            | BinaryFunc::BitShiftRightUInt32
            | BinaryFunc::BitShiftRightUInt64 => (false, false),
            BinaryFunc::SubInt16
            | BinaryFunc::SubInt32
            | BinaryFunc::SubInt64
            | BinaryFunc::SubUInt16
            | BinaryFunc::SubUInt32
            | BinaryFunc::SubUInt64
            | BinaryFunc::SubFloat32
            | BinaryFunc::SubFloat64
            | BinaryFunc::SubInterval
            | BinaryFunc::SubTimestamp
            | BinaryFunc::SubTimestampTz
            | BinaryFunc::SubTimestampInterval
            | BinaryFunc::SubTimestampTzInterval
            | BinaryFunc::SubDate
            | BinaryFunc::SubDateInterval
            | BinaryFunc::SubTime
            | BinaryFunc::SubTimeInterval
            | BinaryFunc::SubNumeric => (true, true),
            BinaryFunc::MulInt16
            | BinaryFunc::MulInt32
            | BinaryFunc::MulInt64
            | BinaryFunc::MulUInt16
            | BinaryFunc::MulUInt32
            | BinaryFunc::MulUInt64
            | BinaryFunc::MulFloat32
            | BinaryFunc::MulFloat64
            | BinaryFunc::MulNumeric
            | BinaryFunc::MulInterval => (true, true),
            BinaryFunc::DivInt16
            | BinaryFunc::DivInt32
            | BinaryFunc::DivInt64
            | BinaryFunc::DivUInt16
            | BinaryFunc::DivUInt32
            | BinaryFunc::DivUInt64
            | BinaryFunc::DivFloat32
            | BinaryFunc::DivFloat64
            | BinaryFunc::DivNumeric
            | BinaryFunc::DivInterval => (true, false),
            BinaryFunc::ModInt16
            | BinaryFunc::ModInt32
            | BinaryFunc::ModInt64
            | BinaryFunc::ModUInt16
            | BinaryFunc::ModUInt32
            | BinaryFunc::ModUInt64
            | BinaryFunc::ModFloat32
            | BinaryFunc::ModFloat64
            | BinaryFunc::ModNumeric => (false, false),
            BinaryFunc::RoundNumeric => (true, false),
            BinaryFunc::Eq | BinaryFunc::NotEq => (false, false),
            BinaryFunc::Lt | BinaryFunc::Lte | BinaryFunc::Gt | BinaryFunc::Gte => (true, true),
            BinaryFunc::LikeEscape
            | BinaryFunc::IsLikeMatch { .. }
            | BinaryFunc::IsRegexpMatch { .. } => (false, false),
            BinaryFunc::ToCharTimestamp | BinaryFunc::ToCharTimestampTz => (false, false),
            BinaryFunc::DateBinTimestamp | BinaryFunc::DateBinTimestampTz => (true, true),
            BinaryFunc::AgeTimestamp | BinaryFunc::AgeTimestampTz => (true, true),
            // Text concatenation is monotonic in its second argument, because if I change the
            // second argument but don't change the first argument, then we won't find a difference
            // in that part of the concatenation result that came from the first argument, so we'll
            // find the difference that comes from changing the second argument.
            // (It's not monotonic in its first argument, because e.g.,
            // 'A' < 'AA' but 'AZ' > 'AAZ'.)
            BinaryFunc::TextConcat => (false, true),
            // `left` is unfortunately not monotonic (at least for negative second arguments),
            // because 'aa' < 'z', but `left(_, -1)` makes 'a' > ''.
            BinaryFunc::Left => (false, false),
            // TODO: can these ever be treated as monotone? It's safe to treat the unary versions
            // as monotone in some cases, but only when extracting specific parts.
            BinaryFunc::ExtractInterval
            | BinaryFunc::ExtractTime
            | BinaryFunc::ExtractTimestamp
            | BinaryFunc::ExtractTimestampTz
            | BinaryFunc::ExtractDate => (false, false),
            BinaryFunc::DatePartInterval
            | BinaryFunc::DatePartTime
            | BinaryFunc::DatePartTimestamp
            | BinaryFunc::DatePartTimestampTz => (false, false),
            BinaryFunc::DateTruncTimestamp
            | BinaryFunc::DateTruncTimestampTz
            | BinaryFunc::DateTruncInterval => (false, false),
            BinaryFunc::TimezoneTimestamp
            | BinaryFunc::TimezoneTimestampTz
            | BinaryFunc::TimezoneIntervalTimestamp
            | BinaryFunc::TimezoneIntervalTimestampTz
            | BinaryFunc::TimezoneIntervalTime
            | BinaryFunc::TimezoneOffset => (false, false),
            BinaryFunc::JsonbGetInt64
            | BinaryFunc::JsonbGetInt64Stringify
            | BinaryFunc::JsonbGetString
            | BinaryFunc::JsonbGetStringStringify
            | BinaryFunc::JsonbGetPath
            | BinaryFunc::JsonbGetPathStringify
            | BinaryFunc::JsonbContainsString
            | BinaryFunc::JsonbConcat
            | BinaryFunc::JsonbContainsJsonb
            | BinaryFunc::JsonbDeleteInt64
            | BinaryFunc::JsonbDeleteString
            | BinaryFunc::MapContainsKey
            | BinaryFunc::MapGetValue
            | BinaryFunc::MapContainsAllKeys
            | BinaryFunc::MapContainsAnyKeys
            | BinaryFunc::MapContainsMap => (false, false),
            BinaryFunc::ConvertFrom
            | BinaryFunc::Position
            | BinaryFunc::Right
            | BinaryFunc::RepeatString
            | BinaryFunc::Trim
            | BinaryFunc::TrimLeading
            | BinaryFunc::TrimTrailing
            | BinaryFunc::EncodedBytesCharLength
            | BinaryFunc::ListLengthMax { .. }
            | BinaryFunc::ArrayContains
            | BinaryFunc::ArrayContainsArray { .. }
            | BinaryFunc::ArrayLength
            | BinaryFunc::ArrayLower
            | BinaryFunc::ArrayRemove
            | BinaryFunc::ArrayUpper
            | BinaryFunc::ArrayArrayConcat
            | BinaryFunc::ListListConcat
            | BinaryFunc::ListElementConcat
            | BinaryFunc::ElementListConcat
            | BinaryFunc::ListContainsList { .. }
            | BinaryFunc::ListRemove
            | BinaryFunc::DigestString
            | BinaryFunc::DigestBytes
            | BinaryFunc::MzRenderTypmod
            | BinaryFunc::Encode
            | BinaryFunc::Decode => (false, false),
            // TODO: it may be safe to treat these as monotone.
            BinaryFunc::LogNumeric | BinaryFunc::Power | BinaryFunc::PowerNumeric => (false, false),
            BinaryFunc::GetBit
            | BinaryFunc::GetByte
            | BinaryFunc::RangeContainsElem { .. }
            | BinaryFunc::RangeContainsRange { .. }
            | BinaryFunc::RangeOverlaps
            | BinaryFunc::RangeAfter
            | BinaryFunc::RangeBefore
            | BinaryFunc::RangeOverleft
            | BinaryFunc::RangeOverright
            | BinaryFunc::RangeAdjacent
            | BinaryFunc::RangeUnion
            | BinaryFunc::RangeIntersection
            | BinaryFunc::RangeDifference => (false, false),
            BinaryFunc::UuidGenerateV5 => (false, false),
            BinaryFunc::MzAclItemContainsPrivilege => (false, false),
            BinaryFunc::ParseIdent => (false, false),
            BinaryFunc::ConstantTimeEqBytes | BinaryFunc::ConstantTimeEqString => (false, false),
            BinaryFunc::PrettySql => (false, false),
            BinaryFunc::RegexpReplace { .. } => (false, false),
            BinaryFunc::StartsWith => (false, false),
        }
    }
}

impl fmt::Display for BinaryFunc {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            BinaryFunc::AddInt16 => f.write_str("+"),
            BinaryFunc::AddInt32 => f.write_str("+"),
            BinaryFunc::AddInt64 => f.write_str("+"),
            BinaryFunc::AddUInt16 => f.write_str("+"),
            BinaryFunc::AddUInt32 => f.write_str("+"),
            BinaryFunc::AddUInt64 => f.write_str("+"),
            BinaryFunc::AddFloat32 => f.write_str("+"),
            BinaryFunc::AddFloat64 => f.write_str("+"),
            BinaryFunc::AddNumeric => f.write_str("+"),
            BinaryFunc::AddInterval => f.write_str("+"),
            BinaryFunc::AddTimestampInterval => f.write_str("+"),
            BinaryFunc::AddTimestampTzInterval => f.write_str("+"),
            BinaryFunc::AddDateTime => f.write_str("+"),
            BinaryFunc::AddDateInterval => f.write_str("+"),
            BinaryFunc::AddTimeInterval => f.write_str("+"),
            BinaryFunc::AgeTimestamp => f.write_str("age"),
            BinaryFunc::AgeTimestampTz => f.write_str("age"),
            BinaryFunc::BitAndInt16 => f.write_str("&"),
            BinaryFunc::BitAndInt32 => f.write_str("&"),
            BinaryFunc::BitAndInt64 => f.write_str("&"),
            BinaryFunc::BitAndUInt16 => f.write_str("&"),
            BinaryFunc::BitAndUInt32 => f.write_str("&"),
            BinaryFunc::BitAndUInt64 => f.write_str("&"),
            BinaryFunc::BitOrInt16 => f.write_str("|"),
            BinaryFunc::BitOrInt32 => f.write_str("|"),
            BinaryFunc::BitOrInt64 => f.write_str("|"),
            BinaryFunc::BitOrUInt16 => f.write_str("|"),
            BinaryFunc::BitOrUInt32 => f.write_str("|"),
            BinaryFunc::BitOrUInt64 => f.write_str("|"),
            BinaryFunc::BitXorInt16 => f.write_str("#"),
            BinaryFunc::BitXorInt32 => f.write_str("#"),
            BinaryFunc::BitXorInt64 => f.write_str("#"),
            BinaryFunc::BitXorUInt16 => f.write_str("#"),
            BinaryFunc::BitXorUInt32 => f.write_str("#"),
            BinaryFunc::BitXorUInt64 => f.write_str("#"),
            BinaryFunc::BitShiftLeftInt16 => f.write_str("<<"),
            BinaryFunc::BitShiftLeftInt32 => f.write_str("<<"),
            BinaryFunc::BitShiftLeftInt64 => f.write_str("<<"),
            BinaryFunc::BitShiftLeftUInt16 => f.write_str("<<"),
            BinaryFunc::BitShiftLeftUInt32 => f.write_str("<<"),
            BinaryFunc::BitShiftLeftUInt64 => f.write_str("<<"),
            BinaryFunc::BitShiftRightInt16 => f.write_str(">>"),
            BinaryFunc::BitShiftRightInt32 => f.write_str(">>"),
            BinaryFunc::BitShiftRightInt64 => f.write_str(">>"),
            BinaryFunc::BitShiftRightUInt16 => f.write_str(">>"),
            BinaryFunc::BitShiftRightUInt32 => f.write_str(">>"),
            BinaryFunc::BitShiftRightUInt64 => f.write_str(">>"),
            BinaryFunc::SubInt16 => f.write_str("-"),
            BinaryFunc::SubInt32 => f.write_str("-"),
            BinaryFunc::SubInt64 => f.write_str("-"),
            BinaryFunc::SubUInt16 => f.write_str("-"),
            BinaryFunc::SubUInt32 => f.write_str("-"),
            BinaryFunc::SubUInt64 => f.write_str("-"),
            BinaryFunc::SubFloat32 => f.write_str("-"),
            BinaryFunc::SubFloat64 => f.write_str("-"),
            BinaryFunc::SubNumeric => f.write_str("-"),
            BinaryFunc::SubInterval => f.write_str("-"),
            BinaryFunc::SubTimestamp => f.write_str("-"),
            BinaryFunc::SubTimestampTz => f.write_str("-"),
            BinaryFunc::SubTimestampInterval => f.write_str("-"),
            BinaryFunc::SubTimestampTzInterval => f.write_str("-"),
            BinaryFunc::SubDate => f.write_str("-"),
            BinaryFunc::SubDateInterval => f.write_str("-"),
            BinaryFunc::SubTime => f.write_str("-"),
            BinaryFunc::SubTimeInterval => f.write_str("-"),
            BinaryFunc::MulInt16 => f.write_str("*"),
            BinaryFunc::MulInt32 => f.write_str("*"),
            BinaryFunc::MulInt64 => f.write_str("*"),
            BinaryFunc::MulUInt16 => f.write_str("*"),
            BinaryFunc::MulUInt32 => f.write_str("*"),
            BinaryFunc::MulUInt64 => f.write_str("*"),
            BinaryFunc::MulFloat32 => f.write_str("*"),
            BinaryFunc::MulFloat64 => f.write_str("*"),
            BinaryFunc::MulNumeric => f.write_str("*"),
            BinaryFunc::MulInterval => f.write_str("*"),
            BinaryFunc::DivInt16 => f.write_str("/"),
            BinaryFunc::DivInt32 => f.write_str("/"),
            BinaryFunc::DivInt64 => f.write_str("/"),
            BinaryFunc::DivUInt16 => f.write_str("/"),
            BinaryFunc::DivUInt32 => f.write_str("/"),
            BinaryFunc::DivUInt64 => f.write_str("/"),
            BinaryFunc::DivFloat32 => f.write_str("/"),
            BinaryFunc::DivFloat64 => f.write_str("/"),
            BinaryFunc::DivNumeric => f.write_str("/"),
            BinaryFunc::DivInterval => f.write_str("/"),
            BinaryFunc::ModInt16 => f.write_str("%"),
            BinaryFunc::ModInt32 => f.write_str("%"),
            BinaryFunc::ModInt64 => f.write_str("%"),
            BinaryFunc::ModUInt16 => f.write_str("%"),
            BinaryFunc::ModUInt32 => f.write_str("%"),
            BinaryFunc::ModUInt64 => f.write_str("%"),
            BinaryFunc::ModFloat32 => f.write_str("%"),
            BinaryFunc::ModFloat64 => f.write_str("%"),
            BinaryFunc::ModNumeric => f.write_str("%"),
            BinaryFunc::Eq => f.write_str("="),
            BinaryFunc::NotEq => f.write_str("!="),
            BinaryFunc::Lt => f.write_str("<"),
            BinaryFunc::Lte => f.write_str("<="),
            BinaryFunc::Gt => f.write_str(">"),
            BinaryFunc::Gte => f.write_str(">="),
            BinaryFunc::LikeEscape => f.write_str("like_escape"),
            BinaryFunc::IsLikeMatch {
                case_insensitive: false,
            } => f.write_str("like"),
            BinaryFunc::IsLikeMatch {
                case_insensitive: true,
            } => f.write_str("ilike"),
            BinaryFunc::IsRegexpMatch {
                case_insensitive: false,
            } => f.write_str("~"),
            BinaryFunc::IsRegexpMatch {
                case_insensitive: true,
            } => f.write_str("~*"),
            BinaryFunc::ToCharTimestamp => f.write_str("tocharts"),
            BinaryFunc::ToCharTimestampTz => f.write_str("tochartstz"),
            BinaryFunc::DateBinTimestamp => f.write_str("bin_unix_epoch_timestamp"),
            BinaryFunc::DateBinTimestampTz => f.write_str("bin_unix_epoch_timestamptz"),
            BinaryFunc::ExtractInterval => f.write_str("extractiv"),
            BinaryFunc::ExtractTime => f.write_str("extractt"),
            BinaryFunc::ExtractTimestamp => f.write_str("extractts"),
            BinaryFunc::ExtractTimestampTz => f.write_str("extracttstz"),
            BinaryFunc::ExtractDate => f.write_str("extractd"),
            BinaryFunc::DatePartInterval => f.write_str("date_partiv"),
            BinaryFunc::DatePartTime => f.write_str("date_partt"),
            BinaryFunc::DatePartTimestamp => f.write_str("date_partts"),
            BinaryFunc::DatePartTimestampTz => f.write_str("date_parttstz"),
            BinaryFunc::DateTruncTimestamp => f.write_str("date_truncts"),
            BinaryFunc::DateTruncInterval => f.write_str("date_trunciv"),
            BinaryFunc::DateTruncTimestampTz => f.write_str("date_trunctstz"),
            BinaryFunc::TimezoneTimestamp => f.write_str("timezonets"),
            BinaryFunc::TimezoneTimestampTz => f.write_str("timezonetstz"),
            BinaryFunc::TimezoneIntervalTimestamp => f.write_str("timezoneits"),
            BinaryFunc::TimezoneIntervalTimestampTz => f.write_str("timezoneitstz"),
            BinaryFunc::TimezoneIntervalTime => f.write_str("timezoneit"),
            BinaryFunc::TimezoneOffset => f.write_str("timezone_offset"),
            BinaryFunc::TextConcat => f.write_str("||"),
            BinaryFunc::JsonbGetInt64 => f.write_str("->"),
            BinaryFunc::JsonbGetInt64Stringify => f.write_str("->>"),
            BinaryFunc::JsonbGetString => f.write_str("->"),
            BinaryFunc::JsonbGetStringStringify => f.write_str("->>"),
            BinaryFunc::JsonbGetPath => f.write_str("#>"),
            BinaryFunc::JsonbGetPathStringify => f.write_str("#>>"),
            BinaryFunc::JsonbContainsString | BinaryFunc::MapContainsKey => f.write_str("?"),
            BinaryFunc::JsonbConcat => f.write_str("||"),
            BinaryFunc::JsonbContainsJsonb | BinaryFunc::MapContainsMap => f.write_str("@>"),
            BinaryFunc::JsonbDeleteInt64 => f.write_str("-"),
            BinaryFunc::JsonbDeleteString => f.write_str("-"),
            BinaryFunc::MapGetValue => f.write_str("->"),
            BinaryFunc::MapContainsAllKeys => f.write_str("?&"),
            BinaryFunc::MapContainsAnyKeys => f.write_str("?|"),
            BinaryFunc::RoundNumeric => f.write_str("round"),
            BinaryFunc::ConvertFrom => f.write_str("convert_from"),
            BinaryFunc::Left => f.write_str("left"),
            BinaryFunc::Position => f.write_str("position"),
            BinaryFunc::Right => f.write_str("right"),
            BinaryFunc::Trim => f.write_str("btrim"),
            BinaryFunc::TrimLeading => f.write_str("ltrim"),
            BinaryFunc::TrimTrailing => f.write_str("rtrim"),
            BinaryFunc::EncodedBytesCharLength => f.write_str("length"),
            BinaryFunc::ListLengthMax { .. } => f.write_str("list_length_max"),
            BinaryFunc::ArrayContains => f.write_str("array_contains"),
            BinaryFunc::ArrayContainsArray { rev } => f.write_str(if *rev { "<@" } else { "@>" }),
            BinaryFunc::ArrayLength => f.write_str("array_length"),
            BinaryFunc::ArrayLower => f.write_str("array_lower"),
            BinaryFunc::ArrayRemove => f.write_str("array_remove"),
            BinaryFunc::ArrayUpper => f.write_str("array_upper"),
            BinaryFunc::ArrayArrayConcat => f.write_str("||"),
            BinaryFunc::ListListConcat => f.write_str("||"),
            BinaryFunc::ListElementConcat => f.write_str("||"),
            BinaryFunc::ElementListConcat => f.write_str("||"),
            BinaryFunc::ListRemove => f.write_str("list_remove"),
            BinaryFunc::ListContainsList { rev } => f.write_str(if *rev { "<@" } else { "@>" }),
            BinaryFunc::DigestString | BinaryFunc::DigestBytes => f.write_str("digest"),
            BinaryFunc::MzRenderTypmod => f.write_str("mz_render_typmod"),
            BinaryFunc::Encode => f.write_str("encode"),
            BinaryFunc::Decode => f.write_str("decode"),
            BinaryFunc::LogNumeric => f.write_str("log"),
            BinaryFunc::Power => f.write_str("power"),
            BinaryFunc::PowerNumeric => f.write_str("power_numeric"),
            BinaryFunc::RepeatString => f.write_str("repeat"),
            BinaryFunc::GetBit => f.write_str("get_bit"),
            BinaryFunc::GetByte => f.write_str("get_byte"),
            BinaryFunc::ConstantTimeEqBytes => f.write_str("constant_time_compare_bytes"),
            BinaryFunc::ConstantTimeEqString => f.write_str("constant_time_compare_strings"),
            BinaryFunc::RangeContainsElem { rev, .. } => {
                f.write_str(if *rev { "<@" } else { "@>" })
            }
            BinaryFunc::RangeContainsRange { rev, .. } => {
                f.write_str(if *rev { "<@" } else { "@>" })
            }
            BinaryFunc::RangeOverlaps => f.write_str("&&"),
            BinaryFunc::RangeAfter => f.write_str(">>"),
            BinaryFunc::RangeBefore => f.write_str("<<"),
            BinaryFunc::RangeOverleft => f.write_str("&<"),
            BinaryFunc::RangeOverright => f.write_str("&>"),
            BinaryFunc::RangeAdjacent => f.write_str("-|-"),
            BinaryFunc::RangeUnion => f.write_str("+"),
            BinaryFunc::RangeIntersection => f.write_str("*"),
            BinaryFunc::RangeDifference => f.write_str("-"),
            BinaryFunc::UuidGenerateV5 => f.write_str("uuid_generate_v5"),
            BinaryFunc::MzAclItemContainsPrivilege => f.write_str("mz_aclitem_contains_privilege"),
            BinaryFunc::ParseIdent => f.write_str("parse_ident"),
            BinaryFunc::PrettySql => f.write_str("pretty_sql"),
            BinaryFunc::RegexpReplace { regex, limit } => write!(
                f,
                "regexp_replace[{}, case_insensitive={}, limit={}]",
                regex.pattern().escaped(),
                regex.case_insensitive,
                limit
            ),
            BinaryFunc::StartsWith => f.write_str("starts_with"),
        }
    }
}

/// An explicit [`Arbitrary`] implementation needed here because of a known
/// `proptest` issue.
///
/// Revert to the derive-macro impementation once the issue[^1] is fixed.
///
/// [^1]: <https://github.com/AltSysrq/proptest/issues/152>
impl Arbitrary for BinaryFunc {
    type Parameters = ();

    type Strategy = Union<BoxedStrategy<Self>>;

    fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
        Union::new(vec![
            Just(BinaryFunc::AddInt16).boxed(),
            Just(BinaryFunc::AddInt32).boxed(),
            Just(BinaryFunc::AddInt64).boxed(),
            Just(BinaryFunc::AddUInt16).boxed(),
            Just(BinaryFunc::AddUInt32).boxed(),
            Just(BinaryFunc::AddUInt64).boxed(),
            Just(BinaryFunc::AddFloat32).boxed(),
            Just(BinaryFunc::AddFloat64).boxed(),
            Just(BinaryFunc::AddInterval).boxed(),
            Just(BinaryFunc::AddTimestampInterval).boxed(),
            Just(BinaryFunc::AddTimestampTzInterval).boxed(),
            Just(BinaryFunc::AddDateInterval).boxed(),
            Just(BinaryFunc::AddDateTime).boxed(),
            Just(BinaryFunc::AddTimeInterval).boxed(),
            Just(BinaryFunc::AddNumeric).boxed(),
            Just(BinaryFunc::AgeTimestamp).boxed(),
            Just(BinaryFunc::AgeTimestampTz).boxed(),
            Just(BinaryFunc::BitAndInt16).boxed(),
            Just(BinaryFunc::BitAndInt32).boxed(),
            Just(BinaryFunc::BitAndInt64).boxed(),
            Just(BinaryFunc::BitAndUInt16).boxed(),
            Just(BinaryFunc::BitAndUInt32).boxed(),
            Just(BinaryFunc::BitAndUInt64).boxed(),
            Just(BinaryFunc::BitOrInt16).boxed(),
            Just(BinaryFunc::BitOrInt32).boxed(),
            Just(BinaryFunc::BitOrInt64).boxed(),
            Just(BinaryFunc::BitOrUInt16).boxed(),
            Just(BinaryFunc::BitOrUInt32).boxed(),
            Just(BinaryFunc::BitOrUInt64).boxed(),
            Just(BinaryFunc::BitXorInt16).boxed(),
            Just(BinaryFunc::BitXorInt32).boxed(),
            Just(BinaryFunc::BitXorInt64).boxed(),
            Just(BinaryFunc::BitXorUInt16).boxed(),
            Just(BinaryFunc::BitXorUInt32).boxed(),
            Just(BinaryFunc::BitXorUInt64).boxed(),
            Just(BinaryFunc::BitShiftLeftInt16).boxed(),
            Just(BinaryFunc::BitShiftLeftInt32).boxed(),
            Just(BinaryFunc::BitShiftLeftInt64).boxed(),
            Just(BinaryFunc::BitShiftLeftUInt16).boxed(),
            Just(BinaryFunc::BitShiftLeftUInt32).boxed(),
            Just(BinaryFunc::BitShiftLeftUInt64).boxed(),
            Just(BinaryFunc::BitShiftRightInt16).boxed(),
            Just(BinaryFunc::BitShiftRightInt32).boxed(),
            Just(BinaryFunc::BitShiftRightInt64).boxed(),
            Just(BinaryFunc::BitShiftRightUInt16).boxed(),
            Just(BinaryFunc::BitShiftRightUInt32).boxed(),
            Just(BinaryFunc::BitShiftRightUInt64).boxed(),
            Just(BinaryFunc::SubInt16).boxed(),
            Just(BinaryFunc::SubInt32).boxed(),
            Just(BinaryFunc::SubInt64).boxed(),
            Just(BinaryFunc::SubUInt16).boxed(),
            Just(BinaryFunc::SubUInt32).boxed(),
            Just(BinaryFunc::SubUInt64).boxed(),
            Just(BinaryFunc::SubFloat32).boxed(),
            Just(BinaryFunc::SubFloat64).boxed(),
            Just(BinaryFunc::SubInterval).boxed(),
            Just(BinaryFunc::SubTimestamp).boxed(),
            Just(BinaryFunc::SubTimestampTz).boxed(),
            Just(BinaryFunc::SubTimestampInterval).boxed(),
            Just(BinaryFunc::SubTimestampTzInterval).boxed(),
            Just(BinaryFunc::SubDate).boxed(),
            Just(BinaryFunc::SubDateInterval).boxed(),
            Just(BinaryFunc::SubTime).boxed(),
            Just(BinaryFunc::SubTimeInterval).boxed(),
            Just(BinaryFunc::SubNumeric).boxed(),
            Just(BinaryFunc::MulInt16).boxed(),
            Just(BinaryFunc::MulInt32).boxed(),
            Just(BinaryFunc::MulInt64).boxed(),
            Just(BinaryFunc::MulUInt16).boxed(),
            Just(BinaryFunc::MulUInt32).boxed(),
            Just(BinaryFunc::MulUInt64).boxed(),
            Just(BinaryFunc::MulFloat32).boxed(),
            Just(BinaryFunc::MulFloat64).boxed(),
            Just(BinaryFunc::MulNumeric).boxed(),
            Just(BinaryFunc::MulInterval).boxed(),
            Just(BinaryFunc::DivInt16).boxed(),
            Just(BinaryFunc::DivInt32).boxed(),
            Just(BinaryFunc::DivInt64).boxed(),
            Just(BinaryFunc::DivUInt16).boxed(),
            Just(BinaryFunc::DivUInt32).boxed(),
            Just(BinaryFunc::DivUInt64).boxed(),
            Just(BinaryFunc::DivFloat32).boxed(),
            Just(BinaryFunc::DivFloat64).boxed(),
            Just(BinaryFunc::DivNumeric).boxed(),
            Just(BinaryFunc::DivInterval).boxed(),
            Just(BinaryFunc::ModInt16).boxed(),
            Just(BinaryFunc::ModInt32).boxed(),
            Just(BinaryFunc::ModInt64).boxed(),
            Just(BinaryFunc::ModUInt16).boxed(),
            Just(BinaryFunc::ModUInt32).boxed(),
            Just(BinaryFunc::ModUInt64).boxed(),
            Just(BinaryFunc::ModFloat32).boxed(),
            Just(BinaryFunc::ModFloat64).boxed(),
            Just(BinaryFunc::ModNumeric).boxed(),
            Just(BinaryFunc::RoundNumeric).boxed(),
            Just(BinaryFunc::Eq).boxed(),
            Just(BinaryFunc::NotEq).boxed(),
            Just(BinaryFunc::Lt).boxed(),
            Just(BinaryFunc::Lte).boxed(),
            Just(BinaryFunc::Gt).boxed(),
            Just(BinaryFunc::Gte).boxed(),
            Just(BinaryFunc::LikeEscape).boxed(),
            bool::arbitrary()
                .prop_map(|case_insensitive| BinaryFunc::IsLikeMatch { case_insensitive })
                .boxed(),
            bool::arbitrary()
                .prop_map(|case_insensitive| BinaryFunc::IsRegexpMatch { case_insensitive })
                .boxed(),
            Just(BinaryFunc::ToCharTimestamp).boxed(),
            Just(BinaryFunc::ToCharTimestampTz).boxed(),
            Just(BinaryFunc::DateBinTimestamp).boxed(),
            Just(BinaryFunc::DateBinTimestampTz).boxed(),
            Just(BinaryFunc::ExtractInterval).boxed(),
            Just(BinaryFunc::ExtractTime).boxed(),
            Just(BinaryFunc::ExtractTimestamp).boxed(),
            Just(BinaryFunc::ExtractTimestampTz).boxed(),
            Just(BinaryFunc::ExtractDate).boxed(),
            Just(BinaryFunc::DatePartInterval).boxed(),
            Just(BinaryFunc::DatePartTime).boxed(),
            Just(BinaryFunc::DatePartTimestamp).boxed(),
            Just(BinaryFunc::DatePartTimestampTz).boxed(),
            Just(BinaryFunc::DateTruncTimestamp).boxed(),
            Just(BinaryFunc::DateTruncTimestampTz).boxed(),
            Just(BinaryFunc::DateTruncInterval).boxed(),
            Just(BinaryFunc::TimezoneTimestamp).boxed(),
            Just(BinaryFunc::TimezoneTimestampTz).boxed(),
            Just(BinaryFunc::TimezoneIntervalTimestamp).boxed(),
            Just(BinaryFunc::TimezoneIntervalTimestampTz).boxed(),
            Just(BinaryFunc::TimezoneIntervalTime).boxed(),
            Just(BinaryFunc::TimezoneOffset).boxed(),
            Just(BinaryFunc::TextConcat).boxed(),
            Just(BinaryFunc::JsonbGetInt64).boxed(),
            Just(BinaryFunc::JsonbGetInt64Stringify).boxed(),
            Just(BinaryFunc::JsonbGetString).boxed(),
            Just(BinaryFunc::JsonbGetStringStringify).boxed(),
            Just(BinaryFunc::JsonbGetPath).boxed(),
            Just(BinaryFunc::JsonbGetPathStringify).boxed(),
            Just(BinaryFunc::JsonbContainsString).boxed(),
            Just(BinaryFunc::JsonbConcat).boxed(),
            Just(BinaryFunc::JsonbContainsJsonb).boxed(),
            Just(BinaryFunc::JsonbDeleteInt64).boxed(),
            Just(BinaryFunc::JsonbDeleteString).boxed(),
            Just(BinaryFunc::MapContainsKey).boxed(),
            Just(BinaryFunc::MapGetValue).boxed(),
            Just(BinaryFunc::MapContainsAllKeys).boxed(),
            Just(BinaryFunc::MapContainsAnyKeys).boxed(),
            Just(BinaryFunc::MapContainsMap).boxed(),
            Just(BinaryFunc::ConvertFrom).boxed(),
            Just(BinaryFunc::Left).boxed(),
            Just(BinaryFunc::Position).boxed(),
            Just(BinaryFunc::Right).boxed(),
            Just(BinaryFunc::RepeatString).boxed(),
            Just(BinaryFunc::Trim).boxed(),
            Just(BinaryFunc::TrimLeading).boxed(),
            Just(BinaryFunc::TrimTrailing).boxed(),
            Just(BinaryFunc::EncodedBytesCharLength).boxed(),
            usize::arbitrary()
                .prop_map(|max_layer| BinaryFunc::ListLengthMax { max_layer })
                .boxed(),
            Just(BinaryFunc::ArrayContains).boxed(),
            Just(BinaryFunc::ArrayLength).boxed(),
            Just(BinaryFunc::ArrayLower).boxed(),
            Just(BinaryFunc::ArrayRemove).boxed(),
            Just(BinaryFunc::ArrayUpper).boxed(),
            Just(BinaryFunc::ArrayArrayConcat).boxed(),
            Just(BinaryFunc::ListListConcat).boxed(),
            Just(BinaryFunc::ListElementConcat).boxed(),
            Just(BinaryFunc::ElementListConcat).boxed(),
            Just(BinaryFunc::ListRemove).boxed(),
            Just(BinaryFunc::DigestString).boxed(),
            Just(BinaryFunc::DigestBytes).boxed(),
            Just(BinaryFunc::MzRenderTypmod).boxed(),
            Just(BinaryFunc::Encode).boxed(),
            Just(BinaryFunc::Decode).boxed(),
            Just(BinaryFunc::LogNumeric).boxed(),
            Just(BinaryFunc::Power).boxed(),
            Just(BinaryFunc::PowerNumeric).boxed(),
            (bool::arbitrary(), mz_repr::arb_range_type())
                .prop_map(|(rev, elem_type)| BinaryFunc::RangeContainsElem { elem_type, rev })
                .boxed(),
            bool::arbitrary()
                .prop_map(|rev| BinaryFunc::RangeContainsRange { rev })
                .boxed(),
            Just(BinaryFunc::RangeOverlaps).boxed(),
            Just(BinaryFunc::RangeAfter).boxed(),
            Just(BinaryFunc::RangeBefore).boxed(),
            Just(BinaryFunc::RangeOverleft).boxed(),
            Just(BinaryFunc::RangeOverright).boxed(),
            Just(BinaryFunc::RangeAdjacent).boxed(),
            Just(BinaryFunc::RangeUnion).boxed(),
            Just(BinaryFunc::RangeIntersection).boxed(),
            Just(BinaryFunc::RangeDifference).boxed(),
            Just(BinaryFunc::ParseIdent).boxed(),
        ])
    }
}

impl RustType<ProtoBinaryFunc> for BinaryFunc {
    fn into_proto(&self) -> ProtoBinaryFunc {
        use crate::scalar::proto_binary_func::Kind::*;
        let kind = match self {
            BinaryFunc::AddInt16 => AddInt16(()),
            BinaryFunc::AddInt32 => AddInt32(()),
            BinaryFunc::AddInt64 => AddInt64(()),
            BinaryFunc::AddUInt16 => AddUint16(()),
            BinaryFunc::AddUInt32 => AddUint32(()),
            BinaryFunc::AddUInt64 => AddUint64(()),
            BinaryFunc::AddFloat32 => AddFloat32(()),
            BinaryFunc::AddFloat64 => AddFloat64(()),
            BinaryFunc::AddInterval => AddInterval(()),
            BinaryFunc::AddTimestampInterval => AddTimestampInterval(()),
            BinaryFunc::AddTimestampTzInterval => AddTimestampTzInterval(()),
            BinaryFunc::AddDateInterval => AddDateInterval(()),
            BinaryFunc::AddDateTime => AddDateTime(()),
            BinaryFunc::AddTimeInterval => AddTimeInterval(()),
            BinaryFunc::AddNumeric => AddNumeric(()),
            BinaryFunc::AgeTimestamp => AgeTimestamp(()),
            BinaryFunc::AgeTimestampTz => AgeTimestampTz(()),
            BinaryFunc::BitAndInt16 => BitAndInt16(()),
            BinaryFunc::BitAndInt32 => BitAndInt32(()),
            BinaryFunc::BitAndInt64 => BitAndInt64(()),
            BinaryFunc::BitAndUInt16 => BitAndUint16(()),
            BinaryFunc::BitAndUInt32 => BitAndUint32(()),
            BinaryFunc::BitAndUInt64 => BitAndUint64(()),
            BinaryFunc::BitOrInt16 => BitOrInt16(()),
            BinaryFunc::BitOrInt32 => BitOrInt32(()),
            BinaryFunc::BitOrInt64 => BitOrInt64(()),
            BinaryFunc::BitOrUInt16 => BitOrUint16(()),
            BinaryFunc::BitOrUInt32 => BitOrUint32(()),
            BinaryFunc::BitOrUInt64 => BitOrUint64(()),
            BinaryFunc::BitXorInt16 => BitXorInt16(()),
            BinaryFunc::BitXorInt32 => BitXorInt32(()),
            BinaryFunc::BitXorInt64 => BitXorInt64(()),
            BinaryFunc::BitXorUInt16 => BitXorUint16(()),
            BinaryFunc::BitXorUInt32 => BitXorUint32(()),
            BinaryFunc::BitXorUInt64 => BitXorUint64(()),
            BinaryFunc::BitShiftLeftInt16 => BitShiftLeftInt16(()),
            BinaryFunc::BitShiftLeftInt32 => BitShiftLeftInt32(()),
            BinaryFunc::BitShiftLeftInt64 => BitShiftLeftInt64(()),
            BinaryFunc::BitShiftLeftUInt16 => BitShiftLeftUint16(()),
            BinaryFunc::BitShiftLeftUInt32 => BitShiftLeftUint32(()),
            BinaryFunc::BitShiftLeftUInt64 => BitShiftLeftUint64(()),
            BinaryFunc::BitShiftRightInt16 => BitShiftRightInt16(()),
            BinaryFunc::BitShiftRightInt32 => BitShiftRightInt32(()),
            BinaryFunc::BitShiftRightInt64 => BitShiftRightInt64(()),
            BinaryFunc::BitShiftRightUInt16 => BitShiftRightUint16(()),
            BinaryFunc::BitShiftRightUInt32 => BitShiftRightUint32(()),
            BinaryFunc::BitShiftRightUInt64 => BitShiftRightUint64(()),
            BinaryFunc::SubInt16 => SubInt16(()),
            BinaryFunc::SubInt32 => SubInt32(()),
            BinaryFunc::SubInt64 => SubInt64(()),
            BinaryFunc::SubUInt16 => SubUint16(()),
            BinaryFunc::SubUInt32 => SubUint32(()),
            BinaryFunc::SubUInt64 => SubUint64(()),
            BinaryFunc::SubFloat32 => SubFloat32(()),
            BinaryFunc::SubFloat64 => SubFloat64(()),
            BinaryFunc::SubInterval => SubInterval(()),
            BinaryFunc::SubTimestamp => SubTimestamp(()),
            BinaryFunc::SubTimestampTz => SubTimestampTz(()),
            BinaryFunc::SubTimestampInterval => SubTimestampInterval(()),
            BinaryFunc::SubTimestampTzInterval => SubTimestampTzInterval(()),
            BinaryFunc::SubDate => SubDate(()),
            BinaryFunc::SubDateInterval => SubDateInterval(()),
            BinaryFunc::SubTime => SubTime(()),
            BinaryFunc::SubTimeInterval => SubTimeInterval(()),
            BinaryFunc::SubNumeric => SubNumeric(()),
            BinaryFunc::MulInt16 => MulInt16(()),
            BinaryFunc::MulInt32 => MulInt32(()),
            BinaryFunc::MulInt64 => MulInt64(()),
            BinaryFunc::MulUInt16 => MulUint16(()),
            BinaryFunc::MulUInt32 => MulUint32(()),
            BinaryFunc::MulUInt64 => MulUint64(()),
            BinaryFunc::MulFloat32 => MulFloat32(()),
            BinaryFunc::MulFloat64 => MulFloat64(()),
            BinaryFunc::MulNumeric => MulNumeric(()),
            BinaryFunc::MulInterval => MulInterval(()),
            BinaryFunc::DivInt16 => DivInt16(()),
            BinaryFunc::DivInt32 => DivInt32(()),
            BinaryFunc::DivInt64 => DivInt64(()),
            BinaryFunc::DivUInt16 => DivUint16(()),
            BinaryFunc::DivUInt32 => DivUint32(()),
            BinaryFunc::DivUInt64 => DivUint64(()),
            BinaryFunc::DivFloat32 => DivFloat32(()),
            BinaryFunc::DivFloat64 => DivFloat64(()),
            BinaryFunc::DivNumeric => DivNumeric(()),
            BinaryFunc::DivInterval => DivInterval(()),
            BinaryFunc::ModInt16 => ModInt16(()),
            BinaryFunc::ModInt32 => ModInt32(()),
            BinaryFunc::ModInt64 => ModInt64(()),
            BinaryFunc::ModUInt16 => ModUint16(()),
            BinaryFunc::ModUInt32 => ModUint32(()),
            BinaryFunc::ModUInt64 => ModUint64(()),
            BinaryFunc::ModFloat32 => ModFloat32(()),
            BinaryFunc::ModFloat64 => ModFloat64(()),
            BinaryFunc::ModNumeric => ModNumeric(()),
            BinaryFunc::RoundNumeric => RoundNumeric(()),
            BinaryFunc::Eq => Eq(()),
            BinaryFunc::NotEq => NotEq(()),
            BinaryFunc::Lt => Lt(()),
            BinaryFunc::Lte => Lte(()),
            BinaryFunc::Gt => Gt(()),
            BinaryFunc::Gte => Gte(()),
            BinaryFunc::LikeEscape => LikeEscape(()),
            BinaryFunc::IsLikeMatch { case_insensitive } => IsLikeMatch(*case_insensitive),
            BinaryFunc::IsRegexpMatch { case_insensitive } => IsRegexpMatch(*case_insensitive),
            BinaryFunc::ToCharTimestamp => ToCharTimestamp(()),
            BinaryFunc::ToCharTimestampTz => ToCharTimestampTz(()),
            BinaryFunc::DateBinTimestamp => DateBinTimestamp(()),
            BinaryFunc::DateBinTimestampTz => DateBinTimestampTz(()),
            BinaryFunc::ExtractInterval => ExtractInterval(()),
            BinaryFunc::ExtractTime => ExtractTime(()),
            BinaryFunc::ExtractTimestamp => ExtractTimestamp(()),
            BinaryFunc::ExtractTimestampTz => ExtractTimestampTz(()),
            BinaryFunc::ExtractDate => ExtractDate(()),
            BinaryFunc::DatePartInterval => DatePartInterval(()),
            BinaryFunc::DatePartTime => DatePartTime(()),
            BinaryFunc::DatePartTimestamp => DatePartTimestamp(()),
            BinaryFunc::DatePartTimestampTz => DatePartTimestampTz(()),
            BinaryFunc::DateTruncTimestamp => DateTruncTimestamp(()),
            BinaryFunc::DateTruncTimestampTz => DateTruncTimestampTz(()),
            BinaryFunc::DateTruncInterval => DateTruncInterval(()),
            BinaryFunc::TimezoneTimestamp => TimezoneTimestamp(()),
            BinaryFunc::TimezoneTimestampTz => TimezoneTimestampTz(()),
            BinaryFunc::TimezoneIntervalTimestamp => TimezoneIntervalTimestamp(()),
            BinaryFunc::TimezoneIntervalTimestampTz => TimezoneIntervalTimestampTz(()),
            BinaryFunc::TimezoneIntervalTime => TimezoneIntervalTime(()),
            BinaryFunc::TimezoneOffset => TimezoneOffset(()),
            BinaryFunc::TextConcat => TextConcat(()),
            BinaryFunc::JsonbGetInt64 => JsonbGetInt64(()),
            BinaryFunc::JsonbGetInt64Stringify => JsonbGetInt64Stringify(()),
            BinaryFunc::JsonbGetString => JsonbGetString(()),
            BinaryFunc::JsonbGetStringStringify => JsonbGetStringStringify(()),
            BinaryFunc::JsonbGetPath => JsonbGetPath(()),
            BinaryFunc::JsonbGetPathStringify => JsonbGetPathStringify(()),
            BinaryFunc::JsonbContainsString => JsonbContainsString(()),
            BinaryFunc::JsonbConcat => JsonbConcat(()),
            BinaryFunc::JsonbContainsJsonb => JsonbContainsJsonb(()),
            BinaryFunc::JsonbDeleteInt64 => JsonbDeleteInt64(()),
            BinaryFunc::JsonbDeleteString => JsonbDeleteString(()),
            BinaryFunc::MapContainsKey => MapContainsKey(()),
            BinaryFunc::MapGetValue => MapGetValue(()),
            BinaryFunc::MapContainsAllKeys => MapContainsAllKeys(()),
            BinaryFunc::MapContainsAnyKeys => MapContainsAnyKeys(()),
            BinaryFunc::MapContainsMap => MapContainsMap(()),
            BinaryFunc::ConvertFrom => ConvertFrom(()),
            BinaryFunc::Left => Left(()),
            BinaryFunc::Position => Position(()),
            BinaryFunc::Right => Right(()),
            BinaryFunc::RepeatString => RepeatString(()),
            BinaryFunc::Trim => Trim(()),
            BinaryFunc::TrimLeading => TrimLeading(()),
            BinaryFunc::TrimTrailing => TrimTrailing(()),
            BinaryFunc::EncodedBytesCharLength => EncodedBytesCharLength(()),
            BinaryFunc::ListLengthMax { max_layer } => ListLengthMax(max_layer.into_proto()),
            BinaryFunc::ArrayContains => ArrayContains(()),
            BinaryFunc::ArrayContainsArray { rev } => ArrayContainsArray(*rev),
            BinaryFunc::ArrayLength => ArrayLength(()),
            BinaryFunc::ArrayLower => ArrayLower(()),
            BinaryFunc::ArrayRemove => ArrayRemove(()),
            BinaryFunc::ArrayUpper => ArrayUpper(()),
            BinaryFunc::ArrayArrayConcat => ArrayArrayConcat(()),
            BinaryFunc::ListListConcat => ListListConcat(()),
            BinaryFunc::ListElementConcat => ListElementConcat(()),
            BinaryFunc::ElementListConcat => ElementListConcat(()),
            BinaryFunc::ListRemove => ListRemove(()),
            BinaryFunc::ListContainsList { rev } => ListContainsList(*rev),
            BinaryFunc::DigestString => DigestString(()),
            BinaryFunc::DigestBytes => DigestBytes(()),
            BinaryFunc::MzRenderTypmod => MzRenderTypmod(()),
            BinaryFunc::Encode => Encode(()),
            BinaryFunc::Decode => Decode(()),
            BinaryFunc::LogNumeric => LogNumeric(()),
            BinaryFunc::Power => Power(()),
            BinaryFunc::PowerNumeric => PowerNumeric(()),
            BinaryFunc::GetBit => GetBit(()),
            BinaryFunc::GetByte => GetByte(()),
            BinaryFunc::RangeContainsElem { elem_type, rev } => {
                RangeContainsElem(crate::scalar::proto_binary_func::ProtoRangeContainsInner {
                    elem_type: Some(elem_type.into_proto()),
                    rev: *rev,
                })
            }
            BinaryFunc::RangeContainsRange { rev } => RangeContainsRange(*rev),
            BinaryFunc::RangeOverlaps => RangeOverlaps(()),
            BinaryFunc::RangeAfter => RangeAfter(()),
            BinaryFunc::RangeBefore => RangeBefore(()),
            BinaryFunc::RangeOverleft => RangeOverleft(()),
            BinaryFunc::RangeOverright => RangeOverright(()),
            BinaryFunc::RangeAdjacent => RangeAdjacent(()),
            BinaryFunc::RangeUnion => RangeUnion(()),
            BinaryFunc::RangeIntersection => RangeIntersection(()),
            BinaryFunc::RangeDifference => RangeDifference(()),
            BinaryFunc::UuidGenerateV5 => UuidGenerateV5(()),
            BinaryFunc::MzAclItemContainsPrivilege => MzAclItemContainsPrivilege(()),
            BinaryFunc::ParseIdent => ParseIdent(()),
            BinaryFunc::ConstantTimeEqBytes => ConstantTimeEqBytes(()),
            BinaryFunc::ConstantTimeEqString => ConstantTimeEqString(()),
            BinaryFunc::PrettySql => PrettySql(()),
            BinaryFunc::RegexpReplace { regex, limit } => {
                use crate::scalar::proto_binary_func::*;
                RegexpReplace(ProtoRegexpReplace {
                    regex: Some(regex.into_proto()),
                    limit: limit.into_proto(),
                })
            }
            BinaryFunc::StartsWith => StartsWith(()),
        };
        ProtoBinaryFunc { kind: Some(kind) }
    }

    fn from_proto(proto: ProtoBinaryFunc) -> Result<Self, TryFromProtoError> {
        use crate::scalar::proto_binary_func::Kind::*;
        if let Some(kind) = proto.kind {
            match kind {
                AddInt16(()) => Ok(BinaryFunc::AddInt16),
                AddInt32(()) => Ok(BinaryFunc::AddInt32),
                AddInt64(()) => Ok(BinaryFunc::AddInt64),
                AddUint16(()) => Ok(BinaryFunc::AddUInt16),
                AddUint32(()) => Ok(BinaryFunc::AddUInt32),
                AddUint64(()) => Ok(BinaryFunc::AddUInt64),
                AddFloat32(()) => Ok(BinaryFunc::AddFloat32),
                AddFloat64(()) => Ok(BinaryFunc::AddFloat64),
                AddInterval(()) => Ok(BinaryFunc::AddInterval),
                AddTimestampInterval(()) => Ok(BinaryFunc::AddTimestampInterval),
                AddTimestampTzInterval(()) => Ok(BinaryFunc::AddTimestampTzInterval),
                AddDateInterval(()) => Ok(BinaryFunc::AddDateInterval),
                AddDateTime(()) => Ok(BinaryFunc::AddDateTime),
                AddTimeInterval(()) => Ok(BinaryFunc::AddTimeInterval),
                AddNumeric(()) => Ok(BinaryFunc::AddNumeric),
                AgeTimestamp(()) => Ok(BinaryFunc::AgeTimestamp),
                AgeTimestampTz(()) => Ok(BinaryFunc::AgeTimestampTz),
                BitAndInt16(()) => Ok(BinaryFunc::BitAndInt16),
                BitAndInt32(()) => Ok(BinaryFunc::BitAndInt32),
                BitAndInt64(()) => Ok(BinaryFunc::BitAndInt64),
                BitAndUint16(()) => Ok(BinaryFunc::BitAndUInt16),
                BitAndUint32(()) => Ok(BinaryFunc::BitAndUInt32),
                BitAndUint64(()) => Ok(BinaryFunc::BitAndUInt64),
                BitOrInt16(()) => Ok(BinaryFunc::BitOrInt16),
                BitOrInt32(()) => Ok(BinaryFunc::BitOrInt32),
                BitOrInt64(()) => Ok(BinaryFunc::BitOrInt64),
                BitOrUint16(()) => Ok(BinaryFunc::BitOrUInt16),
                BitOrUint32(()) => Ok(BinaryFunc::BitOrUInt32),
                BitOrUint64(()) => Ok(BinaryFunc::BitOrUInt64),
                BitXorInt16(()) => Ok(BinaryFunc::BitXorInt16),
                BitXorInt32(()) => Ok(BinaryFunc::BitXorInt32),
                BitXorInt64(()) => Ok(BinaryFunc::BitXorInt64),
                BitXorUint16(()) => Ok(BinaryFunc::BitXorUInt16),
                BitXorUint32(()) => Ok(BinaryFunc::BitXorUInt32),
                BitXorUint64(()) => Ok(BinaryFunc::BitXorUInt64),
                BitShiftLeftInt16(()) => Ok(BinaryFunc::BitShiftLeftInt16),
                BitShiftLeftInt32(()) => Ok(BinaryFunc::BitShiftLeftInt32),
                BitShiftLeftInt64(()) => Ok(BinaryFunc::BitShiftLeftInt64),
                BitShiftLeftUint16(()) => Ok(BinaryFunc::BitShiftLeftUInt16),
                BitShiftLeftUint32(()) => Ok(BinaryFunc::BitShiftLeftUInt32),
                BitShiftLeftUint64(()) => Ok(BinaryFunc::BitShiftLeftUInt64),
                BitShiftRightInt16(()) => Ok(BinaryFunc::BitShiftRightInt16),
                BitShiftRightInt32(()) => Ok(BinaryFunc::BitShiftRightInt32),
                BitShiftRightInt64(()) => Ok(BinaryFunc::BitShiftRightInt64),
                BitShiftRightUint16(()) => Ok(BinaryFunc::BitShiftRightUInt16),
                BitShiftRightUint32(()) => Ok(BinaryFunc::BitShiftRightUInt32),
                BitShiftRightUint64(()) => Ok(BinaryFunc::BitShiftRightUInt64),
                SubInt16(()) => Ok(BinaryFunc::SubInt16),
                SubInt32(()) => Ok(BinaryFunc::SubInt32),
                SubInt64(()) => Ok(BinaryFunc::SubInt64),
                SubUint16(()) => Ok(BinaryFunc::SubUInt16),
                SubUint32(()) => Ok(BinaryFunc::SubUInt32),
                SubUint64(()) => Ok(BinaryFunc::SubUInt64),
                SubFloat32(()) => Ok(BinaryFunc::SubFloat32),
                SubFloat64(()) => Ok(BinaryFunc::SubFloat64),
                SubInterval(()) => Ok(BinaryFunc::SubInterval),
                SubTimestamp(()) => Ok(BinaryFunc::SubTimestamp),
                SubTimestampTz(()) => Ok(BinaryFunc::SubTimestampTz),
                SubTimestampInterval(()) => Ok(BinaryFunc::SubTimestampInterval),
                SubTimestampTzInterval(()) => Ok(BinaryFunc::SubTimestampTzInterval),
                SubDate(()) => Ok(BinaryFunc::SubDate),
                SubDateInterval(()) => Ok(BinaryFunc::SubDateInterval),
                SubTime(()) => Ok(BinaryFunc::SubTime),
                SubTimeInterval(()) => Ok(BinaryFunc::SubTimeInterval),
                SubNumeric(()) => Ok(BinaryFunc::SubNumeric),
                MulInt16(()) => Ok(BinaryFunc::MulInt16),
                MulInt32(()) => Ok(BinaryFunc::MulInt32),
                MulInt64(()) => Ok(BinaryFunc::MulInt64),
                MulUint16(()) => Ok(BinaryFunc::MulUInt16),
                MulUint32(()) => Ok(BinaryFunc::MulUInt32),
                MulUint64(()) => Ok(BinaryFunc::MulUInt64),
                MulFloat32(()) => Ok(BinaryFunc::MulFloat32),
                MulFloat64(()) => Ok(BinaryFunc::MulFloat64),
                MulNumeric(()) => Ok(BinaryFunc::MulNumeric),
                MulInterval(()) => Ok(BinaryFunc::MulInterval),
                DivInt16(()) => Ok(BinaryFunc::DivInt16),
                DivInt32(()) => Ok(BinaryFunc::DivInt32),
                DivInt64(()) => Ok(BinaryFunc::DivInt64),
                DivUint16(()) => Ok(BinaryFunc::DivUInt16),
                DivUint32(()) => Ok(BinaryFunc::DivUInt32),
                DivUint64(()) => Ok(BinaryFunc::DivUInt64),
                DivFloat32(()) => Ok(BinaryFunc::DivFloat32),
                DivFloat64(()) => Ok(BinaryFunc::DivFloat64),
                DivNumeric(()) => Ok(BinaryFunc::DivNumeric),
                DivInterval(()) => Ok(BinaryFunc::DivInterval),
                ModInt16(()) => Ok(BinaryFunc::ModInt16),
                ModInt32(()) => Ok(BinaryFunc::ModInt32),
                ModInt64(()) => Ok(BinaryFunc::ModInt64),
                ModUint16(()) => Ok(BinaryFunc::ModUInt16),
                ModUint32(()) => Ok(BinaryFunc::ModUInt32),
                ModUint64(()) => Ok(BinaryFunc::ModUInt64),
                ModFloat32(()) => Ok(BinaryFunc::ModFloat32),
                ModFloat64(()) => Ok(BinaryFunc::ModFloat64),
                ModNumeric(()) => Ok(BinaryFunc::ModNumeric),
                RoundNumeric(()) => Ok(BinaryFunc::RoundNumeric),
                Eq(()) => Ok(BinaryFunc::Eq),
                NotEq(()) => Ok(BinaryFunc::NotEq),
                Lt(()) => Ok(BinaryFunc::Lt),
                Lte(()) => Ok(BinaryFunc::Lte),
                Gt(()) => Ok(BinaryFunc::Gt),
                Gte(()) => Ok(BinaryFunc::Gte),
                LikeEscape(()) => Ok(BinaryFunc::LikeEscape),
                IsLikeMatch(case_insensitive) => Ok(BinaryFunc::IsLikeMatch { case_insensitive }),
                IsRegexpMatch(case_insensitive) => {
                    Ok(BinaryFunc::IsRegexpMatch { case_insensitive })
                }
                ToCharTimestamp(()) => Ok(BinaryFunc::ToCharTimestamp),
                ToCharTimestampTz(()) => Ok(BinaryFunc::ToCharTimestampTz),
                DateBinTimestamp(()) => Ok(BinaryFunc::DateBinTimestamp),
                DateBinTimestampTz(()) => Ok(BinaryFunc::DateBinTimestampTz),
                ExtractInterval(()) => Ok(BinaryFunc::ExtractInterval),
                ExtractTime(()) => Ok(BinaryFunc::ExtractTime),
                ExtractTimestamp(()) => Ok(BinaryFunc::ExtractTimestamp),
                ExtractTimestampTz(()) => Ok(BinaryFunc::ExtractTimestampTz),
                ExtractDate(()) => Ok(BinaryFunc::ExtractDate),
                DatePartInterval(()) => Ok(BinaryFunc::DatePartInterval),
                DatePartTime(()) => Ok(BinaryFunc::DatePartTime),
                DatePartTimestamp(()) => Ok(BinaryFunc::DatePartTimestamp),
                DatePartTimestampTz(()) => Ok(BinaryFunc::DatePartTimestampTz),
                DateTruncTimestamp(()) => Ok(BinaryFunc::DateTruncTimestamp),
                DateTruncTimestampTz(()) => Ok(BinaryFunc::DateTruncTimestampTz),
                DateTruncInterval(()) => Ok(BinaryFunc::DateTruncInterval),
                TimezoneTimestamp(()) => Ok(BinaryFunc::TimezoneTimestamp),
                TimezoneTimestampTz(()) => Ok(BinaryFunc::TimezoneTimestampTz),
                TimezoneIntervalTimestamp(()) => Ok(BinaryFunc::TimezoneIntervalTimestamp),
                TimezoneIntervalTimestampTz(()) => Ok(BinaryFunc::TimezoneIntervalTimestampTz),
                TimezoneIntervalTime(()) => Ok(BinaryFunc::TimezoneIntervalTime),
                TimezoneOffset(()) => Ok(BinaryFunc::TimezoneOffset),
                TextConcat(()) => Ok(BinaryFunc::TextConcat),
                JsonbGetInt64(()) => Ok(BinaryFunc::JsonbGetInt64),
                JsonbGetInt64Stringify(()) => Ok(BinaryFunc::JsonbGetInt64Stringify),
                JsonbGetString(()) => Ok(BinaryFunc::JsonbGetString),
                JsonbGetStringStringify(()) => Ok(BinaryFunc::JsonbGetStringStringify),
                JsonbGetPath(()) => Ok(BinaryFunc::JsonbGetPath),
                JsonbGetPathStringify(()) => Ok(BinaryFunc::JsonbGetPathStringify),
                JsonbContainsString(()) => Ok(BinaryFunc::JsonbContainsString),
                JsonbConcat(()) => Ok(BinaryFunc::JsonbConcat),
                JsonbContainsJsonb(()) => Ok(BinaryFunc::JsonbContainsJsonb),
                JsonbDeleteInt64(()) => Ok(BinaryFunc::JsonbDeleteInt64),
                JsonbDeleteString(()) => Ok(BinaryFunc::JsonbDeleteString),
                MapContainsKey(()) => Ok(BinaryFunc::MapContainsKey),
                MapGetValue(()) => Ok(BinaryFunc::MapGetValue),
                MapContainsAllKeys(()) => Ok(BinaryFunc::MapContainsAllKeys),
                MapContainsAnyKeys(()) => Ok(BinaryFunc::MapContainsAnyKeys),
                MapContainsMap(()) => Ok(BinaryFunc::MapContainsMap),
                ConvertFrom(()) => Ok(BinaryFunc::ConvertFrom),
                Left(()) => Ok(BinaryFunc::Left),
                Position(()) => Ok(BinaryFunc::Position),
                Right(()) => Ok(BinaryFunc::Right),
                RepeatString(()) => Ok(BinaryFunc::RepeatString),
                Trim(()) => Ok(BinaryFunc::Trim),
                TrimLeading(()) => Ok(BinaryFunc::TrimLeading),
                TrimTrailing(()) => Ok(BinaryFunc::TrimTrailing),
                EncodedBytesCharLength(()) => Ok(BinaryFunc::EncodedBytesCharLength),
                ListLengthMax(max_layer) => Ok(BinaryFunc::ListLengthMax {
                    max_layer: max_layer.into_rust()?,
                }),
                ArrayContains(()) => Ok(BinaryFunc::ArrayContains),
                ArrayContainsArray(rev) => Ok(BinaryFunc::ArrayContainsArray { rev }),
                ArrayLength(()) => Ok(BinaryFunc::ArrayLength),
                ArrayLower(()) => Ok(BinaryFunc::ArrayLower),
                ArrayRemove(()) => Ok(BinaryFunc::ArrayRemove),
                ArrayUpper(()) => Ok(BinaryFunc::ArrayUpper),
                ArrayArrayConcat(()) => Ok(BinaryFunc::ArrayArrayConcat),
                ListListConcat(()) => Ok(BinaryFunc::ListListConcat),
                ListElementConcat(()) => Ok(BinaryFunc::ListElementConcat),
                ElementListConcat(()) => Ok(BinaryFunc::ElementListConcat),
                ListRemove(()) => Ok(BinaryFunc::ListRemove),
                ListContainsList(rev) => Ok(BinaryFunc::ListContainsList { rev }),
                DigestString(()) => Ok(BinaryFunc::DigestString),
                DigestBytes(()) => Ok(BinaryFunc::DigestBytes),
                MzRenderTypmod(()) => Ok(BinaryFunc::MzRenderTypmod),
                Encode(()) => Ok(BinaryFunc::Encode),
                Decode(()) => Ok(BinaryFunc::Decode),
                LogNumeric(()) => Ok(BinaryFunc::LogNumeric),
                Power(()) => Ok(BinaryFunc::Power),
                PowerNumeric(()) => Ok(BinaryFunc::PowerNumeric),
                GetBit(()) => Ok(BinaryFunc::GetBit),
                GetByte(()) => Ok(BinaryFunc::GetByte),
                RangeContainsElem(inner) => Ok(BinaryFunc::RangeContainsElem {
                    elem_type: inner
                        .elem_type
                        .into_rust_if_some("ProtoRangeContainsInner::elem_type")?,
                    rev: inner.rev,
                }),
                RangeContainsRange(rev) => Ok(BinaryFunc::RangeContainsRange { rev }),
                RangeOverlaps(()) => Ok(BinaryFunc::RangeOverlaps),
                RangeAfter(()) => Ok(BinaryFunc::RangeAfter),
                RangeBefore(()) => Ok(BinaryFunc::RangeBefore),
                RangeOverleft(()) => Ok(BinaryFunc::RangeOverleft),
                RangeOverright(()) => Ok(BinaryFunc::RangeOverright),
                RangeAdjacent(()) => Ok(BinaryFunc::RangeAdjacent),
                RangeUnion(()) => Ok(BinaryFunc::RangeUnion),
                RangeIntersection(()) => Ok(BinaryFunc::RangeIntersection),
                RangeDifference(()) => Ok(BinaryFunc::RangeDifference),
                UuidGenerateV5(()) => Ok(BinaryFunc::UuidGenerateV5),
                MzAclItemContainsPrivilege(()) => Ok(BinaryFunc::MzAclItemContainsPrivilege),
                ParseIdent(()) => Ok(BinaryFunc::ParseIdent),
                ConstantTimeEqBytes(()) => Ok(BinaryFunc::ConstantTimeEqBytes),
                ConstantTimeEqString(()) => Ok(BinaryFunc::ConstantTimeEqString),
                PrettySql(()) => Ok(BinaryFunc::PrettySql),
                RegexpReplace(inner) => Ok(BinaryFunc::RegexpReplace {
                    regex: inner.regex.into_rust_if_some("ProtoRegexReplace::regex")?,
                    limit: inner.limit.into_rust()?,
                }),
                StartsWith(()) => Ok(BinaryFunc::StartsWith),
            }
        } else {
            Err(TryFromProtoError::missing_field("ProtoBinaryFunc::kind"))
        }
    }
}

/// A description of an SQL unary function that has the ability to lazy evaluate its arguments
// This trait will eventually be annotated with #[enum_dispatch] to autogenerate the UnaryFunc enum
trait LazyUnaryFunc {
    fn eval<'a>(
        &'a self,
        datums: &[Datum<'a>],
        temp_storage: &'a RowArena,
        a: &'a MirScalarExpr,
    ) -> Result<Datum<'a>, EvalError>;

    /// The output ColumnType of this function.
    fn output_type(&self, input_type: ColumnType) -> ColumnType;

    /// Whether this function will produce NULL on NULL input.
    fn propagates_nulls(&self) -> bool;

    /// Whether this function will produce NULL on non-NULL input.
    fn introduces_nulls(&self) -> bool;

    /// Whether this function might error on non-error input.
    fn could_error(&self) -> bool {
        // NB: override this for functions that never error.
        true
    }

    /// Whether this function preserves uniqueness.
    ///
    /// Uniqueness is preserved when `if f(x) = f(y) then x = y` is true. This
    /// is used by the optimizer when a guarantee can be made that a collection
    /// with unique items will stay unique when mapped by this function.
    ///
    /// Note that error results are not covered: Even with `preserves_uniqueness = true`, it can
    /// happen that two different inputs produce the same error result. (e.g., in case of a
    /// narrowing cast)
    ///
    /// Functions should conservatively return `false` unless they are certain
    /// the above property is true.
    fn preserves_uniqueness(&self) -> bool;

    /// The [inverse] of this function, if it has one and we have determined it.
    ///
    /// The optimizer _can_ use this information when selecting indexes, e.g. an
    /// indexed column has a cast applied to it, by moving the right inverse of
    /// the cast to another value, we can select the indexed column.
    ///
    /// Note that a value of `None` does not imply that the inverse does not
    /// exist; it could also mean we have not yet invested the energy in
    /// representing it. For example, in the case of complex casts, such as
    /// between two list types, we could determine the right inverse, but doing
    /// so is not immediately necessary as this information is only used by the
    /// optimizer.
    ///
    /// ## Right vs. left vs. inverses
    /// - Right inverses are when the inverse function preserves uniqueness.
    ///   These are the functions that the optimizer uses to move casts between
    ///   expressions.
    /// - Left inverses are when the function itself preserves uniqueness.
    /// - Inverses are when a function is both a right and a left inverse (e.g.,
    ///   bit_not_int64 is both a right and left inverse of itself).
    ///
    /// We call this function `inverse` for simplicity's sake; it doesn't always
    /// correspond to the mathematical notion of "inverse." However, in
    /// conjunction with checks to `preserves_uniqueness` you can determine
    /// which type of inverse we return.
    ///
    /// [inverse]: https://en.wikipedia.org/wiki/Inverse_function
    fn inverse(&self) -> Option<crate::UnaryFunc>;

    /// Returns true if the function is monotone. (Non-strict; either increasing or decreasing.)
    /// Monotone functions map ranges to ranges: ie. given a range of possible inputs, we can
    /// determine the range of possible outputs just by mapping the endpoints.
    ///
    /// This property describes the behaviour of the function over ranges where the function is defined:
    /// ie. the argument and the result are non-error datums.
    fn is_monotone(&self) -> bool;
}

/// A description of an SQL unary function that operates on eagerly evaluated expressions
trait EagerUnaryFunc<'a> {
    type Input: DatumType<'a, EvalError>;
    type Output: DatumType<'a, EvalError>;

    fn call(&self, input: Self::Input) -> Self::Output;

    /// The output ColumnType of this function
    fn output_type(&self, input_type: ColumnType) -> ColumnType;

    /// Whether this function will produce NULL on NULL input
    fn propagates_nulls(&self) -> bool {
        // If the input is not nullable then nulls are propagated
        !Self::Input::nullable()
    }

    /// Whether this function will produce NULL on non-NULL input
    fn introduces_nulls(&self) -> bool {
        // If the output is nullable then nulls can be introduced
        Self::Output::nullable()
    }

    /// Whether this function could produce an error
    fn could_error(&self) -> bool {
        Self::Output::fallible()
    }

    /// Whether this function preserves uniqueness
    fn preserves_uniqueness(&self) -> bool {
        false
    }

    fn inverse(&self) -> Option<crate::UnaryFunc> {
        None
    }

    fn is_monotone(&self) -> bool {
        false
    }
}

impl<T: for<'a> EagerUnaryFunc<'a>> LazyUnaryFunc for T {
    fn eval<'a>(
        &'a self,
        datums: &[Datum<'a>],
        temp_storage: &'a RowArena,
        a: &'a MirScalarExpr,
    ) -> Result<Datum<'a>, EvalError> {
        match T::Input::try_from_result(a.eval(datums, temp_storage)) {
            // If we can convert to the input type then we call the function
            Ok(input) => self.call(input).into_result(temp_storage),
            // If we can't and we got a non-null datum something went wrong in the planner
            Err(Ok(datum)) if !datum.is_null() => {
                Err(EvalError::Internal("invalid input type".into()))
            }
            // Otherwise we just propagate NULLs and errors
            Err(res) => res,
        }
    }

    fn output_type(&self, input_type: ColumnType) -> ColumnType {
        self.output_type(input_type)
    }

    fn propagates_nulls(&self) -> bool {
        self.propagates_nulls()
    }

    fn introduces_nulls(&self) -> bool {
        self.introduces_nulls()
    }

    fn could_error(&self) -> bool {
        self.could_error()
    }

    fn preserves_uniqueness(&self) -> bool {
        self.preserves_uniqueness()
    }

    fn inverse(&self) -> Option<crate::UnaryFunc> {
        self.inverse()
    }

    fn is_monotone(&self) -> bool {
        self.is_monotone()
    }
}

derive_unary!(
    Not,
    IsNull,
    IsTrue,
    IsFalse,
    BitNotInt16,
    BitNotInt32,
    BitNotInt64,
    BitNotUint16,
    BitNotUint32,
    BitNotUint64,
    NegInt16,
    NegInt32,
    NegInt64,
    NegFloat32,
    NegFloat64,
    NegNumeric,
    NegInterval,
    SqrtFloat64,
    SqrtNumeric,
    CbrtFloat64,
    AbsInt16,
    AbsInt32,
    AbsInt64,
    AbsFloat32,
    AbsFloat64,
    AbsNumeric,
    CastBoolToString,
    CastBoolToStringNonstandard,
    CastBoolToInt32,
    CastBoolToInt64,
    CastInt16ToFloat32,
    CastInt16ToFloat64,
    CastInt16ToInt32,
    CastInt16ToInt64,
    CastInt16ToUint16,
    CastInt16ToUint32,
    CastInt16ToUint64,
    CastInt16ToString,
    CastInt2VectorToArray,
    CastInt32ToBool,
    CastInt32ToFloat32,
    CastInt32ToFloat64,
    CastInt32ToOid,
    CastInt32ToPgLegacyChar,
    CastInt32ToInt16,
    CastInt32ToInt64,
    CastInt32ToUint16,
    CastInt32ToUint32,
    CastInt32ToUint64,
    CastInt32ToString,
    CastOidToInt32,
    CastOidToInt64,
    CastOidToString,
    CastOidToRegClass,
    CastRegClassToOid,
    CastOidToRegProc,
    CastRegProcToOid,
    CastOidToRegType,
    CastRegTypeToOid,
    CastInt64ToInt16,
    CastInt64ToInt32,
    CastInt64ToUint16,
    CastInt64ToUint32,
    CastInt64ToUint64,
    CastInt16ToNumeric,
    CastInt32ToNumeric,
    CastInt64ToBool,
    CastInt64ToNumeric,
    CastInt64ToFloat32,
    CastInt64ToFloat64,
    CastInt64ToOid,
    CastInt64ToString,
    CastUint16ToUint32,
    CastUint16ToUint64,
    CastUint16ToInt16,
    CastUint16ToInt32,
    CastUint16ToInt64,
    CastUint16ToNumeric,
    CastUint16ToFloat32,
    CastUint16ToFloat64,
    CastUint16ToString,
    CastUint32ToUint16,
    CastUint32ToUint64,
    CastUint32ToInt16,
    CastUint32ToInt32,
    CastUint32ToInt64,
    CastUint32ToNumeric,
    CastUint32ToFloat32,
    CastUint32ToFloat64,
    CastUint32ToString,
    CastUint64ToUint16,
    CastUint64ToUint32,
    CastUint64ToInt16,
    CastUint64ToInt32,
    CastUint64ToInt64,
    CastUint64ToNumeric,
    CastUint64ToFloat32,
    CastUint64ToFloat64,
    CastUint64ToString,
    CastFloat32ToInt16,
    CastFloat32ToInt32,
    CastFloat32ToInt64,
    CastFloat32ToUint16,
    CastFloat32ToUint32,
    CastFloat32ToUint64,
    CastFloat32ToFloat64,
    CastFloat32ToString,
    CastFloat32ToNumeric,
    CastFloat64ToNumeric,
    CastFloat64ToInt16,
    CastFloat64ToInt32,
    CastFloat64ToInt64,
    CastFloat64ToUint16,
    CastFloat64ToUint32,
    CastFloat64ToUint64,
    CastFloat64ToFloat32,
    CastFloat64ToString,
    CastNumericToFloat32,
    CastNumericToFloat64,
    CastNumericToInt16,
    CastNumericToInt32,
    CastNumericToInt64,
    CastNumericToUint16,
    CastNumericToUint32,
    CastNumericToUint64,
    CastNumericToString,
    CastMzTimestampToString,
    CastMzTimestampToTimestamp,
    CastMzTimestampToTimestampTz,
    CastStringToMzTimestamp,
    CastUint64ToMzTimestamp,
    CastUint32ToMzTimestamp,
    CastInt64ToMzTimestamp,
    CastInt32ToMzTimestamp,
    CastNumericToMzTimestamp,
    CastTimestampToMzTimestamp,
    CastTimestampTzToMzTimestamp,
    CastDateToMzTimestamp,
    CastStringToBool,
    CastStringToPgLegacyChar,
    CastStringToPgLegacyName,
    CastStringToBytes,
    CastStringToInt16,
    CastStringToInt32,
    CastStringToInt64,
    CastStringToUint16,
    CastStringToUint32,
    CastStringToUint64,
    CastStringToInt2Vector,
    CastStringToOid,
    CastStringToFloat32,
    CastStringToFloat64,
    CastStringToDate,
    CastStringToArray,
    CastStringToList,
    CastStringToMap,
    CastStringToRange,
    CastStringToTime,
    CastStringToTimestamp,
    CastStringToTimestampTz,
    CastStringToInterval,
    CastStringToNumeric,
    CastStringToUuid,
    CastStringToChar,
    PadChar,
    CastStringToVarChar,
    CastCharToString,
    CastVarCharToString,
    CastDateToTimestamp,
    CastDateToTimestampTz,
    CastDateToString,
    CastTimeToInterval,
    CastTimeToString,
    CastIntervalToString,
    CastIntervalToTime,
    CastTimestampToDate,
    AdjustTimestampPrecision,
    CastTimestampToTimestampTz,
    CastTimestampToString,
    CastTimestampToTime,
    CastTimestampTzToDate,
    CastTimestampTzToTimestamp,
    AdjustTimestampTzPrecision,
    CastTimestampTzToString,
    CastTimestampTzToTime,
    CastPgLegacyCharToString,
    CastPgLegacyCharToChar,
    CastPgLegacyCharToVarChar,
    CastPgLegacyCharToInt32,
    CastBytesToString,
    CastStringToJsonb,
    CastJsonbToString,
    CastJsonbableToJsonb,
    CastJsonbToInt16,
    CastJsonbToInt32,
    CastJsonbToInt64,
    CastJsonbToFloat32,
    CastJsonbToFloat64,
    CastJsonbToNumeric,
    CastJsonbToBool,
    CastUuidToString,
    CastRecordToString,
    CastRecord1ToRecord2,
    CastArrayToArray,
    CastArrayToJsonb,
    CastArrayToString,
    CastListToString,
    CastListToJsonb,
    CastList1ToList2,
    CastArrayToListOneDim,
    CastMapToString,
    CastInt2VectorToString,
    CastRangeToString,
    CeilFloat32,
    CeilFloat64,
    CeilNumeric,
    FloorFloat32,
    FloorFloat64,
    FloorNumeric,
    Ascii,
    BitCountBytes,
    BitLengthBytes,
    BitLengthString,
    ByteLengthBytes,
    ByteLengthString,
    CharLength,
    Chr,
    IsLikeMatch,
    IsRegexpMatch,
    RegexpMatch,
    ExtractInterval,
    ExtractTime,
    ExtractTimestamp,
    ExtractTimestampTz,
    ExtractDate,
    DatePartInterval,
    DatePartTime,
    DatePartTimestamp,
    DatePartTimestampTz,
    DateTruncTimestamp,
    DateTruncTimestampTz,
    TimezoneTimestamp,
    TimezoneTimestampTz,
    TimezoneTime,
    ToTimestamp,
    ToCharTimestamp,
    ToCharTimestampTz,
    JustifyDays,
    JustifyHours,
    JustifyInterval,
    JsonbArrayLength,
    JsonbTypeof,
    JsonbStripNulls,
    JsonbPretty,
    RoundFloat32,
    RoundFloat64,
    RoundNumeric,
    TruncFloat32,
    TruncFloat64,
    TruncNumeric,
    TrimWhitespace,
    TrimLeadingWhitespace,
    TrimTrailingWhitespace,
    Initcap,
    RecordGet,
    ListLength,
    MapLength,
    MapBuildFromRecordList,
    Upper,
    Lower,
    Cos,
    Acos,
    Cosh,
    Acosh,
    Sin,
    Asin,
    Sinh,
    Asinh,
    Tan,
    Atan,
    Tanh,
    Atanh,
    Cot,
    Degrees,
    Radians,
    Log10,
    Log10Numeric,
    Ln,
    LnNumeric,
    Exp,
    ExpNumeric,
    Sleep,
    Panic,
    AdjustNumericScale,
    PgColumnSize,
    MzRowSize,
    MzTypeName,
    StepMzTimestamp,
    RangeLower,
    RangeUpper,
    RangeEmpty,
    RangeLowerInc,
    RangeUpperInc,
    RangeLowerInf,
    RangeUpperInf,
    MzAclItemGrantor,
    MzAclItemGrantee,
    MzAclItemPrivileges,
    MzFormatPrivileges,
    MzValidatePrivileges,
    MzValidateRolePrivilege,
    AclItemGrantor,
    AclItemGrantee,
    AclItemPrivileges,
    QuoteIdent,
    TryParseMonotonicIso8601Timestamp,
    RegexpSplitToArray,
    PgSizePretty,
    Crc32Bytes,
    Crc32String,
    KafkaMurmur2Bytes,
    KafkaMurmur2String,
    SeahashBytes,
    SeahashString,
    Reverse
);

impl UnaryFunc {
    /// If the unary_func represents "IS X", return X.
    ///
    /// A helper method for being able to print Not(IsX) as IS NOT X.
    pub fn is(&self) -> Option<&'static str> {
        match self {
            UnaryFunc::IsNull(_) => Some("NULL"),
            UnaryFunc::IsTrue(_) => Some("TRUE"),
            UnaryFunc::IsFalse(_) => Some("FALSE"),
            _ => None,
        }
    }
}

/// An explicit [`Arbitrary`] implementation needed here because of a known
/// `proptest` issue.
///
/// Revert to the derive-macro implementation once the issue[^1] is fixed.
///
/// [^1]: <https://github.com/AltSysrq/proptest/issues/152>
impl Arbitrary for UnaryFunc {
    type Parameters = ();

    type Strategy = Union<BoxedStrategy<Self>>;

    fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
        Union::new(vec![
            Not::arbitrary().prop_map_into().boxed(),
            IsNull::arbitrary().prop_map_into().boxed(),
            IsTrue::arbitrary().prop_map_into().boxed(),
            IsFalse::arbitrary().prop_map_into().boxed(),
            BitNotInt16::arbitrary().prop_map_into().boxed(),
            BitNotInt32::arbitrary().prop_map_into().boxed(),
            BitNotInt64::arbitrary().prop_map_into().boxed(),
            BitNotUint16::arbitrary().prop_map_into().boxed(),
            BitNotUint32::arbitrary().prop_map_into().boxed(),
            BitNotUint64::arbitrary().prop_map_into().boxed(),
            NegInt16::arbitrary().prop_map_into().boxed(),
            NegInt32::arbitrary().prop_map_into().boxed(),
            NegInt64::arbitrary().prop_map_into().boxed(),
            NegFloat32::arbitrary().prop_map_into().boxed(),
            NegFloat64::arbitrary().prop_map_into().boxed(),
            NegNumeric::arbitrary().prop_map_into().boxed(),
            NegInterval::arbitrary().prop_map_into().boxed(),
            SqrtFloat64::arbitrary().prop_map_into().boxed(),
            SqrtNumeric::arbitrary().prop_map_into().boxed(),
            CbrtFloat64::arbitrary().prop_map_into().boxed(),
            AbsInt16::arbitrary().prop_map_into().boxed(),
            AbsInt32::arbitrary().prop_map_into().boxed(),
            AbsInt64::arbitrary().prop_map_into().boxed(),
            AbsFloat32::arbitrary().prop_map_into().boxed(),
            AbsFloat64::arbitrary().prop_map_into().boxed(),
            AbsNumeric::arbitrary().prop_map_into().boxed(),
            CastBoolToString::arbitrary().prop_map_into().boxed(),
            CastBoolToStringNonstandard::arbitrary()
                .prop_map_into()
                .boxed(),
            CastBoolToInt32::arbitrary().prop_map_into().boxed(),
            CastBoolToInt64::arbitrary().prop_map_into().boxed(),
            CastInt16ToFloat32::arbitrary().prop_map_into().boxed(),
            CastInt16ToFloat64::arbitrary().prop_map_into().boxed(),
            CastInt16ToInt32::arbitrary().prop_map_into().boxed(),
            CastInt16ToInt64::arbitrary().prop_map_into().boxed(),
            CastInt16ToUint16::arbitrary().prop_map_into().boxed(),
            CastInt16ToUint32::arbitrary().prop_map_into().boxed(),
            CastInt16ToUint64::arbitrary().prop_map_into().boxed(),
            CastInt16ToString::arbitrary().prop_map_into().boxed(),
            CastInt2VectorToArray::arbitrary().prop_map_into().boxed(),
            CastInt32ToBool::arbitrary().prop_map_into().boxed(),
            CastInt32ToFloat32::arbitrary().prop_map_into().boxed(),
            CastInt32ToFloat64::arbitrary().prop_map_into().boxed(),
            CastInt32ToOid::arbitrary().prop_map_into().boxed(),
            CastInt32ToPgLegacyChar::arbitrary().prop_map_into().boxed(),
            CastInt32ToInt16::arbitrary().prop_map_into().boxed(),
            CastInt32ToInt64::arbitrary().prop_map_into().boxed(),
            CastInt32ToUint16::arbitrary().prop_map_into().boxed(),
            CastInt32ToUint32::arbitrary().prop_map_into().boxed(),
            CastInt32ToUint64::arbitrary().prop_map_into().boxed(),
            CastInt32ToString::arbitrary().prop_map_into().boxed(),
            CastOidToInt32::arbitrary().prop_map_into().boxed(),
            CastOidToInt64::arbitrary().prop_map_into().boxed(),
            CastOidToString::arbitrary().prop_map_into().boxed(),
            CastOidToRegClass::arbitrary().prop_map_into().boxed(),
            CastRegClassToOid::arbitrary().prop_map_into().boxed(),
            CastOidToRegProc::arbitrary().prop_map_into().boxed(),
            CastRegProcToOid::arbitrary().prop_map_into().boxed(),
            CastOidToRegType::arbitrary().prop_map_into().boxed(),
            CastRegTypeToOid::arbitrary().prop_map_into().boxed(),
            CastInt64ToInt16::arbitrary().prop_map_into().boxed(),
            CastInt64ToInt32::arbitrary().prop_map_into().boxed(),
            CastInt64ToUint16::arbitrary().prop_map_into().boxed(),
            CastInt64ToUint32::arbitrary().prop_map_into().boxed(),
            CastInt64ToUint64::arbitrary().prop_map_into().boxed(),
            any::<Option<NumericMaxScale>>()
                .prop_map(|i| UnaryFunc::CastInt16ToNumeric(CastInt16ToNumeric(i)))
                .boxed(),
            any::<Option<NumericMaxScale>>()
                .prop_map(|i| UnaryFunc::CastInt32ToNumeric(CastInt32ToNumeric(i)))
                .boxed(),
            CastInt64ToBool::arbitrary().prop_map_into().boxed(),
            any::<Option<NumericMaxScale>>()
                .prop_map(|i| UnaryFunc::CastInt64ToNumeric(CastInt64ToNumeric(i)))
                .boxed(),
            CastInt64ToFloat32::arbitrary().prop_map_into().boxed(),
            CastInt64ToFloat64::arbitrary().prop_map_into().boxed(),
            CastInt64ToOid::arbitrary().prop_map_into().boxed(),
            CastInt64ToString::arbitrary().prop_map_into().boxed(),
            CastUint16ToUint32::arbitrary().prop_map_into().boxed(),
            CastUint16ToUint64::arbitrary().prop_map_into().boxed(),
            CastUint16ToInt16::arbitrary().prop_map_into().boxed(),
            CastUint16ToInt32::arbitrary().prop_map_into().boxed(),
            CastUint16ToInt64::arbitrary().prop_map_into().boxed(),
            any::<Option<NumericMaxScale>>()
                .prop_map(|i| UnaryFunc::CastUint16ToNumeric(CastUint16ToNumeric(i)))
                .boxed(),
            CastUint16ToFloat32::arbitrary().prop_map_into().boxed(),
            CastUint16ToFloat64::arbitrary().prop_map_into().boxed(),
            CastUint16ToString::arbitrary().prop_map_into().boxed(),
            CastUint32ToUint16::arbitrary().prop_map_into().boxed(),
            CastUint32ToUint64::arbitrary().prop_map_into().boxed(),
            CastUint32ToInt32::arbitrary().prop_map_into().boxed(),
            CastUint32ToInt64::arbitrary().prop_map_into().boxed(),
            any::<Option<NumericMaxScale>>()
                .prop_map(|i| UnaryFunc::CastUint32ToNumeric(CastUint32ToNumeric(i)))
                .boxed(),
            CastUint32ToFloat32::arbitrary().prop_map_into().boxed(),
            CastUint32ToFloat64::arbitrary().prop_map_into().boxed(),
            CastUint32ToString::arbitrary().prop_map_into().boxed(),
            CastUint64ToUint16::arbitrary().prop_map_into().boxed(),
            CastUint64ToUint32::arbitrary().prop_map_into().boxed(),
            CastUint64ToInt32::arbitrary().prop_map_into().boxed(),
            CastUint64ToInt64::arbitrary().prop_map_into().boxed(),
            any::<Option<NumericMaxScale>>()
                .prop_map(|i| UnaryFunc::CastUint64ToNumeric(CastUint64ToNumeric(i)))
                .boxed(),
            CastUint64ToFloat32::arbitrary().prop_map_into().boxed(),
            CastUint64ToFloat64::arbitrary().prop_map_into().boxed(),
            CastUint64ToString::arbitrary().prop_map_into().boxed(),
            CastFloat32ToInt16::arbitrary().prop_map_into().boxed(),
            CastFloat32ToInt32::arbitrary().prop_map_into().boxed(),
            CastFloat32ToInt64::arbitrary().prop_map_into().boxed(),
            CastFloat32ToUint16::arbitrary().prop_map_into().boxed(),
            CastFloat32ToUint32::arbitrary().prop_map_into().boxed(),
            CastFloat32ToUint64::arbitrary().prop_map_into().boxed(),
            CastFloat32ToFloat64::arbitrary().prop_map_into().boxed(),
            CastFloat32ToString::arbitrary().prop_map_into().boxed(),
            any::<Option<NumericMaxScale>>()
                .prop_map(|i| UnaryFunc::CastFloat32ToNumeric(CastFloat32ToNumeric(i)))
                .boxed(),
            any::<Option<NumericMaxScale>>()
                .prop_map(|i| UnaryFunc::CastFloat64ToNumeric(CastFloat64ToNumeric(i)))
                .boxed(),
            CastFloat64ToInt16::arbitrary().prop_map_into().boxed(),
            CastFloat64ToInt32::arbitrary().prop_map_into().boxed(),
            CastFloat64ToInt64::arbitrary().prop_map_into().boxed(),
            CastFloat64ToUint16::arbitrary().prop_map_into().boxed(),
            CastFloat64ToUint32::arbitrary().prop_map_into().boxed(),
            CastFloat64ToUint64::arbitrary().prop_map_into().boxed(),
            CastFloat64ToFloat32::arbitrary().prop_map_into().boxed(),
            CastFloat64ToString::arbitrary().prop_map_into().boxed(),
            CastNumericToFloat32::arbitrary().prop_map_into().boxed(),
            CastNumericToFloat64::arbitrary().prop_map_into().boxed(),
            CastNumericToInt16::arbitrary().prop_map_into().boxed(),
            CastNumericToInt32::arbitrary().prop_map_into().boxed(),
            CastNumericToInt64::arbitrary().prop_map_into().boxed(),
            CastNumericToUint16::arbitrary().prop_map_into().boxed(),
            CastNumericToUint32::arbitrary().prop_map_into().boxed(),
            CastNumericToUint64::arbitrary().prop_map_into().boxed(),
            CastNumericToString::arbitrary().prop_map_into().boxed(),
            CastStringToBool::arbitrary().prop_map_into().boxed(),
            CastStringToPgLegacyChar::arbitrary()
                .prop_map_into()
                .boxed(),
            CastStringToPgLegacyName::arbitrary()
                .prop_map_into()
                .boxed(),
            CastStringToBytes::arbitrary().prop_map_into().boxed(),
            CastStringToInt16::arbitrary().prop_map_into().boxed(),
            CastStringToInt32::arbitrary().prop_map_into().boxed(),
            CastStringToInt64::arbitrary().prop_map_into().boxed(),
            CastStringToUint16::arbitrary().prop_map_into().boxed(),
            CastStringToUint32::arbitrary().prop_map_into().boxed(),
            CastStringToUint64::arbitrary().prop_map_into().boxed(),
            CastStringToInt2Vector::arbitrary().prop_map_into().boxed(),
            CastStringToOid::arbitrary().prop_map_into().boxed(),
            CastStringToFloat32::arbitrary().prop_map_into().boxed(),
            CastStringToFloat64::arbitrary().prop_map_into().boxed(),
            CastStringToDate::arbitrary().prop_map_into().boxed(),
            (any::<ScalarType>(), any::<MirScalarExpr>())
                .prop_map(|(return_ty, expr)| {
                    UnaryFunc::CastStringToArray(CastStringToArray {
                        return_ty,
                        cast_expr: Box::new(expr),
                    })
                })
                .boxed(),
            (any::<ScalarType>(), any::<MirScalarExpr>())
                .prop_map(|(return_ty, expr)| {
                    UnaryFunc::CastStringToList(CastStringToList {
                        return_ty,
                        cast_expr: Box::new(expr),
                    })
                })
                .boxed(),
            (any::<ScalarType>(), any::<MirScalarExpr>())
                .prop_map(|(return_ty, expr)| {
                    UnaryFunc::CastStringToMap(CastStringToMap {
                        return_ty,
                        cast_expr: Box::new(expr),
                    })
                })
                .boxed(),
            (any::<ScalarType>(), any::<MirScalarExpr>())
                .prop_map(|(return_ty, expr)| {
                    UnaryFunc::CastStringToRange(CastStringToRange {
                        return_ty,
                        cast_expr: Box::new(expr),
                    })
                })
                .boxed(),
            CastStringToTime::arbitrary().prop_map_into().boxed(),
            CastStringToTimestamp::arbitrary().prop_map_into().boxed(),
            CastStringToTimestampTz::arbitrary().prop_map_into().boxed(),
            CastStringToInterval::arbitrary().prop_map_into().boxed(),
            CastStringToNumeric::arbitrary().prop_map_into().boxed(),
            CastStringToUuid::arbitrary().prop_map_into().boxed(),
            CastStringToChar::arbitrary().prop_map_into().boxed(),
            PadChar::arbitrary().prop_map_into().boxed(),
            CastStringToVarChar::arbitrary().prop_map_into().boxed(),
            CastCharToString::arbitrary().prop_map_into().boxed(),
            CastVarCharToString::arbitrary().prop_map_into().boxed(),
            CastDateToTimestamp::arbitrary().prop_map_into().boxed(),
            CastDateToTimestampTz::arbitrary().prop_map_into().boxed(),
            CastDateToString::arbitrary().prop_map_into().boxed(),
            CastTimeToInterval::arbitrary().prop_map_into().boxed(),
            CastTimeToString::arbitrary().prop_map_into().boxed(),
            CastIntervalToString::arbitrary().prop_map_into().boxed(),
            CastIntervalToTime::arbitrary().prop_map_into().boxed(),
            CastTimestampToDate::arbitrary().prop_map_into().boxed(),
            CastTimestampToTimestampTz::arbitrary()
                .prop_map_into()
                .boxed(),
            CastTimestampToString::arbitrary().prop_map_into().boxed(),
            CastTimestampToTime::arbitrary().prop_map_into().boxed(),
            CastTimestampTzToDate::arbitrary().prop_map_into().boxed(),
            CastTimestampTzToTimestamp::arbitrary()
                .prop_map_into()
                .boxed(),
            CastTimestampTzToString::arbitrary().prop_map_into().boxed(),
            CastTimestampTzToTime::arbitrary().prop_map_into().boxed(),
            CastPgLegacyCharToString::arbitrary()
                .prop_map_into()
                .boxed(),
            CastPgLegacyCharToChar::arbitrary().prop_map_into().boxed(),
            CastPgLegacyCharToVarChar::arbitrary()
                .prop_map_into()
                .boxed(),
            CastPgLegacyCharToInt32::arbitrary().prop_map_into().boxed(),
            CastBytesToString::arbitrary().prop_map_into().boxed(),
            CastStringToJsonb::arbitrary().prop_map_into().boxed(),
            CastJsonbToString::arbitrary().prop_map_into().boxed(),
            CastJsonbableToJsonb::arbitrary().prop_map_into().boxed(),
            CastJsonbToInt16::arbitrary().prop_map_into().boxed(),
            CastJsonbToInt32::arbitrary().prop_map_into().boxed(),
            CastJsonbToInt64::arbitrary().prop_map_into().boxed(),
            CastJsonbToFloat32::arbitrary().prop_map_into().boxed(),
            CastJsonbToFloat64::arbitrary().prop_map_into().boxed(),
            CastJsonbToNumeric::arbitrary().prop_map_into().boxed(),
            CastJsonbToBool::arbitrary().prop_map_into().boxed(),
            CastUuidToString::arbitrary().prop_map_into().boxed(),
            CastRecordToString::arbitrary().prop_map_into().boxed(),
            (
                any::<ScalarType>(),
                proptest::collection::vec(any::<MirScalarExpr>(), 1..5),
            )
                .prop_map(|(return_ty, cast_exprs)| {
                    UnaryFunc::CastRecord1ToRecord2(CastRecord1ToRecord2 {
                        return_ty,
                        cast_exprs: cast_exprs.into(),
                    })
                })
                .boxed(),
            CastArrayToJsonb::arbitrary().prop_map_into().boxed(),
            CastArrayToString::arbitrary().prop_map_into().boxed(),
            CastListToString::arbitrary().prop_map_into().boxed(),
            CastListToJsonb::arbitrary().prop_map_into().boxed(),
            (any::<ScalarType>(), any::<MirScalarExpr>())
                .prop_map(|(return_ty, expr)| {
                    UnaryFunc::CastList1ToList2(CastList1ToList2 {
                        return_ty,
                        cast_expr: Box::new(expr),
                    })
                })
                .boxed(),
            CastArrayToListOneDim::arbitrary().prop_map_into().boxed(),
            CastMapToString::arbitrary().prop_map_into().boxed(),
            CastInt2VectorToString::arbitrary().prop_map_into().boxed(),
            CastRangeToString::arbitrary().prop_map_into().boxed(),
            CeilFloat32::arbitrary().prop_map_into().boxed(),
            CeilFloat64::arbitrary().prop_map_into().boxed(),
            CeilNumeric::arbitrary().prop_map_into().boxed(),
            FloorFloat32::arbitrary().prop_map_into().boxed(),
            FloorFloat64::arbitrary().prop_map_into().boxed(),
            FloorNumeric::arbitrary().prop_map_into().boxed(),
            Ascii::arbitrary().prop_map_into().boxed(),
            BitCountBytes::arbitrary().prop_map_into().boxed(),
            BitLengthBytes::arbitrary().prop_map_into().boxed(),
            BitLengthString::arbitrary().prop_map_into().boxed(),
            ByteLengthBytes::arbitrary().prop_map_into().boxed(),
            ByteLengthString::arbitrary().prop_map_into().boxed(),
            CharLength::arbitrary().prop_map_into().boxed(),
            Chr::arbitrary().prop_map_into().boxed(),
            like_pattern::any_matcher()
                .prop_map(|matcher| UnaryFunc::IsLikeMatch(IsLikeMatch(matcher)))
                .boxed(),
            any_regex()
                .prop_map(|regex| UnaryFunc::IsRegexpMatch(IsRegexpMatch(regex)))
                .boxed(),
            any_regex()
                .prop_map(|regex| UnaryFunc::RegexpMatch(RegexpMatch(regex)))
                .boxed(),
            any_regex()
                .prop_map(|regex| UnaryFunc::RegexpSplitToArray(RegexpSplitToArray(regex)))
                .boxed(),
            ExtractInterval::arbitrary().prop_map_into().boxed(),
            ExtractTime::arbitrary().prop_map_into().boxed(),
            ExtractTimestamp::arbitrary().prop_map_into().boxed(),
            ExtractTimestampTz::arbitrary().prop_map_into().boxed(),
            ExtractDate::arbitrary().prop_map_into().boxed(),
            DatePartInterval::arbitrary().prop_map_into().boxed(),
            DatePartTime::arbitrary().prop_map_into().boxed(),
            DatePartTimestamp::arbitrary().prop_map_into().boxed(),
            DatePartTimestampTz::arbitrary().prop_map_into().boxed(),
            DateTruncTimestamp::arbitrary().prop_map_into().boxed(),
            DateTruncTimestampTz::arbitrary().prop_map_into().boxed(),
            TimezoneTimestamp::arbitrary().prop_map_into().boxed(),
            TimezoneTimestampTz::arbitrary().prop_map_into().boxed(),
            TimezoneTime::arbitrary().prop_map_into().boxed(),
            ToTimestamp::arbitrary().prop_map_into().boxed(),
            JustifyDays::arbitrary().prop_map_into().boxed(),
            JustifyHours::arbitrary().prop_map_into().boxed(),
            JustifyInterval::arbitrary().prop_map_into().boxed(),
            JsonbArrayLength::arbitrary().prop_map_into().boxed(),
            JsonbTypeof::arbitrary().prop_map_into().boxed(),
            JsonbStripNulls::arbitrary().prop_map_into().boxed(),
            JsonbPretty::arbitrary().prop_map_into().boxed(),
            RoundFloat32::arbitrary().prop_map_into().boxed(),
            RoundFloat64::arbitrary().prop_map_into().boxed(),
            RoundNumeric::arbitrary().prop_map_into().boxed(),
            TruncFloat32::arbitrary().prop_map_into().boxed(),
            TruncFloat64::arbitrary().prop_map_into().boxed(),
            TruncNumeric::arbitrary().prop_map_into().boxed(),
            TrimWhitespace::arbitrary().prop_map_into().boxed(),
            TrimLeadingWhitespace::arbitrary().prop_map_into().boxed(),
            TrimTrailingWhitespace::arbitrary().prop_map_into().boxed(),
            RecordGet::arbitrary().prop_map_into().boxed(),
            ListLength::arbitrary().prop_map_into().boxed(),
            (any::<ScalarType>())
                .prop_map(|value_type| {
                    UnaryFunc::MapBuildFromRecordList(MapBuildFromRecordList { value_type })
                })
                .boxed(),
            MapLength::arbitrary().prop_map_into().boxed(),
            Upper::arbitrary().prop_map_into().boxed(),
            Lower::arbitrary().prop_map_into().boxed(),
            Cos::arbitrary().prop_map_into().boxed(),
            Acos::arbitrary().prop_map_into().boxed(),
            Cosh::arbitrary().prop_map_into().boxed(),
            Acosh::arbitrary().prop_map_into().boxed(),
            Sin::arbitrary().prop_map_into().boxed(),
            Asin::arbitrary().prop_map_into().boxed(),
            Sinh::arbitrary().prop_map_into().boxed(),
            Asinh::arbitrary().prop_map_into().boxed(),
            Tan::arbitrary().prop_map_into().boxed(),
            Atan::arbitrary().prop_map_into().boxed(),
            Tanh::arbitrary().prop_map_into().boxed(),
            Atanh::arbitrary().prop_map_into().boxed(),
            Cot::arbitrary().prop_map_into().boxed(),
            Degrees::arbitrary().prop_map_into().boxed(),
            Radians::arbitrary().prop_map_into().boxed(),
            Log10::arbitrary().prop_map_into().boxed(),
            Log10Numeric::arbitrary().prop_map_into().boxed(),
            Ln::arbitrary().prop_map_into().boxed(),
            LnNumeric::arbitrary().prop_map_into().boxed(),
            Exp::arbitrary().prop_map_into().boxed(),
            ExpNumeric::arbitrary().prop_map_into().boxed(),
            Sleep::arbitrary().prop_map_into().boxed(),
            Panic::arbitrary().prop_map_into().boxed(),
            AdjustNumericScale::arbitrary().prop_map_into().boxed(),
            PgColumnSize::arbitrary().prop_map_into().boxed(),
            PgSizePretty::arbitrary().prop_map_into().boxed(),
            MzRowSize::arbitrary().prop_map_into().boxed(),
            MzTypeName::arbitrary().prop_map_into().boxed(),
            RangeLower::arbitrary().prop_map_into().boxed(),
            RangeUpper::arbitrary().prop_map_into().boxed(),
            RangeEmpty::arbitrary().prop_map_into().boxed(),
            RangeLowerInc::arbitrary().prop_map_into().boxed(),
            RangeUpperInc::arbitrary().prop_map_into().boxed(),
            RangeLowerInf::arbitrary().prop_map_into().boxed(),
            RangeUpperInf::arbitrary().prop_map_into().boxed(),
            MzAclItemGrantor::arbitrary().prop_map_into().boxed(),
            MzAclItemGrantee::arbitrary().prop_map_into().boxed(),
            MzAclItemPrivileges::arbitrary().prop_map_into().boxed(),
            MzFormatPrivileges::arbitrary().prop_map_into().boxed(),
            MzValidatePrivileges::arbitrary().prop_map_into().boxed(),
            MzValidateRolePrivilege::arbitrary().prop_map_into().boxed(),
            AclItemGrantor::arbitrary().prop_map_into().boxed(),
            AclItemGrantee::arbitrary().prop_map_into().boxed(),
            AclItemPrivileges::arbitrary().prop_map_into().boxed(),
            QuoteIdent::arbitrary().prop_map_into().boxed(),
        ])
    }
}

impl RustType<ProtoUnaryFunc> for UnaryFunc {
    fn into_proto(&self) -> ProtoUnaryFunc {
        use crate::scalar::proto_unary_func::Kind::*;
        use crate::scalar::proto_unary_func::*;
        let kind = match self {
            UnaryFunc::Not(_) => Not(()),
            UnaryFunc::IsNull(_) => IsNull(()),
            UnaryFunc::IsTrue(_) => IsTrue(()),
            UnaryFunc::IsFalse(_) => IsFalse(()),
            UnaryFunc::BitNotInt16(_) => BitNotInt16(()),
            UnaryFunc::BitNotInt32(_) => BitNotInt32(()),
            UnaryFunc::BitNotInt64(_) => BitNotInt64(()),
            UnaryFunc::BitNotUint16(_) => BitNotUint16(()),
            UnaryFunc::BitNotUint32(_) => BitNotUint32(()),
            UnaryFunc::BitNotUint64(_) => BitNotUint64(()),
            UnaryFunc::NegInt16(_) => NegInt16(()),
            UnaryFunc::NegInt32(_) => NegInt32(()),
            UnaryFunc::NegInt64(_) => NegInt64(()),
            UnaryFunc::NegFloat32(_) => NegFloat32(()),
            UnaryFunc::NegFloat64(_) => NegFloat64(()),
            UnaryFunc::NegNumeric(_) => NegNumeric(()),
            UnaryFunc::NegInterval(_) => NegInterval(()),
            UnaryFunc::SqrtFloat64(_) => SqrtFloat64(()),
            UnaryFunc::SqrtNumeric(_) => SqrtNumeric(()),
            UnaryFunc::CbrtFloat64(_) => CbrtFloat64(()),
            UnaryFunc::AbsInt16(_) => AbsInt16(()),
            UnaryFunc::AbsInt32(_) => AbsInt32(()),
            UnaryFunc::AbsInt64(_) => AbsInt64(()),
            UnaryFunc::AbsFloat32(_) => AbsFloat32(()),
            UnaryFunc::AbsFloat64(_) => AbsFloat64(()),
            UnaryFunc::AbsNumeric(_) => AbsNumeric(()),
            UnaryFunc::CastBoolToString(_) => CastBoolToString(()),
            UnaryFunc::CastBoolToStringNonstandard(_) => CastBoolToStringNonstandard(()),
            UnaryFunc::CastBoolToInt32(_) => CastBoolToInt32(()),
            UnaryFunc::CastBoolToInt64(_) => CastBoolToInt64(()),
            UnaryFunc::CastInt16ToFloat32(_) => CastInt16ToFloat32(()),
            UnaryFunc::CastInt16ToFloat64(_) => CastInt16ToFloat64(()),
            UnaryFunc::CastInt16ToInt32(_) => CastInt16ToInt32(()),
            UnaryFunc::CastInt16ToInt64(_) => CastInt16ToInt64(()),
            UnaryFunc::CastInt16ToUint16(_) => CastInt16ToUint16(()),
            UnaryFunc::CastInt16ToUint32(_) => CastInt16ToUint32(()),
            UnaryFunc::CastInt16ToUint64(_) => CastInt16ToUint64(()),
            UnaryFunc::CastInt16ToString(_) => CastInt16ToString(()),
            UnaryFunc::CastInt2VectorToArray(_) => CastInt2VectorToArray(()),
            UnaryFunc::CastInt32ToBool(_) => CastInt32ToBool(()),
            UnaryFunc::CastInt32ToFloat32(_) => CastInt32ToFloat32(()),
            UnaryFunc::CastInt32ToFloat64(_) => CastInt32ToFloat64(()),
            UnaryFunc::CastInt32ToOid(_) => CastInt32ToOid(()),
            UnaryFunc::CastInt32ToPgLegacyChar(_) => CastInt32ToPgLegacyChar(()),
            UnaryFunc::CastInt32ToInt16(_) => CastInt32ToInt16(()),
            UnaryFunc::CastInt32ToInt64(_) => CastInt32ToInt64(()),
            UnaryFunc::CastInt32ToUint16(_) => CastInt32ToUint16(()),
            UnaryFunc::CastInt32ToUint32(_) => CastInt32ToUint32(()),
            UnaryFunc::CastInt32ToUint64(_) => CastInt32ToUint64(()),
            UnaryFunc::CastInt32ToString(_) => CastInt32ToString(()),
            UnaryFunc::CastOidToInt32(_) => CastOidToInt32(()),
            UnaryFunc::CastOidToInt64(_) => CastOidToInt64(()),
            UnaryFunc::CastOidToString(_) => CastOidToString(()),
            UnaryFunc::CastOidToRegClass(_) => CastOidToRegClass(()),
            UnaryFunc::CastRegClassToOid(_) => CastRegClassToOid(()),
            UnaryFunc::CastOidToRegProc(_) => CastOidToRegProc(()),
            UnaryFunc::CastRegProcToOid(_) => CastRegProcToOid(()),
            UnaryFunc::CastOidToRegType(_) => CastOidToRegType(()),
            UnaryFunc::CastRegTypeToOid(_) => CastRegTypeToOid(()),
            UnaryFunc::CastInt64ToInt16(_) => CastInt64ToInt16(()),
            UnaryFunc::CastInt64ToInt32(_) => CastInt64ToInt32(()),
            UnaryFunc::CastInt64ToUint16(_) => CastInt64ToUint16(()),
            UnaryFunc::CastInt64ToUint32(_) => CastInt64ToUint32(()),
            UnaryFunc::CastInt64ToUint64(_) => CastInt64ToUint64(()),
            UnaryFunc::CastInt16ToNumeric(func) => CastInt16ToNumeric(func.0.into_proto()),
            UnaryFunc::CastInt32ToNumeric(func) => CastInt32ToNumeric(func.0.into_proto()),
            UnaryFunc::CastInt64ToBool(_) => CastInt64ToBool(()),
            UnaryFunc::CastInt64ToNumeric(func) => CastInt64ToNumeric(func.0.into_proto()),
            UnaryFunc::CastInt64ToFloat32(_) => CastInt64ToFloat32(()),
            UnaryFunc::CastInt64ToFloat64(_) => CastInt64ToFloat64(()),
            UnaryFunc::CastInt64ToOid(_) => CastInt64ToOid(()),
            UnaryFunc::CastInt64ToString(_) => CastInt64ToString(()),
            UnaryFunc::CastUint16ToUint32(_) => CastUint16ToUint32(()),
            UnaryFunc::CastUint16ToUint64(_) => CastUint16ToUint64(()),
            UnaryFunc::CastUint16ToInt16(_) => CastUint16ToInt16(()),
            UnaryFunc::CastUint16ToInt32(_) => CastUint16ToInt32(()),
            UnaryFunc::CastUint16ToInt64(_) => CastUint16ToInt64(()),
            UnaryFunc::CastUint16ToNumeric(func) => CastUint16ToNumeric(func.0.into_proto()),
            UnaryFunc::CastUint16ToFloat32(_) => CastUint16ToFloat32(()),
            UnaryFunc::CastUint16ToFloat64(_) => CastUint16ToFloat64(()),
            UnaryFunc::CastUint16ToString(_) => CastUint16ToString(()),
            UnaryFunc::CastUint32ToUint16(_) => CastUint32ToUint16(()),
            UnaryFunc::CastUint32ToUint64(_) => CastUint32ToUint64(()),
            UnaryFunc::CastUint32ToInt16(_) => CastUint32ToInt16(()),
            UnaryFunc::CastUint32ToInt32(_) => CastUint32ToInt32(()),
            UnaryFunc::CastUint32ToInt64(_) => CastUint32ToInt64(()),
            UnaryFunc::CastUint32ToNumeric(func) => CastUint32ToNumeric(func.0.into_proto()),
            UnaryFunc::CastUint32ToFloat32(_) => CastUint32ToFloat32(()),
            UnaryFunc::CastUint32ToFloat64(_) => CastUint32ToFloat64(()),
            UnaryFunc::CastUint32ToString(_) => CastUint32ToString(()),
            UnaryFunc::CastUint64ToUint16(_) => CastUint64ToUint16(()),
            UnaryFunc::CastUint64ToUint32(_) => CastUint64ToUint32(()),
            UnaryFunc::CastUint64ToInt16(_) => CastUint64ToInt16(()),
            UnaryFunc::CastUint64ToInt32(_) => CastUint64ToInt32(()),
            UnaryFunc::CastUint64ToInt64(_) => CastUint64ToInt64(()),
            UnaryFunc::CastUint64ToNumeric(func) => CastUint64ToNumeric(func.0.into_proto()),
            UnaryFunc::CastUint64ToFloat32(_) => CastUint64ToFloat32(()),
            UnaryFunc::CastUint64ToFloat64(_) => CastUint64ToFloat64(()),
            UnaryFunc::CastUint64ToString(_) => CastUint64ToString(()),
            UnaryFunc::CastFloat32ToInt16(_) => CastFloat32ToInt16(()),
            UnaryFunc::CastFloat32ToInt32(_) => CastFloat32ToInt32(()),
            UnaryFunc::CastFloat32ToInt64(_) => CastFloat32ToInt64(()),
            UnaryFunc::CastFloat32ToUint16(_) => CastFloat32ToUint16(()),
            UnaryFunc::CastFloat32ToUint32(_) => CastFloat32ToUint32(()),
            UnaryFunc::CastFloat32ToUint64(_) => CastFloat32ToUint64(()),
            UnaryFunc::CastFloat32ToFloat64(_) => CastFloat32ToFloat64(()),
            UnaryFunc::CastFloat32ToString(_) => CastFloat32ToString(()),
            UnaryFunc::CastFloat32ToNumeric(func) => CastFloat32ToNumeric(func.0.into_proto()),
            UnaryFunc::CastFloat64ToNumeric(func) => CastFloat64ToNumeric(func.0.into_proto()),
            UnaryFunc::CastFloat64ToInt16(_) => CastFloat64ToInt16(()),
            UnaryFunc::CastFloat64ToInt32(_) => CastFloat64ToInt32(()),
            UnaryFunc::CastFloat64ToInt64(_) => CastFloat64ToInt64(()),
            UnaryFunc::CastFloat64ToUint16(_) => CastFloat64ToUint16(()),
            UnaryFunc::CastFloat64ToUint32(_) => CastFloat64ToUint32(()),
            UnaryFunc::CastFloat64ToUint64(_) => CastFloat64ToUint64(()),
            UnaryFunc::CastFloat64ToFloat32(_) => CastFloat64ToFloat32(()),
            UnaryFunc::CastFloat64ToString(_) => CastFloat64ToString(()),
            UnaryFunc::CastNumericToFloat32(_) => CastNumericToFloat32(()),
            UnaryFunc::CastNumericToFloat64(_) => CastNumericToFloat64(()),
            UnaryFunc::CastNumericToInt16(_) => CastNumericToInt16(()),
            UnaryFunc::CastNumericToInt32(_) => CastNumericToInt32(()),
            UnaryFunc::CastNumericToInt64(_) => CastNumericToInt64(()),
            UnaryFunc::CastNumericToUint16(_) => CastNumericToUint16(()),
            UnaryFunc::CastNumericToUint32(_) => CastNumericToUint32(()),
            UnaryFunc::CastNumericToUint64(_) => CastNumericToUint64(()),
            UnaryFunc::CastNumericToString(_) => CastNumericToString(()),
            UnaryFunc::CastStringToBool(_) => CastStringToBool(()),
            UnaryFunc::CastStringToPgLegacyChar(_) => CastStringToPgLegacyChar(()),
            UnaryFunc::CastStringToPgLegacyName(_) => CastStringToPgLegacyName(()),
            UnaryFunc::CastStringToBytes(_) => CastStringToBytes(()),
            UnaryFunc::CastStringToInt16(_) => CastStringToInt16(()),
            UnaryFunc::CastStringToInt32(_) => CastStringToInt32(()),
            UnaryFunc::CastStringToInt64(_) => CastStringToInt64(()),
            UnaryFunc::CastStringToUint16(_) => CastStringToUint16(()),
            UnaryFunc::CastStringToUint32(_) => CastStringToUint32(()),
            UnaryFunc::CastStringToUint64(_) => CastStringToUint64(()),
            UnaryFunc::CastStringToInt2Vector(_) => CastStringToInt2Vector(()),
            UnaryFunc::CastStringToOid(_) => CastStringToOid(()),
            UnaryFunc::CastStringToFloat32(_) => CastStringToFloat32(()),
            UnaryFunc::CastStringToFloat64(_) => CastStringToFloat64(()),
            UnaryFunc::CastStringToDate(_) => CastStringToDate(()),
            UnaryFunc::CastStringToArray(inner) => {
                CastStringToArray(Box::new(ProtoCastToVariableType {
                    return_ty: Some(inner.return_ty.into_proto()),
                    cast_expr: Some(inner.cast_expr.into_proto()),
                }))
            }
            UnaryFunc::CastStringToList(inner) => {
                CastStringToList(Box::new(ProtoCastToVariableType {
                    return_ty: Some(inner.return_ty.into_proto()),
                    cast_expr: Some(inner.cast_expr.into_proto()),
                }))
            }
            UnaryFunc::CastStringToMap(inner) => {
                CastStringToMap(Box::new(ProtoCastToVariableType {
                    return_ty: Some(inner.return_ty.into_proto()),
                    cast_expr: Some(inner.cast_expr.into_proto()),
                }))
            }
            UnaryFunc::CastStringToRange(inner) => {
                CastStringToRange(Box::new(ProtoCastToVariableType {
                    return_ty: Some(inner.return_ty.into_proto()),
                    cast_expr: Some(inner.cast_expr.into_proto()),
                }))
            }
            UnaryFunc::CastStringToTime(_) => CastStringToTime(()),
            UnaryFunc::CastStringToTimestamp(precision) => {
                CastStringToTimestamp(precision.0.into_proto())
            }
            UnaryFunc::CastStringToTimestampTz(precision) => {
                CastStringToTimestampTz(precision.0.into_proto())
            }
            UnaryFunc::CastStringToInterval(_) => CastStringToInterval(()),
            UnaryFunc::CastStringToNumeric(func) => CastStringToNumeric(func.0.into_proto()),
            UnaryFunc::CastStringToUuid(_) => CastStringToUuid(()),
            UnaryFunc::CastStringToChar(func) => CastStringToChar(ProtoCastStringToChar {
                length: func.length.into_proto(),
                fail_on_len: func.fail_on_len,
            }),
            UnaryFunc::PadChar(func) => PadChar(ProtoPadChar {
                length: func.length.into_proto(),
            }),
            UnaryFunc::CastStringToVarChar(func) => CastStringToVarChar(ProtoCastStringToVarChar {
                length: func.length.into_proto(),
                fail_on_len: func.fail_on_len,
            }),
            UnaryFunc::CastCharToString(_) => CastCharToString(()),
            UnaryFunc::CastVarCharToString(_) => CastVarCharToString(()),
            UnaryFunc::CastDateToTimestamp(func) => CastDateToTimestamp(func.0.into_proto()),
            UnaryFunc::CastDateToTimestampTz(func) => CastDateToTimestampTz(func.0.into_proto()),
            UnaryFunc::CastDateToString(_) => CastDateToString(()),
            UnaryFunc::CastTimeToInterval(_) => CastTimeToInterval(()),
            UnaryFunc::CastTimeToString(_) => CastTimeToString(()),
            UnaryFunc::CastIntervalToString(_) => CastIntervalToString(()),
            UnaryFunc::CastIntervalToTime(_) => CastIntervalToTime(()),
            UnaryFunc::CastTimestampToDate(_) => CastTimestampToDate(()),
            UnaryFunc::AdjustTimestampPrecision(func) => Kind::AdjustTimestampPrecision(
                mz_repr::adt::timestamp::ProtoFromToTimestampPrecisions {
                    from: func.from.map(|p| p.into_proto()),
                    to: func.to.map(|p| p.into_proto()),
                },
            ),
            UnaryFunc::CastTimestampToTimestampTz(func) => CastTimestampToTimestampTz(
                mz_repr::adt::timestamp::ProtoFromToTimestampPrecisions {
                    from: func.from.map(|p| p.into_proto()),
                    to: func.to.map(|p| p.into_proto()),
                },
            ),
            UnaryFunc::CastTimestampToString(_) => CastTimestampToString(()),
            UnaryFunc::CastTimestampToTime(_) => CastTimestampToTime(()),
            UnaryFunc::CastTimestampTzToDate(_) => CastTimestampTzToDate(()),
            UnaryFunc::AdjustTimestampTzPrecision(func) => Kind::AdjustTimestampTzPrecision(
                mz_repr::adt::timestamp::ProtoFromToTimestampPrecisions {
                    from: func.from.map(|p| p.into_proto()),
                    to: func.to.map(|p| p.into_proto()),
                },
            ),
            UnaryFunc::CastTimestampTzToTimestamp(func) => CastTimestampTzToTimestamp(
                mz_repr::adt::timestamp::ProtoFromToTimestampPrecisions {
                    from: func.from.map(|p| p.into_proto()),
                    to: func.to.map(|p| p.into_proto()),
                },
            ),
            UnaryFunc::CastTimestampTzToString(_) => CastTimestampTzToString(()),
            UnaryFunc::CastTimestampTzToTime(_) => CastTimestampTzToTime(()),
            UnaryFunc::CastPgLegacyCharToString(_) => CastPgLegacyCharToString(()),
            UnaryFunc::CastPgLegacyCharToChar(_) => CastPgLegacyCharToChar(()),
            UnaryFunc::CastPgLegacyCharToVarChar(_) => CastPgLegacyCharToVarChar(()),
            UnaryFunc::CastPgLegacyCharToInt32(_) => CastPgLegacyCharToInt32(()),
            UnaryFunc::CastBytesToString(_) => CastBytesToString(()),
            UnaryFunc::CastStringToJsonb(_) => CastStringToJsonb(()),
            UnaryFunc::CastJsonbToString(_) => CastJsonbToString(()),
            UnaryFunc::CastJsonbableToJsonb(_) => CastJsonbableToJsonb(()),
            UnaryFunc::CastJsonbToInt16(_) => CastJsonbToInt16(()),
            UnaryFunc::CastJsonbToInt32(_) => CastJsonbToInt32(()),
            UnaryFunc::CastJsonbToInt64(_) => CastJsonbToInt64(()),
            UnaryFunc::CastJsonbToFloat32(_) => CastJsonbToFloat32(()),
            UnaryFunc::CastJsonbToFloat64(_) => CastJsonbToFloat64(()),
            UnaryFunc::CastJsonbToNumeric(func) => CastJsonbToNumeric(func.0.into_proto()),
            UnaryFunc::CastJsonbToBool(_) => CastJsonbToBool(()),
            UnaryFunc::CastUuidToString(_) => CastUuidToString(()),
            UnaryFunc::CastRecordToString(func) => CastRecordToString(func.ty.into_proto()),
            UnaryFunc::CastRecord1ToRecord2(inner) => {
                CastRecord1ToRecord2(ProtoCastRecord1ToRecord2 {
                    return_ty: Some(inner.return_ty.into_proto()),
                    cast_exprs: inner.cast_exprs.into_proto(),
                })
            }
            UnaryFunc::CastArrayToArray(inner) => {
                CastArrayToArray(Box::new(ProtoCastToVariableType {
                    return_ty: Some(inner.return_ty.into_proto()),
                    cast_expr: Some(inner.cast_expr.into_proto()),
                }))
            }
            UnaryFunc::CastArrayToJsonb(inner) => CastArrayToJsonb(inner.cast_element.into_proto()),
            UnaryFunc::CastArrayToString(func) => CastArrayToString(func.ty.into_proto()),
            UnaryFunc::CastListToJsonb(inner) => CastListToJsonb(inner.cast_element.into_proto()),
            UnaryFunc::CastListToString(func) => CastListToString(func.ty.into_proto()),
            UnaryFunc::CastList1ToList2(inner) => {
                CastList1ToList2(Box::new(ProtoCastToVariableType {
                    return_ty: Some(inner.return_ty.into_proto()),
                    cast_expr: Some(inner.cast_expr.into_proto()),
                }))
            }
            UnaryFunc::CastArrayToListOneDim(_) => CastArrayToListOneDim(()),
            UnaryFunc::CastMapToString(func) => CastMapToString(func.ty.into_proto()),
            UnaryFunc::CastInt2VectorToString(_) => CastInt2VectorToString(()),
            UnaryFunc::CastRangeToString(func) => CastRangeToString(func.ty.into_proto()),
            UnaryFunc::CeilFloat32(_) => CeilFloat32(()),
            UnaryFunc::CeilFloat64(_) => CeilFloat64(()),
            UnaryFunc::CeilNumeric(_) => CeilNumeric(()),
            UnaryFunc::FloorFloat32(_) => FloorFloat32(()),
            UnaryFunc::FloorFloat64(_) => FloorFloat64(()),
            UnaryFunc::FloorNumeric(_) => FloorNumeric(()),
            UnaryFunc::Ascii(_) => Ascii(()),
            UnaryFunc::BitCountBytes(_) => BitCountBytes(()),
            UnaryFunc::BitLengthBytes(_) => BitLengthBytes(()),
            UnaryFunc::BitLengthString(_) => BitLengthString(()),
            UnaryFunc::ByteLengthBytes(_) => ByteLengthBytes(()),
            UnaryFunc::ByteLengthString(_) => ByteLengthString(()),
            UnaryFunc::CharLength(_) => CharLength(()),
            UnaryFunc::Chr(_) => Chr(()),
            UnaryFunc::IsLikeMatch(pattern) => IsLikeMatch(pattern.0.into_proto()),
            UnaryFunc::IsRegexpMatch(regex) => IsRegexpMatch(regex.0.into_proto()),
            UnaryFunc::RegexpMatch(regex) => RegexpMatch(regex.0.into_proto()),
            UnaryFunc::RegexpSplitToArray(regex) => RegexpSplitToArray(regex.0.into_proto()),
            UnaryFunc::ExtractInterval(func) => ExtractInterval(func.0.into_proto()),
            UnaryFunc::ExtractTime(func) => ExtractTime(func.0.into_proto()),
            UnaryFunc::ExtractTimestamp(func) => ExtractTimestamp(func.0.into_proto()),
            UnaryFunc::ExtractTimestampTz(func) => ExtractTimestampTz(func.0.into_proto()),
            UnaryFunc::ExtractDate(func) => ExtractDate(func.0.into_proto()),
            UnaryFunc::DatePartInterval(func) => DatePartInterval(func.0.into_proto()),
            UnaryFunc::DatePartTime(func) => DatePartTime(func.0.into_proto()),
            UnaryFunc::DatePartTimestamp(func) => DatePartTimestamp(func.0.into_proto()),
            UnaryFunc::DatePartTimestampTz(func) => DatePartTimestampTz(func.0.into_proto()),
            UnaryFunc::DateTruncTimestamp(func) => DateTruncTimestamp(func.0.into_proto()),
            UnaryFunc::DateTruncTimestampTz(func) => DateTruncTimestampTz(func.0.into_proto()),
            UnaryFunc::TimezoneTimestamp(func) => TimezoneTimestamp(func.0.into_proto()),
            UnaryFunc::TimezoneTimestampTz(func) => TimezoneTimestampTz(func.0.into_proto()),
            UnaryFunc::TimezoneTime(func) => TimezoneTime(ProtoTimezoneTime {
                tz: Some(func.tz.into_proto()),
                wall_time: Some(func.wall_time.into_proto()),
            }),
            UnaryFunc::ToTimestamp(_) => ToTimestamp(()),
            UnaryFunc::ToCharTimestamp(func) => ToCharTimestamp(ProtoToCharTimestamp {
                format_string: func.format_string.into_proto(),
                format: Some(func.format.into_proto()),
            }),
            UnaryFunc::ToCharTimestampTz(func) => ToCharTimestampTz(ProtoToCharTimestamp {
                format_string: func.format_string.into_proto(),
                format: Some(func.format.into_proto()),
            }),
            UnaryFunc::JustifyDays(_) => JustifyDays(()),
            UnaryFunc::JustifyHours(_) => JustifyHours(()),
            UnaryFunc::JustifyInterval(_) => JustifyInterval(()),
            UnaryFunc::JsonbArrayLength(_) => JsonbArrayLength(()),
            UnaryFunc::JsonbTypeof(_) => JsonbTypeof(()),
            UnaryFunc::JsonbStripNulls(_) => JsonbStripNulls(()),
            UnaryFunc::JsonbPretty(_) => JsonbPretty(()),
            UnaryFunc::RoundFloat32(_) => RoundFloat32(()),
            UnaryFunc::RoundFloat64(_) => RoundFloat64(()),
            UnaryFunc::RoundNumeric(_) => RoundNumeric(()),
            UnaryFunc::TruncFloat32(_) => TruncFloat32(()),
            UnaryFunc::TruncFloat64(_) => TruncFloat64(()),
            UnaryFunc::TruncNumeric(_) => TruncNumeric(()),
            UnaryFunc::TrimWhitespace(_) => TrimWhitespace(()),
            UnaryFunc::TrimLeadingWhitespace(_) => TrimLeadingWhitespace(()),
            UnaryFunc::TrimTrailingWhitespace(_) => TrimTrailingWhitespace(()),
            UnaryFunc::Initcap(_) => Initcap(()),
            UnaryFunc::RecordGet(func) => RecordGet(func.0.into_proto()),
            UnaryFunc::ListLength(_) => ListLength(()),
            UnaryFunc::MapBuildFromRecordList(inner) => {
                MapBuildFromRecordList(inner.value_type.into_proto())
            }
            UnaryFunc::MapLength(_) => MapLength(()),
            UnaryFunc::Upper(_) => Upper(()),
            UnaryFunc::Lower(_) => Lower(()),
            UnaryFunc::Cos(_) => Cos(()),
            UnaryFunc::Acos(_) => Acos(()),
            UnaryFunc::Cosh(_) => Cosh(()),
            UnaryFunc::Acosh(_) => Acosh(()),
            UnaryFunc::Sin(_) => Sin(()),
            UnaryFunc::Asin(_) => Asin(()),
            UnaryFunc::Sinh(_) => Sinh(()),
            UnaryFunc::Asinh(_) => Asinh(()),
            UnaryFunc::Tan(_) => Tan(()),
            UnaryFunc::Atan(_) => Atan(()),
            UnaryFunc::Tanh(_) => Tanh(()),
            UnaryFunc::Atanh(_) => Atanh(()),
            UnaryFunc::Cot(_) => Cot(()),
            UnaryFunc::Degrees(_) => Degrees(()),
            UnaryFunc::Radians(_) => Radians(()),
            UnaryFunc::Log10(_) => Log10(()),
            UnaryFunc::Log10Numeric(_) => Log10Numeric(()),
            UnaryFunc::Ln(_) => Ln(()),
            UnaryFunc::LnNumeric(_) => LnNumeric(()),
            UnaryFunc::Exp(_) => Exp(()),
            UnaryFunc::ExpNumeric(_) => ExpNumeric(()),
            UnaryFunc::Sleep(_) => Sleep(()),
            UnaryFunc::Panic(_) => Panic(()),
            UnaryFunc::AdjustNumericScale(func) => AdjustNumericScale(func.0.into_proto()),
            UnaryFunc::PgColumnSize(_) => PgColumnSize(()),
            UnaryFunc::PgSizePretty(_) => PgSizePretty(()),
            UnaryFunc::MzRowSize(_) => MzRowSize(()),
            UnaryFunc::MzTypeName(_) => MzTypeName(()),
            UnaryFunc::CastMzTimestampToString(_) => CastMzTimestampToString(()),
            UnaryFunc::CastMzTimestampToTimestamp(_) => CastMzTimestampToTimestamp(()),
            UnaryFunc::CastMzTimestampToTimestampTz(_) => CastMzTimestampToTimestampTz(()),
            UnaryFunc::CastStringToMzTimestamp(_) => CastStringToMzTimestamp(()),
            UnaryFunc::CastUint64ToMzTimestamp(_) => CastUint64ToMzTimestamp(()),
            UnaryFunc::CastUint32ToMzTimestamp(_) => CastUint32ToMzTimestamp(()),
            UnaryFunc::CastInt64ToMzTimestamp(_) => CastInt64ToMzTimestamp(()),
            UnaryFunc::CastInt32ToMzTimestamp(_) => CastInt32ToMzTimestamp(()),
            UnaryFunc::CastNumericToMzTimestamp(_) => CastNumericToMzTimestamp(()),
            UnaryFunc::CastTimestampToMzTimestamp(_) => CastTimestampToMzTimestamp(()),
            UnaryFunc::CastTimestampTzToMzTimestamp(_) => CastTimestampTzToMzTimestamp(()),
            UnaryFunc::CastDateToMzTimestamp(_) => CastDateToMzTimestamp(()),
            UnaryFunc::StepMzTimestamp(_) => StepMzTimestamp(()),
            UnaryFunc::RangeLower(_) => RangeLower(()),
            UnaryFunc::RangeUpper(_) => RangeUpper(()),
            UnaryFunc::RangeEmpty(_) => RangeEmpty(()),
            UnaryFunc::RangeLowerInc(_) => RangeLowerInc(()),
            UnaryFunc::RangeUpperInc(_) => RangeUpperInc(()),
            UnaryFunc::RangeLowerInf(_) => RangeLowerInf(()),
            UnaryFunc::RangeUpperInf(_) => RangeUpperInf(()),
            UnaryFunc::MzAclItemGrantor(_) => MzAclItemGrantor(()),
            UnaryFunc::MzAclItemGrantee(_) => MzAclItemGrantee(()),
            UnaryFunc::MzAclItemPrivileges(_) => MzAclItemPrivileges(()),
            UnaryFunc::MzFormatPrivileges(_) => MzFormatPrivileges(()),
            UnaryFunc::MzValidatePrivileges(_) => MzValidatePrivileges(()),
            UnaryFunc::MzValidateRolePrivilege(_) => MzValidateRolePrivilege(()),
            UnaryFunc::AclItemGrantor(_) => AclItemGrantor(()),
            UnaryFunc::AclItemGrantee(_) => AclItemGrantee(()),
            UnaryFunc::AclItemPrivileges(_) => AclItemPrivileges(()),
            UnaryFunc::QuoteIdent(_) => QuoteIdent(()),
            UnaryFunc::TryParseMonotonicIso8601Timestamp(_) => {
                TryParseMonotonicIso8601Timestamp(())
            }
            UnaryFunc::Crc32Bytes(_) => Crc32Bytes(()),
            UnaryFunc::Crc32String(_) => Crc32String(()),
            UnaryFunc::KafkaMurmur2Bytes(_) => KafkaMurmur2Bytes(()),
            UnaryFunc::KafkaMurmur2String(_) => KafkaMurmur2String(()),
            UnaryFunc::SeahashBytes(_) => SeahashBytes(()),
            UnaryFunc::SeahashString(_) => SeahashString(()),
            UnaryFunc::Reverse(_) => Reverse(()),
        };
        ProtoUnaryFunc { kind: Some(kind) }
    }

    fn from_proto(proto: ProtoUnaryFunc) -> Result<Self, TryFromProtoError> {
        use crate::scalar::proto_unary_func::Kind::*;
        if let Some(kind) = proto.kind {
            match kind {
                Not(()) => Ok(impls::Not.into()),
                IsNull(()) => Ok(impls::IsNull.into()),
                IsTrue(()) => Ok(impls::IsTrue.into()),
                IsFalse(()) => Ok(impls::IsFalse.into()),
                BitNotInt16(()) => Ok(impls::BitNotInt16.into()),
                BitNotInt32(()) => Ok(impls::BitNotInt32.into()),
                BitNotInt64(()) => Ok(impls::BitNotInt64.into()),
                BitNotUint16(()) => Ok(impls::BitNotUint16.into()),
                BitNotUint32(()) => Ok(impls::BitNotUint32.into()),
                BitNotUint64(()) => Ok(impls::BitNotUint64.into()),
                NegInt16(()) => Ok(impls::NegInt16.into()),
                NegInt32(()) => Ok(impls::NegInt32.into()),
                NegInt64(()) => Ok(impls::NegInt64.into()),
                NegFloat32(()) => Ok(impls::NegFloat32.into()),
                NegFloat64(()) => Ok(impls::NegFloat64.into()),
                NegNumeric(()) => Ok(impls::NegNumeric.into()),
                NegInterval(()) => Ok(impls::NegInterval.into()),
                SqrtFloat64(()) => Ok(impls::SqrtFloat64.into()),
                SqrtNumeric(()) => Ok(impls::SqrtNumeric.into()),
                CbrtFloat64(()) => Ok(impls::CbrtFloat64.into()),
                AbsInt16(()) => Ok(impls::AbsInt16.into()),
                AbsInt32(()) => Ok(impls::AbsInt32.into()),
                AbsInt64(()) => Ok(impls::AbsInt64.into()),
                AbsFloat32(()) => Ok(impls::AbsFloat32.into()),
                AbsFloat64(()) => Ok(impls::AbsFloat64.into()),
                AbsNumeric(()) => Ok(impls::AbsNumeric.into()),
                CastBoolToString(()) => Ok(impls::CastBoolToString.into()),
                CastBoolToStringNonstandard(()) => Ok(impls::CastBoolToStringNonstandard.into()),
                CastBoolToInt32(()) => Ok(impls::CastBoolToInt32.into()),
                CastBoolToInt64(()) => Ok(impls::CastBoolToInt64.into()),
                CastInt16ToFloat32(()) => Ok(impls::CastInt16ToFloat32.into()),
                CastInt16ToFloat64(()) => Ok(impls::CastInt16ToFloat64.into()),
                CastInt16ToInt32(()) => Ok(impls::CastInt16ToInt32.into()),
                CastInt16ToInt64(()) => Ok(impls::CastInt16ToInt64.into()),
                CastInt16ToUint16(()) => Ok(impls::CastInt16ToUint16.into()),
                CastInt16ToUint32(()) => Ok(impls::CastInt16ToUint32.into()),
                CastInt16ToUint64(()) => Ok(impls::CastInt16ToUint64.into()),
                CastInt16ToString(()) => Ok(impls::CastInt16ToString.into()),
                CastInt2VectorToArray(()) => Ok(impls::CastInt2VectorToArray.into()),
                CastInt32ToBool(()) => Ok(impls::CastInt32ToBool.into()),
                CastInt32ToFloat32(()) => Ok(impls::CastInt32ToFloat32.into()),
                CastInt32ToFloat64(()) => Ok(impls::CastInt32ToFloat64.into()),
                CastInt32ToOid(()) => Ok(impls::CastInt32ToOid.into()),
                CastInt32ToPgLegacyChar(()) => Ok(impls::CastInt32ToPgLegacyChar.into()),
                CastInt32ToInt16(()) => Ok(impls::CastInt32ToInt16.into()),
                CastInt32ToInt64(()) => Ok(impls::CastInt32ToInt64.into()),
                CastInt32ToUint16(()) => Ok(impls::CastInt32ToUint16.into()),
                CastInt32ToUint32(()) => Ok(impls::CastInt32ToUint32.into()),
                CastInt32ToUint64(()) => Ok(impls::CastInt32ToUint64.into()),
                CastInt32ToString(()) => Ok(impls::CastInt32ToString.into()),
                CastOidToInt32(()) => Ok(impls::CastOidToInt32.into()),
                CastOidToInt64(()) => Ok(impls::CastOidToInt64.into()),
                CastOidToString(()) => Ok(impls::CastOidToString.into()),
                CastOidToRegClass(()) => Ok(impls::CastOidToRegClass.into()),
                CastRegClassToOid(()) => Ok(impls::CastRegClassToOid.into()),
                CastOidToRegProc(()) => Ok(impls::CastOidToRegProc.into()),
                CastRegProcToOid(()) => Ok(impls::CastRegProcToOid.into()),
                CastOidToRegType(()) => Ok(impls::CastOidToRegType.into()),
                CastRegTypeToOid(()) => Ok(impls::CastRegTypeToOid.into()),
                CastInt64ToInt16(()) => Ok(impls::CastInt64ToInt16.into()),
                CastInt64ToInt32(()) => Ok(impls::CastInt64ToInt32.into()),
                CastInt64ToUint16(()) => Ok(impls::CastInt64ToUint16.into()),
                CastInt64ToUint32(()) => Ok(impls::CastInt64ToUint32.into()),
                CastInt64ToUint64(()) => Ok(impls::CastInt64ToUint64.into()),
                CastInt16ToNumeric(max_scale) => {
                    Ok(impls::CastInt16ToNumeric(max_scale.into_rust()?).into())
                }
                CastInt32ToNumeric(max_scale) => {
                    Ok(impls::CastInt32ToNumeric(max_scale.into_rust()?).into())
                }
                CastInt64ToBool(()) => Ok(impls::CastInt64ToBool.into()),
                CastInt64ToNumeric(max_scale) => {
                    Ok(impls::CastInt64ToNumeric(max_scale.into_rust()?).into())
                }
                CastInt64ToFloat32(()) => Ok(impls::CastInt64ToFloat32.into()),
                CastInt64ToFloat64(()) => Ok(impls::CastInt64ToFloat64.into()),
                CastInt64ToOid(()) => Ok(impls::CastInt64ToOid.into()),
                CastInt64ToString(()) => Ok(impls::CastInt64ToString.into()),
                CastUint16ToUint32(()) => Ok(impls::CastUint16ToUint32.into()),
                CastUint16ToUint64(()) => Ok(impls::CastUint16ToUint64.into()),
                CastUint16ToInt16(()) => Ok(impls::CastUint16ToInt16.into()),
                CastUint16ToInt32(()) => Ok(impls::CastUint16ToInt32.into()),
                CastUint16ToInt64(()) => Ok(impls::CastUint16ToInt64.into()),
                CastUint16ToNumeric(max_scale) => {
                    Ok(impls::CastUint16ToNumeric(max_scale.into_rust()?).into())
                }
                CastUint16ToFloat32(()) => Ok(impls::CastUint16ToFloat32.into()),
                CastUint16ToFloat64(()) => Ok(impls::CastUint16ToFloat64.into()),
                CastUint16ToString(()) => Ok(impls::CastUint16ToString.into()),
                CastUint32ToUint16(()) => Ok(impls::CastUint32ToUint16.into()),
                CastUint32ToUint64(()) => Ok(impls::CastUint32ToUint64.into()),
                CastUint32ToInt16(()) => Ok(impls::CastUint32ToInt16.into()),
                CastUint32ToInt32(()) => Ok(impls::CastUint32ToInt32.into()),
                CastUint32ToInt64(()) => Ok(impls::CastUint32ToInt64.into()),
                CastUint32ToNumeric(max_scale) => {
                    Ok(impls::CastUint32ToNumeric(max_scale.into_rust()?).into())
                }
                CastUint32ToFloat32(()) => Ok(impls::CastUint32ToFloat32.into()),
                CastUint32ToFloat64(()) => Ok(impls::CastUint32ToFloat64.into()),
                CastUint32ToString(()) => Ok(impls::CastUint32ToString.into()),
                CastUint64ToUint16(()) => Ok(impls::CastUint64ToUint16.into()),
                CastUint64ToUint32(()) => Ok(impls::CastUint64ToUint32.into()),
                CastUint64ToInt16(()) => Ok(impls::CastUint64ToInt16.into()),
                CastUint64ToInt32(()) => Ok(impls::CastUint64ToInt32.into()),
                CastUint64ToInt64(()) => Ok(impls::CastUint64ToInt64.into()),
                CastUint64ToNumeric(max_scale) => {
                    Ok(impls::CastUint64ToNumeric(max_scale.into_rust()?).into())
                }
                CastUint64ToFloat32(()) => Ok(impls::CastUint64ToFloat32.into()),
                CastUint64ToFloat64(()) => Ok(impls::CastUint64ToFloat64.into()),
                CastUint64ToString(()) => Ok(impls::CastUint64ToString.into()),
                CastFloat32ToInt16(()) => Ok(impls::CastFloat32ToInt16.into()),
                CastFloat32ToInt32(()) => Ok(impls::CastFloat32ToInt32.into()),
                CastFloat32ToInt64(()) => Ok(impls::CastFloat32ToInt64.into()),
                CastFloat32ToUint16(()) => Ok(impls::CastFloat32ToUint16.into()),
                CastFloat32ToUint32(()) => Ok(impls::CastFloat32ToUint32.into()),
                CastFloat32ToUint64(()) => Ok(impls::CastFloat32ToUint64.into()),
                CastFloat32ToFloat64(()) => Ok(impls::CastFloat32ToFloat64.into()),
                CastFloat32ToString(()) => Ok(impls::CastFloat32ToString.into()),
                CastFloat32ToNumeric(max_scale) => {
                    Ok(impls::CastFloat32ToNumeric(max_scale.into_rust()?).into())
                }
                CastFloat64ToNumeric(max_scale) => {
                    Ok(impls::CastFloat64ToNumeric(max_scale.into_rust()?).into())
                }
                CastFloat64ToInt16(()) => Ok(impls::CastFloat64ToInt16.into()),
                CastFloat64ToInt32(()) => Ok(impls::CastFloat64ToInt32.into()),
                CastFloat64ToInt64(()) => Ok(impls::CastFloat64ToInt64.into()),
                CastFloat64ToUint16(()) => Ok(impls::CastFloat64ToUint16.into()),
                CastFloat64ToUint32(()) => Ok(impls::CastFloat64ToUint32.into()),
                CastFloat64ToUint64(()) => Ok(impls::CastFloat64ToUint64.into()),
                CastFloat64ToFloat32(()) => Ok(impls::CastFloat64ToFloat32.into()),
                CastFloat64ToString(()) => Ok(impls::CastFloat64ToString.into()),
                CastNumericToFloat32(()) => Ok(impls::CastNumericToFloat32.into()),
                CastNumericToFloat64(()) => Ok(impls::CastNumericToFloat64.into()),
                CastNumericToInt16(()) => Ok(impls::CastNumericToInt16.into()),
                CastNumericToInt32(()) => Ok(impls::CastNumericToInt32.into()),
                CastNumericToInt64(()) => Ok(impls::CastNumericToInt64.into()),
                CastNumericToUint16(()) => Ok(impls::CastNumericToUint16.into()),
                CastNumericToUint32(()) => Ok(impls::CastNumericToUint32.into()),
                CastNumericToUint64(()) => Ok(impls::CastNumericToUint64.into()),
                CastNumericToString(()) => Ok(impls::CastNumericToString.into()),
                CastStringToBool(()) => Ok(impls::CastStringToBool.into()),
                CastStringToPgLegacyChar(()) => Ok(impls::CastStringToPgLegacyChar.into()),
                CastStringToPgLegacyName(()) => Ok(impls::CastStringToPgLegacyName.into()),
                CastStringToBytes(()) => Ok(impls::CastStringToBytes.into()),
                CastStringToInt16(()) => Ok(impls::CastStringToInt16.into()),
                CastStringToInt32(()) => Ok(impls::CastStringToInt32.into()),
                CastStringToInt64(()) => Ok(impls::CastStringToInt64.into()),
                CastStringToUint16(()) => Ok(impls::CastStringToUint16.into()),
                CastStringToUint32(()) => Ok(impls::CastStringToUint32.into()),
                CastStringToUint64(()) => Ok(impls::CastStringToUint64.into()),
                CastStringToInt2Vector(()) => Ok(impls::CastStringToInt2Vector.into()),
                CastStringToOid(()) => Ok(impls::CastStringToOid.into()),
                CastStringToFloat32(()) => Ok(impls::CastStringToFloat32.into()),
                CastStringToFloat64(()) => Ok(impls::CastStringToFloat64.into()),
                CastStringToDate(()) => Ok(impls::CastStringToDate.into()),
                CastStringToArray(inner) => Ok(impls::CastStringToArray {
                    return_ty: inner
                        .return_ty
                        .into_rust_if_some("ProtoCastStringToArray::return_ty")?,
                    cast_expr: inner
                        .cast_expr
                        .into_rust_if_some("ProtoCastStringToArray::cast_expr")?,
                }
                .into()),
                CastStringToList(inner) => Ok(impls::CastStringToList {
                    return_ty: inner
                        .return_ty
                        .into_rust_if_some("ProtoCastStringToList::return_ty")?,
                    cast_expr: inner
                        .cast_expr
                        .into_rust_if_some("ProtoCastStringToList::cast_expr")?,
                }
                .into()),
                CastStringToRange(inner) => Ok(impls::CastStringToRange {
                    return_ty: inner
                        .return_ty
                        .into_rust_if_some("ProtoCastStringToRange::return_ty")?,
                    cast_expr: inner
                        .cast_expr
                        .into_rust_if_some("ProtoCastStringToRange::cast_expr")?,
                }
                .into()),
                CastStringToMap(inner) => Ok(impls::CastStringToMap {
                    return_ty: inner
                        .return_ty
                        .into_rust_if_some("ProtoCastStringToMap::return_ty")?,
                    cast_expr: inner
                        .cast_expr
                        .into_rust_if_some("ProtoCastStringToMap::cast_expr")?,
                }
                .into()),
                CastStringToTime(()) => Ok(impls::CastStringToTime.into()),
                CastStringToTimestamp(precision) => {
                    Ok(impls::CastStringToTimestamp(precision.into_rust()?).into())
                }
                CastStringToTimestampTz(precision) => {
                    Ok(impls::CastStringToTimestampTz(precision.into_rust()?).into())
                }
                CastStringToInterval(()) => Ok(impls::CastStringToInterval.into()),
                CastStringToNumeric(max_scale) => {
                    Ok(impls::CastStringToNumeric(max_scale.into_rust()?).into())
                }
                CastStringToUuid(()) => Ok(impls::CastStringToUuid.into()),
                CastStringToChar(func) => Ok(impls::CastStringToChar {
                    length: func.length.into_rust()?,
                    fail_on_len: func.fail_on_len,
                }
                .into()),
                PadChar(func) => Ok(impls::PadChar {
                    length: func.length.into_rust()?,
                }
                .into()),
                CastStringToVarChar(func) => Ok(impls::CastStringToVarChar {
                    length: func.length.into_rust()?,
                    fail_on_len: func.fail_on_len,
                }
                .into()),
                CastCharToString(()) => Ok(impls::CastCharToString.into()),
                CastVarCharToString(()) => Ok(impls::CastVarCharToString.into()),
                CastDateToTimestamp(precision) => {
                    Ok(impls::CastDateToTimestamp(precision.into_rust()?).into())
                }
                CastDateToTimestampTz(precision) => {
                    Ok(impls::CastDateToTimestampTz(precision.into_rust()?).into())
                }
                CastDateToString(()) => Ok(impls::CastDateToString.into()),
                CastTimeToInterval(()) => Ok(impls::CastTimeToInterval.into()),
                CastTimeToString(()) => Ok(impls::CastTimeToString.into()),
                CastIntervalToString(()) => Ok(impls::CastIntervalToString.into()),
                CastIntervalToTime(()) => Ok(impls::CastIntervalToTime.into()),
                CastTimestampToDate(()) => Ok(impls::CastTimestampToDate.into()),
                AdjustTimestampPrecision(precisions) => Ok(impls::AdjustTimestampPrecision {
                    from: precisions.from.into_rust()?,
                    to: precisions.to.into_rust()?,
                }
                .into()),
                CastTimestampToTimestampTz(precisions) => Ok(impls::CastTimestampToTimestampTz {
                    from: precisions.from.into_rust()?,
                    to: precisions.to.into_rust()?,
                }
                .into()),
                CastTimestampToString(()) => Ok(impls::CastTimestampToString.into()),
                CastTimestampToTime(()) => Ok(impls::CastTimestampToTime.into()),
                CastTimestampTzToDate(()) => Ok(impls::CastTimestampTzToDate.into()),
                CastTimestampTzToTimestamp(precisions) => Ok(impls::CastTimestampTzToTimestamp {
                    from: precisions.from.into_rust()?,
                    to: precisions.to.into_rust()?,
                }
                .into()),
                AdjustTimestampTzPrecision(precisions) => Ok(impls::AdjustTimestampTzPrecision {
                    from: precisions.from.into_rust()?,
                    to: precisions.to.into_rust()?,
                }
                .into()),
                CastTimestampTzToString(()) => Ok(impls::CastTimestampTzToString.into()),
                CastTimestampTzToTime(()) => Ok(impls::CastTimestampTzToTime.into()),
                CastPgLegacyCharToString(()) => Ok(impls::CastPgLegacyCharToString.into()),
                CastPgLegacyCharToChar(()) => Ok(impls::CastPgLegacyCharToChar.into()),
                CastPgLegacyCharToVarChar(()) => Ok(impls::CastPgLegacyCharToVarChar.into()),
                CastPgLegacyCharToInt32(()) => Ok(impls::CastPgLegacyCharToInt32.into()),
                CastBytesToString(()) => Ok(impls::CastBytesToString.into()),
                CastStringToJsonb(()) => Ok(impls::CastStringToJsonb.into()),
                CastJsonbToString(()) => Ok(impls::CastJsonbToString.into()),
                CastJsonbableToJsonb(()) => Ok(impls::CastJsonbableToJsonb.into()),
                CastJsonbToInt16(()) => Ok(impls::CastJsonbToInt16.into()),
                CastJsonbToInt32(()) => Ok(impls::CastJsonbToInt32.into()),
                CastJsonbToInt64(()) => Ok(impls::CastJsonbToInt64.into()),
                CastJsonbToFloat32(()) => Ok(impls::CastJsonbToFloat32.into()),
                CastJsonbToFloat64(()) => Ok(impls::CastJsonbToFloat64.into()),
                CastJsonbToNumeric(max_scale) => {
                    Ok(impls::CastJsonbToNumeric(max_scale.into_rust()?).into())
                }
                CastJsonbToBool(()) => Ok(impls::CastJsonbToBool.into()),
                CastUuidToString(()) => Ok(impls::CastUuidToString.into()),
                CastRecordToString(ty) => Ok(impls::CastRecordToString {
                    ty: ty.into_rust()?,
                }
                .into()),
                CastRecord1ToRecord2(inner) => Ok(impls::CastRecord1ToRecord2 {
                    return_ty: inner
                        .return_ty
                        .into_rust_if_some("ProtoCastRecord1ToRecord2::return_ty")?,
                    cast_exprs: inner.cast_exprs.into_rust()?,
                }
                .into()),
                CastArrayToArray(inner) => Ok(impls::CastArrayToArray {
                    return_ty: inner
                        .return_ty
                        .into_rust_if_some("ProtoCastArrayToArray::return_ty")?,
                    cast_expr: inner
                        .cast_expr
                        .into_rust_if_some("ProtoCastArrayToArray::cast_expr")?,
                }
                .into()),
                CastArrayToJsonb(cast_element) => Ok(impls::CastArrayToJsonb {
                    cast_element: cast_element.into_rust()?,
                }
                .into()),
                CastArrayToString(ty) => Ok(impls::CastArrayToString {
                    ty: ty.into_rust()?,
                }
                .into()),
                CastListToJsonb(cast_element) => Ok(impls::CastListToJsonb {
                    cast_element: cast_element.into_rust()?,
                }
                .into()),
                CastListToString(ty) => Ok(impls::CastListToString {
                    ty: ty.into_rust()?,
                }
                .into()),
                CastList1ToList2(inner) => Ok(impls::CastList1ToList2 {
                    return_ty: inner
                        .return_ty
                        .into_rust_if_some("ProtoCastList1ToList2::return_ty")?,
                    cast_expr: inner
                        .cast_expr
                        .into_rust_if_some("ProtoCastList1ToList2::cast_expr")?,
                }
                .into()),
                CastArrayToListOneDim(()) => Ok(impls::CastArrayToListOneDim.into()),
                CastMapToString(ty) => Ok(impls::CastMapToString {
                    ty: ty.into_rust()?,
                }
                .into()),
                CastInt2VectorToString(_) => Ok(impls::CastInt2VectorToString.into()),
                CastRangeToString(ty) => Ok(impls::CastRangeToString {
                    ty: ty.into_rust()?,
                }
                .into()),
                CeilFloat32(_) => Ok(impls::CeilFloat32.into()),
                CeilFloat64(_) => Ok(impls::CeilFloat64.into()),
                CeilNumeric(_) => Ok(impls::CeilNumeric.into()),
                FloorFloat32(_) => Ok(impls::FloorFloat32.into()),
                FloorFloat64(_) => Ok(impls::FloorFloat64.into()),
                FloorNumeric(_) => Ok(impls::FloorNumeric.into()),
                Ascii(_) => Ok(impls::Ascii.into()),
                BitCountBytes(_) => Ok(impls::BitCountBytes.into()),
                BitLengthBytes(_) => Ok(impls::BitLengthBytes.into()),
                BitLengthString(_) => Ok(impls::BitLengthString.into()),
                ByteLengthBytes(_) => Ok(impls::ByteLengthBytes.into()),
                ByteLengthString(_) => Ok(impls::ByteLengthString.into()),
                CharLength(_) => Ok(impls::CharLength.into()),
                Chr(_) => Ok(impls::Chr.into()),
                IsLikeMatch(pattern) => Ok(impls::IsLikeMatch(pattern.into_rust()?).into()),
                IsRegexpMatch(regex) => Ok(impls::IsRegexpMatch(regex.into_rust()?).into()),
                RegexpMatch(regex) => Ok(impls::RegexpMatch(regex.into_rust()?).into()),
                RegexpSplitToArray(regex) => {
                    Ok(impls::RegexpSplitToArray(regex.into_rust()?).into())
                }
                ExtractInterval(units) => Ok(impls::ExtractInterval(units.into_rust()?).into()),
                ExtractTime(units) => Ok(impls::ExtractTime(units.into_rust()?).into()),
                ExtractTimestamp(units) => Ok(impls::ExtractTimestamp(units.into_rust()?).into()),
                ExtractTimestampTz(units) => {
                    Ok(impls::ExtractTimestampTz(units.into_rust()?).into())
                }
                ExtractDate(units) => Ok(impls::ExtractDate(units.into_rust()?).into()),
                DatePartInterval(units) => Ok(impls::DatePartInterval(units.into_rust()?).into()),
                DatePartTime(units) => Ok(impls::DatePartTime(units.into_rust()?).into()),
                DatePartTimestamp(units) => Ok(impls::DatePartTimestamp(units.into_rust()?).into()),
                DatePartTimestampTz(units) => {
                    Ok(impls::DatePartTimestampTz(units.into_rust()?).into())
                }
                DateTruncTimestamp(units) => {
                    Ok(impls::DateTruncTimestamp(units.into_rust()?).into())
                }
                DateTruncTimestampTz(units) => {
                    Ok(impls::DateTruncTimestampTz(units.into_rust()?).into())
                }
                TimezoneTimestamp(tz) => Ok(impls::TimezoneTimestamp(tz.into_rust()?).into()),
                TimezoneTimestampTz(tz) => Ok(impls::TimezoneTimestampTz(tz.into_rust()?).into()),
                TimezoneTime(func) => Ok(impls::TimezoneTime {
                    tz: func.tz.into_rust_if_some("ProtoTimezoneTime::tz")?,
                    wall_time: func
                        .wall_time
                        .into_rust_if_some("ProtoTimezoneTime::wall_time")?,
                }
                .into()),
                ToTimestamp(()) => Ok(impls::ToTimestamp.into()),
                ToCharTimestamp(func) => Ok(impls::ToCharTimestamp {
                    format_string: func.format_string,
                    format: func
                        .format
                        .into_rust_if_some("ProtoToCharTimestamp::format")?,
                }
                .into()),
                ToCharTimestampTz(func) => Ok(impls::ToCharTimestampTz {
                    format_string: func.format_string,
                    format: func
                        .format
                        .into_rust_if_some("ProtoToCharTimestamp::format")?,
                }
                .into()),
                JustifyDays(()) => Ok(impls::JustifyDays.into()),
                JustifyHours(()) => Ok(impls::JustifyHours.into()),
                JustifyInterval(()) => Ok(impls::JustifyInterval.into()),
                JsonbArrayLength(()) => Ok(impls::JsonbArrayLength.into()),
                JsonbTypeof(()) => Ok(impls::JsonbTypeof.into()),
                JsonbStripNulls(()) => Ok(impls::JsonbStripNulls.into()),
                JsonbPretty(()) => Ok(impls::JsonbPretty.into()),
                RoundFloat32(()) => Ok(impls::RoundFloat32.into()),
                RoundFloat64(()) => Ok(impls::RoundFloat64.into()),
                RoundNumeric(()) => Ok(impls::RoundNumeric.into()),
                TruncFloat32(()) => Ok(impls::TruncFloat32.into()),
                TruncFloat64(()) => Ok(impls::TruncFloat64.into()),
                TruncNumeric(()) => Ok(impls::TruncNumeric.into()),
                TrimWhitespace(()) => Ok(impls::TrimWhitespace.into()),
                TrimLeadingWhitespace(()) => Ok(impls::TrimLeadingWhitespace.into()),
                TrimTrailingWhitespace(()) => Ok(impls::TrimTrailingWhitespace.into()),
                Initcap(()) => Ok(impls::Initcap.into()),
                RecordGet(field) => Ok(impls::RecordGet(field.into_rust()?).into()),
                ListLength(()) => Ok(impls::ListLength.into()),
                MapBuildFromRecordList(value_type) => Ok(impls::MapBuildFromRecordList {
                    value_type: value_type.into_rust()?,
                }
                .into()),
                MapLength(()) => Ok(impls::MapLength.into()),
                Upper(()) => Ok(impls::Upper.into()),
                Lower(()) => Ok(impls::Lower.into()),
                Cos(()) => Ok(impls::Cos.into()),
                Acos(()) => Ok(impls::Acos.into()),
                Cosh(()) => Ok(impls::Cosh.into()),
                Acosh(()) => Ok(impls::Acosh.into()),
                Sin(()) => Ok(impls::Sin.into()),
                Asin(()) => Ok(impls::Asin.into()),
                Sinh(()) => Ok(impls::Sinh.into()),
                Asinh(()) => Ok(impls::Asinh.into()),
                Tan(()) => Ok(impls::Tan.into()),
                Atan(()) => Ok(impls::Atan.into()),
                Tanh(()) => Ok(impls::Tanh.into()),
                Atanh(()) => Ok(impls::Atanh.into()),
                Cot(()) => Ok(impls::Cot.into()),
                Degrees(()) => Ok(impls::Degrees.into()),
                Radians(()) => Ok(impls::Radians.into()),
                Log10(()) => Ok(impls::Log10.into()),
                Log10Numeric(()) => Ok(impls::Log10Numeric.into()),
                Ln(()) => Ok(impls::Ln.into()),
                LnNumeric(()) => Ok(impls::LnNumeric.into()),
                Exp(()) => Ok(impls::Exp.into()),
                ExpNumeric(()) => Ok(impls::ExpNumeric.into()),
                Sleep(()) => Ok(impls::Sleep.into()),
                Panic(()) => Ok(impls::Panic.into()),
                AdjustNumericScale(max_scale) => {
                    Ok(impls::AdjustNumericScale(max_scale.into_rust()?).into())
                }
                PgColumnSize(()) => Ok(impls::PgColumnSize.into()),
                PgSizePretty(()) => Ok(impls::PgSizePretty.into()),
                MzRowSize(()) => Ok(impls::MzRowSize.into()),
                MzTypeName(()) => Ok(impls::MzTypeName.into()),

                CastMzTimestampToString(()) => Ok(impls::CastMzTimestampToString.into()),
                CastMzTimestampToTimestamp(()) => Ok(impls::CastMzTimestampToTimestamp.into()),
                CastMzTimestampToTimestampTz(()) => Ok(impls::CastMzTimestampToTimestampTz.into()),
                CastStringToMzTimestamp(()) => Ok(impls::CastStringToMzTimestamp.into()),
                CastUint64ToMzTimestamp(()) => Ok(impls::CastUint64ToMzTimestamp.into()),
                CastUint32ToMzTimestamp(()) => Ok(impls::CastUint32ToMzTimestamp.into()),
                CastInt64ToMzTimestamp(()) => Ok(impls::CastInt64ToMzTimestamp.into()),
                CastInt32ToMzTimestamp(()) => Ok(impls::CastInt32ToMzTimestamp.into()),
                CastNumericToMzTimestamp(()) => Ok(impls::CastNumericToMzTimestamp.into()),
                CastTimestampToMzTimestamp(()) => Ok(impls::CastTimestampToMzTimestamp.into()),
                CastTimestampTzToMzTimestamp(()) => Ok(impls::CastTimestampTzToMzTimestamp.into()),
                CastDateToMzTimestamp(()) => Ok(impls::CastDateToMzTimestamp.into()),
                StepMzTimestamp(()) => Ok(impls::StepMzTimestamp.into()),
                RangeLower(()) => Ok(impls::RangeLower.into()),
                RangeUpper(()) => Ok(impls::RangeUpper.into()),
                RangeEmpty(()) => Ok(impls::RangeEmpty.into()),
                RangeLowerInc(_) => Ok(impls::RangeLowerInc.into()),
                RangeUpperInc(_) => Ok(impls::RangeUpperInc.into()),
                RangeLowerInf(_) => Ok(impls::RangeLowerInf.into()),
                RangeUpperInf(_) => Ok(impls::RangeUpperInf.into()),
                MzAclItemGrantor(_) => Ok(impls::MzAclItemGrantor.into()),
                MzAclItemGrantee(_) => Ok(impls::MzAclItemGrantee.into()),
                MzAclItemPrivileges(_) => Ok(impls::MzAclItemPrivileges.into()),
                MzFormatPrivileges(_) => Ok(impls::MzFormatPrivileges.into()),
                MzValidatePrivileges(_) => Ok(impls::MzValidatePrivileges.into()),
                MzValidateRolePrivilege(_) => Ok(impls::MzValidateRolePrivilege.into()),
                AclItemGrantor(_) => Ok(impls::AclItemGrantor.into()),
                AclItemGrantee(_) => Ok(impls::AclItemGrantee.into()),
                AclItemPrivileges(_) => Ok(impls::AclItemPrivileges.into()),
                QuoteIdent(_) => Ok(impls::QuoteIdent.into()),
                TryParseMonotonicIso8601Timestamp(_) => {
                    Ok(impls::TryParseMonotonicIso8601Timestamp.into())
                }
                Crc32Bytes(()) => Ok(impls::Crc32Bytes.into()),
                Crc32String(()) => Ok(impls::Crc32String.into()),
                KafkaMurmur2Bytes(()) => Ok(impls::KafkaMurmur2Bytes.into()),
                KafkaMurmur2String(()) => Ok(impls::KafkaMurmur2String.into()),
                SeahashBytes(()) => Ok(impls::SeahashBytes.into()),
                SeahashString(()) => Ok(impls::SeahashString.into()),
                Reverse(()) => Ok(impls::Reverse.into()),
            }
        } else {
            Err(TryFromProtoError::missing_field("ProtoUnaryFunc::kind"))
        }
    }
}

impl IntoRustIfSome<UnaryFunc> for Option<Box<ProtoUnaryFunc>> {
    fn into_rust_if_some<S: ToString>(self, field: S) -> Result<UnaryFunc, TryFromProtoError> {
        let value = self.ok_or_else(|| TryFromProtoError::missing_field(field))?;
        (*value).into_rust()
    }
}

fn coalesce<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
    exprs: &'a [MirScalarExpr],
) -> Result<Datum<'a>, EvalError> {
    for e in exprs {
        let d = e.eval(datums, temp_storage)?;
        if !d.is_null() {
            return Ok(d);
        }
    }
    Ok(Datum::Null)
}

fn greatest<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
    exprs: &'a [MirScalarExpr],
) -> Result<Datum<'a>, EvalError> {
    let datums = fallible_iterator::convert(exprs.iter().map(|e| e.eval(datums, temp_storage)));
    Ok(datums
        .filter(|d| Ok(!d.is_null()))
        .max()?
        .unwrap_or(Datum::Null))
}

fn least<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
    exprs: &'a [MirScalarExpr],
) -> Result<Datum<'a>, EvalError> {
    let datums = fallible_iterator::convert(exprs.iter().map(|e| e.eval(datums, temp_storage)));
    Ok(datums
        .filter(|d| Ok(!d.is_null()))
        .min()?
        .unwrap_or(Datum::Null))
}

fn error_if_null<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
    exprs: &'a [MirScalarExpr],
) -> Result<Datum<'a>, EvalError> {
    let first = exprs[0].eval(datums, temp_storage)?;
    match first {
        Datum::Null => {
            let err_msg = match exprs[1].eval(datums, temp_storage)? {
                Datum::Null => {
                    return Err(EvalError::Internal(
                        "unexpected NULL in error side of error_if_null".into(),
                    ));
                }
                o => o.unwrap_str(),
            };
            Err(EvalError::IfNullError(err_msg.into()))
        }
        _ => Ok(first),
    }
}

#[sqlfunc(
    sqlname = "||",
    is_infix_op = true,
    output_type = "String",
    propagates_nulls = true,
    is_monotone = (false, true),
)]
fn text_concat_binary<'a>(a: Datum<'a>, b: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    let mut buf = String::new();
    buf.push_str(a.unwrap_str());
    buf.push_str(b.unwrap_str());
    Datum::String(temp_storage.push_string(buf))
}

fn text_concat_variadic<'a>(datums: &[Datum<'a>], temp_storage: &'a RowArena) -> Datum<'a> {
    let mut buf = String::new();
    for d in datums {
        if !d.is_null() {
            buf.push_str(d.unwrap_str());
        }
    }
    Datum::String(temp_storage.push_string(buf))
}

fn text_concat_ws<'a>(datums: &[Datum<'a>], temp_storage: &'a RowArena) -> Datum<'a> {
    let ws = match datums[0] {
        Datum::Null => return Datum::Null,
        d => d.unwrap_str(),
    };

    let buf = Itertools::join(
        &mut datums[1..].iter().filter_map(|d| match d {
            Datum::Null => None,
            d => Some(d.unwrap_str()),
        }),
        ws,
    );

    Datum::String(temp_storage.push_string(buf))
}

fn pad_leading<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let string = datums[0].unwrap_str();

    let len = match usize::try_from(datums[1].unwrap_int32()) {
        Ok(len) => len,
        Err(_) => {
            return Err(EvalError::InvalidParameterValue(
                "length must be nonnegative".into(),
            ));
        }
    };
    if len > MAX_STRING_BYTES {
        return Err(EvalError::LengthTooLarge);
    }

    let pad_string = if datums.len() == 3 {
        datums[2].unwrap_str()
    } else {
        " "
    };

    let (end_char, end_char_byte_offset) = string
        .chars()
        .take(len)
        .fold((0, 0), |acc, char| (acc.0 + 1, acc.1 + char.len_utf8()));

    let mut buf = String::with_capacity(len);
    if len == end_char {
        buf.push_str(&string[0..end_char_byte_offset]);
    } else {
        buf.extend(pad_string.chars().cycle().take(len - end_char));
        buf.push_str(string);
    }

    Ok(Datum::String(temp_storage.push_string(buf)))
}

fn substr<'a>(datums: &[Datum<'a>]) -> Result<Datum<'a>, EvalError> {
    let s: &'a str = datums[0].unwrap_str();

    let raw_start_idx = i64::from(datums[1].unwrap_int32()) - 1;
    let start_idx = match usize::try_from(cmp::max(raw_start_idx, 0)) {
        Ok(i) => i,
        Err(_) => {
            return Err(EvalError::InvalidParameterValue(
                format!(
                    "substring starting index ({}) exceeds min/max position",
                    raw_start_idx
                )
                .into(),
            ));
        }
    };

    let mut char_indices = s.char_indices();
    let get_str_index = |(index, _char)| index;

    let str_len = s.len();
    let start_char_idx = char_indices.nth(start_idx).map_or(str_len, get_str_index);

    if datums.len() == 3 {
        let end_idx = match i64::from(datums[2].unwrap_int32()) {
            e if e < 0 => {
                return Err(EvalError::InvalidParameterValue(
                    "negative substring length not allowed".into(),
                ));
            }
            e if e == 0 || e + raw_start_idx < 1 => return Ok(Datum::String("")),
            e => {
                let e = cmp::min(raw_start_idx + e - 1, e - 1);
                match usize::try_from(e) {
                    Ok(i) => i,
                    Err(_) => {
                        return Err(EvalError::InvalidParameterValue(
                            format!("substring length ({}) exceeds max position", e).into(),
                        ));
                    }
                }
            }
        };

        let end_char_idx = char_indices.nth(end_idx).map_or(str_len, get_str_index);

        Ok(Datum::String(&s[start_char_idx..end_char_idx]))
    } else {
        Ok(Datum::String(&s[start_char_idx..]))
    }
}

fn split_part<'a>(datums: &[Datum<'a>]) -> Result<Datum<'a>, EvalError> {
    let string = datums[0].unwrap_str();
    let delimiter = datums[1].unwrap_str();

    // Provided index value begins at 1, not 0.
    let index = match usize::try_from(i64::from(datums[2].unwrap_int32()) - 1) {
        Ok(index) => index,
        Err(_) => {
            return Err(EvalError::InvalidParameterValue(
                "field position must be greater than zero".into(),
            ));
        }
    };

    // If the provided delimiter is the empty string,
    // PostgreSQL does not break the string into individual
    // characters. Instead, it generates the following parts: [string].
    if delimiter.is_empty() {
        if index == 0 {
            return Ok(datums[0]);
        } else {
            return Ok(Datum::String(""));
        }
    }

    // If provided index is greater than the number of split parts,
    // return an empty string.
    Ok(Datum::String(
        string.split(delimiter).nth(index).unwrap_or(""),
    ))
}

#[sqlfunc(
    output_type = "String",
    propagates_nulls = true,
    introduces_nulls = false
)]
fn like_escape<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let pattern = a.unwrap_str();
    let escape = like_pattern::EscapeBehavior::from_str(b.unwrap_str())?;
    let normalized = like_pattern::normalize_pattern(pattern, escape)?;
    Ok(Datum::String(temp_storage.push_string(normalized)))
}

fn is_like_match_dynamic<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    case_insensitive: bool,
) -> Result<Datum<'a>, EvalError> {
    let haystack = a.unwrap_str();
    let needle = like_pattern::compile(b.unwrap_str(), case_insensitive)?;
    Ok(Datum::from(needle.is_match(haystack.as_ref())))
}

fn is_regexp_match_dynamic<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    case_insensitive: bool,
) -> Result<Datum<'a>, EvalError> {
    let haystack = a.unwrap_str();
    let needle = build_regex(b.unwrap_str(), if case_insensitive { "i" } else { "" })?;
    Ok(Datum::from(needle.is_match(haystack)))
}

fn regexp_match_dynamic<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let haystack = datums[0];
    let needle = datums[1].unwrap_str();
    let flags = match datums.get(2) {
        Some(d) => d.unwrap_str(),
        None => "",
    };
    let needle = build_regex(needle, flags)?;
    regexp_match_static(haystack, temp_storage, &needle)
}

fn regexp_match_static<'a>(
    haystack: Datum<'a>,
    temp_storage: &'a RowArena,
    needle: &regex::Regex,
) -> Result<Datum<'a>, EvalError> {
    let mut row = Row::default();
    let mut packer = row.packer();
    if needle.captures_len() > 1 {
        // The regex contains capture groups, so return an array containing the
        // matched text in each capture group, unless the entire match fails.
        // Individual capture groups may also be null if that group did not
        // participate in the match.
        match needle.captures(haystack.unwrap_str()) {
            None => packer.push(Datum::Null),
            Some(captures) => packer.try_push_array(
                &[ArrayDimension {
                    lower_bound: 1,
                    length: captures.len() - 1,
                }],
                // Skip the 0th capture group, which is the whole match.
                captures.iter().skip(1).map(|mtch| match mtch {
                    None => Datum::Null,
                    Some(mtch) => Datum::String(mtch.as_str()),
                }),
            )?,
        }
    } else {
        // The regex contains no capture groups, so return a one-element array
        // containing the match, or null if there is no match.
        match needle.find(haystack.unwrap_str()) {
            None => packer.push(Datum::Null),
            Some(mtch) => packer.try_push_array(
                &[ArrayDimension {
                    lower_bound: 1,
                    length: 1,
                }],
                iter::once(Datum::String(mtch.as_str())),
            )?,
        };
    };
    Ok(temp_storage.push_unary_row(row))
}

fn regexp_replace_dynamic<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let source = datums[0];
    let pattern = datums[1];
    let replacement = datums[2];
    let flags = match datums.get(3) {
        Some(d) => d.unwrap_str(),
        None => "",
    };
    let (limit, flags) = regexp_replace_parse_flags(flags);
    let regexp = build_regex(pattern.unwrap_str(), &flags)?;
    regexp_replace_static(source, replacement, &regexp, limit, temp_storage)
}

/// Sets `limit` based on the presence of 'g' in `flags` for use in `Regex::replacen`,
/// and removes 'g' from `flags` if present.
pub(crate) fn regexp_replace_parse_flags(flags: &str) -> (usize, Cow<str>) {
    // 'g' means to replace all instead of the first. Use a Cow to avoid allocating in the fast
    // path. We could switch build_regex to take an iter which would also achieve that.
    let (limit, flags) = if flags.contains('g') {
        let flags = flags.replace('g', "");
        (0, Cow::Owned(flags))
    } else {
        (1, Cow::Borrowed(flags))
    };
    (limit, flags)
}

fn regexp_replace_static<'a>(
    source: Datum<'a>,
    replacement: Datum<'a>,
    regexp: &regex::Regex,
    limit: usize,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let replaced = match regexp.replacen(source.unwrap_str(), limit, replacement.unwrap_str()) {
        Cow::Borrowed(s) => s,
        Cow::Owned(s) => temp_storage.push_string(s),
    };
    Ok(Datum::String(replaced))
}

pub fn build_regex(needle: &str, flags: &str) -> Result<Regex, EvalError> {
    let mut case_insensitive = false;
    // Note: Postgres accepts it when both flags are present, taking the last one. We do the same.
    for f in flags.chars() {
        match f {
            'i' => {
                case_insensitive = true;
            }
            'c' => {
                case_insensitive = false;
            }
            _ => return Err(EvalError::InvalidRegexFlag(f)),
        }
    }
    Ok(Regex::new(needle, case_insensitive)?)
}

pub fn hmac_string<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let to_digest = datums[0].unwrap_str().as_bytes();
    let key = datums[1].unwrap_str().as_bytes();
    let typ = datums[2].unwrap_str();
    hmac_inner(to_digest, key, typ, temp_storage)
}

pub fn hmac_bytes<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let to_digest = datums[0].unwrap_bytes();
    let key = datums[1].unwrap_bytes();
    let typ = datums[2].unwrap_str();
    hmac_inner(to_digest, key, typ, temp_storage)
}

pub fn hmac_inner<'a>(
    to_digest: &[u8],
    key: &[u8],
    typ: &str,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let bytes = match typ {
        "md5" => {
            let mut mac = Hmac::<Md5>::new_from_slice(key).expect("HMAC accepts any key size");
            mac.update(to_digest);
            mac.finalize().into_bytes().to_vec()
        }
        "sha1" => {
            let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("HMAC accepts any key size");
            mac.update(to_digest);
            mac.finalize().into_bytes().to_vec()
        }
        "sha224" => {
            let mut mac = Hmac::<Sha224>::new_from_slice(key).expect("HMAC accepts any key size");
            mac.update(to_digest);
            mac.finalize().into_bytes().to_vec()
        }
        "sha256" => {
            let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key size");
            mac.update(to_digest);
            mac.finalize().into_bytes().to_vec()
        }
        "sha384" => {
            let mut mac = Hmac::<Sha384>::new_from_slice(key).expect("HMAC accepts any key size");
            mac.update(to_digest);
            mac.finalize().into_bytes().to_vec()
        }
        "sha512" => {
            let mut mac = Hmac::<Sha512>::new_from_slice(key).expect("HMAC accepts any key size");
            mac.update(to_digest);
            mac.finalize().into_bytes().to_vec()
        }
        other => return Err(EvalError::InvalidHashAlgorithm(other.into())),
    };
    Ok(Datum::Bytes(temp_storage.push_bytes(bytes)))
}

fn repeat_string<'a>(
    string: Datum<'a>,
    count: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let len = usize::try_from(count.unwrap_int32()).unwrap_or(0);
    let string = string.unwrap_str();
    if (len * string.len()) > MAX_STRING_BYTES {
        return Err(EvalError::LengthTooLarge);
    }
    Ok(Datum::String(temp_storage.push_string(string.repeat(len))))
}

fn replace<'a>(datums: &[Datum<'a>], temp_storage: &'a RowArena) -> Datum<'a> {
    Datum::String(
        temp_storage.push_string(
            datums[0]
                .unwrap_str()
                .replace(datums[1].unwrap_str(), datums[2].unwrap_str()),
        ),
    )
}

fn translate<'a>(datums: &[Datum<'a>], temp_storage: &'a RowArena) -> Datum<'a> {
    let string = datums[0].unwrap_str();
    let from = datums[1].unwrap_str().chars().collect::<Vec<_>>();
    let to = datums[2].unwrap_str().chars().collect::<Vec<_>>();

    Datum::String(
        temp_storage.push_string(
            string
                .chars()
                .filter_map(|c| match from.iter().position(|f| f == &c) {
                    Some(idx) => to.get(idx).copied(),
                    None => Some(c),
                })
                .collect(),
        ),
    )
}

fn jsonb_build_array<'a>(datums: &[Datum<'a>], temp_storage: &'a RowArena) -> Datum<'a> {
    temp_storage.make_datum(|packer| {
        packer.push_list(datums.into_iter().map(|d| match d {
            Datum::Null => Datum::JsonNull,
            d => *d,
        }))
    })
}

fn jsonb_build_object<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let mut kvs = datums.chunks(2).collect::<Vec<_>>();
    kvs.sort_by(|kv1, kv2| kv1[0].cmp(&kv2[0]));
    kvs.dedup_by(|kv1, kv2| kv1[0] == kv2[0]);
    temp_storage.try_make_datum(|packer| {
        packer.push_dict_with(|packer| {
            for kv in kvs {
                let k = kv[0];
                if k.is_null() {
                    return Err(EvalError::KeyCannotBeNull);
                };
                let v = match kv[1] {
                    Datum::Null => Datum::JsonNull,
                    d => d,
                };
                packer.push(k);
                packer.push(v);
            }
            Ok(())
        })
    })
}

fn map_build<'a>(datums: &[Datum<'a>], temp_storage: &'a RowArena) -> Datum<'a> {
    // Collect into a `BTreeMap` to provide the same semantics as it.
    let map: std::collections::BTreeMap<&str, _> = datums
        .into_iter()
        .tuples()
        .filter_map(|(k, v)| {
            if k.is_null() {
                None
            } else {
                Some((k.unwrap_str(), v))
            }
        })
        .collect();

    temp_storage.make_datum(|packer| packer.push_dict(map))
}

/// Constructs a new multidimensional array out of an arbitrary number of
/// lower-dimensional arrays.
///
/// For example, if given three 1D arrays of length 2, this function will
/// construct a 2D array with dimensions 3x2.
///
/// The input datums in `datums` must all be arrays of the same dimensions.
/// (The arrays must also be of the same element type, but that is checked by
/// the SQL type system, rather than checked here at runtime.)
///
/// If all input arrays are zero-dimensional arrays, then the output is a zero-
/// dimensional array. Otherwise the lower bound of the additional dimension is
/// one and the length of the new dimension is equal to `datums.len()`.
fn array_create_multidim<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    // Per PostgreSQL, if all input arrays are zero dimensional, so is the
    // output.
    if datums.iter().all(|d| d.unwrap_array().dims().is_empty()) {
        let dims = &[];
        let datums = &[];
        let datum = temp_storage.try_make_datum(|packer| packer.try_push_array(dims, datums))?;
        return Ok(datum);
    }

    let mut dims = vec![ArrayDimension {
        lower_bound: 1,
        length: datums.len(),
    }];
    if let Some(d) = datums.first() {
        dims.extend(d.unwrap_array().dims());
    };
    let elements = datums
        .iter()
        .flat_map(|d| d.unwrap_array().elements().iter());
    let datum =
        temp_storage.try_make_datum(move |packer| packer.try_push_array(&dims, elements))?;
    Ok(datum)
}

/// Constructs a new zero or one dimensional array out of an arbitrary number of
/// scalars.
///
/// If `datums` is empty, constructs a zero-dimensional array. Otherwise,
/// constructs a one dimensional array whose lower bound is one and whose length
/// is equal to `datums.len()`.
fn array_create_scalar<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let mut dims = &[ArrayDimension {
        lower_bound: 1,
        length: datums.len(),
    }][..];
    if datums.is_empty() {
        // Per PostgreSQL, empty arrays are represented with zero dimensions,
        // not one dimension of zero length. We write this condition a little
        // strangely to satisfy the borrow checker while avoiding an allocation.
        dims = &[];
    }
    let datum = temp_storage.try_make_datum(|packer| packer.try_push_array(dims, datums))?;
    Ok(datum)
}

fn array_to_string<'a>(
    datums: &[Datum<'a>],
    elem_type: &ScalarType,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    if datums[0].is_null() || datums[1].is_null() {
        return Ok(Datum::Null);
    }
    let array = datums[0].unwrap_array();
    let delimiter = datums[1].unwrap_str();
    let null_str = match datums.get(2) {
        None | Some(Datum::Null) => None,
        Some(d) => Some(d.unwrap_str()),
    };

    let mut out = String::new();
    for elem in array.elements().iter() {
        if elem.is_null() {
            if let Some(null_str) = null_str {
                out.push_str(null_str);
                out.push_str(delimiter);
            }
        } else {
            stringify_datum(&mut out, elem, elem_type)?;
            out.push_str(delimiter);
        }
    }
    if out.len() > 0 {
        // Lop off last delimiter only if string is not empty
        out.truncate(out.len() - delimiter.len());
    }
    Ok(Datum::String(temp_storage.push_string(out)))
}

fn list_create<'a>(datums: &[Datum<'a>], temp_storage: &'a RowArena) -> Datum<'a> {
    temp_storage.make_datum(|packer| packer.push_list(datums))
}

fn stringify_datum<'a, B>(
    buf: &mut B,
    d: Datum<'a>,
    ty: &ScalarType,
) -> Result<strconv::Nestable, EvalError>
where
    B: FormatBuffer,
{
    use ScalarType::*;
    match &ty {
        AclItem => Ok(strconv::format_acl_item(buf, d.unwrap_acl_item())),
        Bool => Ok(strconv::format_bool(buf, d.unwrap_bool())),
        Int16 => Ok(strconv::format_int16(buf, d.unwrap_int16())),
        Int32 => Ok(strconv::format_int32(buf, d.unwrap_int32())),
        Int64 => Ok(strconv::format_int64(buf, d.unwrap_int64())),
        UInt16 => Ok(strconv::format_uint16(buf, d.unwrap_uint16())),
        UInt32 | Oid | RegClass | RegProc | RegType => {
            Ok(strconv::format_uint32(buf, d.unwrap_uint32()))
        }
        UInt64 => Ok(strconv::format_uint64(buf, d.unwrap_uint64())),
        Float32 => Ok(strconv::format_float32(buf, d.unwrap_float32())),
        Float64 => Ok(strconv::format_float64(buf, d.unwrap_float64())),
        Numeric { .. } => Ok(strconv::format_numeric(buf, &d.unwrap_numeric())),
        Date => Ok(strconv::format_date(buf, d.unwrap_date())),
        Time => Ok(strconv::format_time(buf, d.unwrap_time())),
        Timestamp { .. } => Ok(strconv::format_timestamp(buf, &d.unwrap_timestamp())),
        TimestampTz { .. } => Ok(strconv::format_timestamptz(buf, &d.unwrap_timestamptz())),
        Interval => Ok(strconv::format_interval(buf, d.unwrap_interval())),
        Bytes => Ok(strconv::format_bytes(buf, d.unwrap_bytes())),
        String | VarChar { .. } | PgLegacyName => Ok(strconv::format_string(buf, d.unwrap_str())),
        Char { length } => Ok(strconv::format_string(
            buf,
            &mz_repr::adt::char::format_str_pad(d.unwrap_str(), *length),
        )),
        PgLegacyChar => {
            format_pg_legacy_char(buf, d.unwrap_uint8())?;
            Ok(strconv::Nestable::MayNeedEscaping)
        }
        Jsonb => Ok(strconv::format_jsonb(buf, JsonbRef::from_datum(d))),
        Uuid => Ok(strconv::format_uuid(buf, d.unwrap_uuid())),
        Record { fields, .. } => {
            let mut fields = fields.iter();
            strconv::format_record(buf, &d.unwrap_list(), |buf, d| {
                let (_name, ty) = fields.next().unwrap();
                if d.is_null() {
                    Ok(buf.write_null())
                } else {
                    stringify_datum(buf.nonnull_buffer(), d, &ty.scalar_type)
                }
            })
        }
        Array(elem_type) => strconv::format_array(
            buf,
            &d.unwrap_array().dims().into_iter().collect::<Vec<_>>(),
            &d.unwrap_array().elements(),
            |buf, d| {
                if d.is_null() {
                    Ok(buf.write_null())
                } else {
                    stringify_datum(buf.nonnull_buffer(), d, elem_type)
                }
            },
        ),
        List { element_type, .. } => strconv::format_list(buf, &d.unwrap_list(), |buf, d| {
            if d.is_null() {
                Ok(buf.write_null())
            } else {
                stringify_datum(buf.nonnull_buffer(), d, element_type)
            }
        }),
        Map { value_type, .. } => strconv::format_map(buf, &d.unwrap_map(), |buf, d| {
            if d.is_null() {
                Ok(buf.write_null())
            } else {
                stringify_datum(buf.nonnull_buffer(), d, value_type)
            }
        }),
        Int2Vector => strconv::format_legacy_vector(buf, &d.unwrap_array().elements(), |buf, d| {
            stringify_datum(buf.nonnull_buffer(), d, &ScalarType::Int16)
        }),
        MzTimestamp { .. } => Ok(strconv::format_mz_timestamp(buf, d.unwrap_mz_timestamp())),
        Range { element_type } => strconv::format_range(buf, &d.unwrap_range(), |buf, d| match d {
            Some(d) => stringify_datum(buf.nonnull_buffer(), *d, element_type),
            None => Ok::<_, EvalError>(buf.write_null()),
        }),
        MzAclItem => Ok(strconv::format_mz_acl_item(buf, d.unwrap_mz_acl_item())),
    }
}

fn array_index<'a>(datums: &[Datum<'a>], offset: i64) -> Datum<'a> {
    mz_ore::soft_assert_no_log!(offset == 0 || offset == 1, "offset must be either 0 or 1");

    let array = datums[0].unwrap_array();
    let dims = array.dims();
    if dims.len() != datums.len() - 1 {
        // You missed the datums "layer"
        return Datum::Null;
    }

    let mut final_idx = 0;

    for (d, idx) in dims.into_iter().zip_eq(datums[1..].iter()) {
        // Lower bound is written in terms of 1-based indexing, which offset accounts for.
        let idx = isize::cast_from(idx.unwrap_int64() + offset);

        let (lower, upper) = d.dimension_bounds();

        // This index missed all of the data at this layer. The dimension bounds are inclusive,
        // while range checks are exclusive, so adjust.
        if !(lower..upper + 1).contains(&idx) {
            return Datum::Null;
        }

        // We discover how many indices our last index represents physically.
        final_idx *= d.length;

        // Because both index and lower bound are handled in 1-based indexing, taking their
        // difference moves us back into 0-based indexing. Similarly, if the lower bound is
        // negative, subtracting a negative value >= to itself ensures its non-negativity.
        final_idx += usize::try_from(idx - d.lower_bound)
            .expect("previous bounds check ensures phsical index is at least 0");
    }

    array
        .elements()
        .iter()
        .nth(final_idx)
        .unwrap_or(Datum::Null)
}

// TODO(benesch): remove potentially dangerous usage of `as`.
#[allow(clippy::as_conversions)]
fn list_index<'a>(datums: &[Datum<'a>]) -> Datum<'a> {
    let mut buf = datums[0];

    for i in datums[1..].iter() {
        if buf.is_null() {
            break;
        }

        let i = i.unwrap_int64();
        if i < 1 {
            return Datum::Null;
        }

        buf = buf
            .unwrap_list()
            .iter()
            .nth(i as usize - 1)
            .unwrap_or(Datum::Null);
    }
    buf
}

// TODO(benesch): remove potentially dangerous usage of `as`.
#[allow(clippy::as_conversions)]
fn list_slice_linear<'a>(datums: &[Datum<'a>], temp_storage: &'a RowArena) -> Datum<'a> {
    assert_eq!(
        datums.len() % 2,
        1,
        "expr::scalar::func::list_slice expects an odd number of arguments; 1 for list + 2 \
        for each start-end pair"
    );
    assert!(
        datums.len() > 2,
        "expr::scalar::func::list_slice expects at least 3 arguments; 1 for list + at least \
        one start-end pair"
    );

    let mut start_idx = 0;
    let mut total_length = usize::MAX;

    for (start, end) in datums[1..].iter().tuples::<(_, _)>() {
        let start = std::cmp::max(start.unwrap_int64(), 1);
        let end = end.unwrap_int64();

        // Result should be empty list.
        if start > end {
            start_idx = 0;
            total_length = 0;
            break;
        }

        let start_inner = start as usize - 1;
        // Start index only moves to geq positions.
        start_idx += start_inner;

        // Length index only moves to leq positions
        let length_inner = (end - start) as usize + 1;
        total_length = std::cmp::min(length_inner, total_length - start_inner);
    }

    let iter = datums[0]
        .unwrap_list()
        .iter()
        .skip(start_idx)
        .take(total_length);

    temp_storage.make_datum(|row| {
        row.push_list_with(|row| {
            // if iter is empty, will get the appropriate empty list.
            for d in iter {
                row.push(d);
            }
        });
    })
}

fn create_range<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let flags = match datums[2] {
        Datum::Null => {
            return Err(EvalError::InvalidRange(
                range::InvalidRangeError::NullRangeBoundFlags,
            ));
        }
        o => o.unwrap_str(),
    };

    let (lower_inclusive, upper_inclusive) = range::parse_range_bound_flags(flags)?;

    let mut range = Range::new(Some((
        RangeBound::new(datums[0], lower_inclusive),
        RangeBound::new(datums[1], upper_inclusive),
    )));

    range.canonicalize()?;

    Ok(temp_storage.make_datum(|row| {
        row.push_range(range).expect("errors already handled");
    }))
}

fn array_position<'a>(datums: &[Datum<'a>]) -> Result<Datum<'a>, EvalError> {
    let array = match datums[0] {
        Datum::Null => return Ok(Datum::Null),
        o => o.unwrap_array(),
    };

    if array.dims().len() > 1 {
        return Err(EvalError::MultiDimensionalArraySearch);
    }

    let search = datums[1];
    if search == Datum::Null {
        return Ok(Datum::Null);
    }

    let skip: usize = match datums.get(2) {
        Some(Datum::Null) => return Err(EvalError::MustNotBeNull("initial position".into())),
        None => 0,
        Some(o) => usize::try_from(o.unwrap_int32())
            .unwrap_or(0)
            .saturating_sub(1),
    };

    let r = array.elements().iter().skip(skip).position(|d| d == search);

    Ok(Datum::from(r.map(|p| {
        // Adjust count for the amount we skipped, plus 1 for adjustng to PG indexing scheme.
        i32::try_from(p + skip + 1).expect("fewer than i32::MAX elements in array")
    })))
}

// TODO(benesch): remove potentially dangerous usage of `as`.
#[allow(clippy::as_conversions)]
fn make_timestamp<'a>(datums: &[Datum<'a>]) -> Result<Datum<'a>, EvalError> {
    let year: i32 = match datums[0].unwrap_int64().try_into() {
        Ok(year) => year,
        Err(_) => return Ok(Datum::Null),
    };
    let month: u32 = match datums[1].unwrap_int64().try_into() {
        Ok(month) => month,
        Err(_) => return Ok(Datum::Null),
    };
    let day: u32 = match datums[2].unwrap_int64().try_into() {
        Ok(day) => day,
        Err(_) => return Ok(Datum::Null),
    };
    let hour: u32 = match datums[3].unwrap_int64().try_into() {
        Ok(day) => day,
        Err(_) => return Ok(Datum::Null),
    };
    let minute: u32 = match datums[4].unwrap_int64().try_into() {
        Ok(day) => day,
        Err(_) => return Ok(Datum::Null),
    };
    let second_float = datums[5].unwrap_float64();
    let second = second_float as u32;
    let micros = ((second_float - second as f64) * 1_000_000.0) as u32;
    let date = match NaiveDate::from_ymd_opt(year, month, day) {
        Some(date) => date,
        None => return Ok(Datum::Null),
    };
    let timestamp = match date.and_hms_micro_opt(hour, minute, second, micros) {
        Some(timestamp) => timestamp,
        None => return Ok(Datum::Null),
    };
    Ok(timestamp.try_into()?)
}

#[sqlfunc(output_type = "i32", propagates_nulls = true)]
fn position<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let substring: &'a str = a.unwrap_str();
    let string = b.unwrap_str();
    let char_index = string.find(substring);

    if let Some(char_index) = char_index {
        // find the index in char space
        let string_prefix = &string[0..char_index];

        let num_prefix_chars = string_prefix.chars().count();
        let num_prefix_chars = i32::try_from(num_prefix_chars)
            .map_err(|_| EvalError::Int32OutOfRange(num_prefix_chars.to_string().into()))?;

        Ok(Datum::Int32(num_prefix_chars + 1))
    } else {
        Ok(Datum::Int32(0))
    }
}

#[sqlfunc(output_type = "String", propagates_nulls = true)]
fn left<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let string: &'a str = a.unwrap_str();
    let n = i64::from(b.unwrap_int32());

    let mut byte_indices = string.char_indices().map(|(i, _)| i);

    let end_in_bytes = match n.cmp(&0) {
        Ordering::Equal => 0,
        Ordering::Greater => {
            let n = usize::try_from(n).map_err(|_| {
                EvalError::InvalidParameterValue(format!("invalid parameter n: {:?}", n).into())
            })?;
            // nth from the back
            byte_indices.nth(n).unwrap_or(string.len())
        }
        Ordering::Less => {
            let n = usize::try_from(n.abs() - 1).map_err(|_| {
                EvalError::InvalidParameterValue(format!("invalid parameter n: {:?}", n).into())
            })?;
            byte_indices.rev().nth(n).unwrap_or(0)
        }
    };

    Ok(Datum::String(&string[..end_in_bytes]))
}

#[sqlfunc(output_type = "String", propagates_nulls = true)]
fn right<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let string: &'a str = a.unwrap_str();
    let n = b.unwrap_int32();

    let mut byte_indices = string.char_indices().map(|(i, _)| i);

    let start_in_bytes = if n == 0 {
        string.len()
    } else if n > 0 {
        let n = usize::try_from(n - 1).map_err(|_| {
            EvalError::InvalidParameterValue(format!("invalid parameter n: {:?}", n).into())
        })?;
        // nth from the back
        byte_indices.rev().nth(n).unwrap_or(0)
    } else if n == i32::MIN {
        // this seems strange but Postgres behaves like this
        0
    } else {
        let n = n.abs();
        let n = usize::try_from(n).map_err(|_| {
            EvalError::InvalidParameterValue(format!("invalid parameter n: {:?}", n).into())
        })?;
        byte_indices.nth(n).unwrap_or(string.len())
    };

    Ok(Datum::String(&string[start_in_bytes..]))
}

#[sqlfunc(sqlname = "btrim", output_type = "String", propagates_nulls = true)]
fn trim<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let trim_chars = b.unwrap_str();

    Datum::from(a.unwrap_str().trim_matches(|c| trim_chars.contains(c)))
}

#[sqlfunc(sqlname = "ltrim", output_type = "String", propagates_nulls = true)]
fn trim_leading<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let trim_chars = b.unwrap_str();

    Datum::from(
        a.unwrap_str()
            .trim_start_matches(|c| trim_chars.contains(c)),
    )
}

#[sqlfunc(sqlname = "rtrim", output_type = "String", propagates_nulls = true)]
fn trim_trailing<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let trim_chars = b.unwrap_str();

    Datum::from(a.unwrap_str().trim_end_matches(|c| trim_chars.contains(c)))
}

#[sqlfunc(
    output_type = "Option<i32>",
    is_infix_op = true,
    sqlname = "array_length",
    propagates_nulls = true,
    introduces_nulls = true
)]
fn array_length<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let i = match usize::try_from(b.unwrap_int64()) {
        Ok(0) | Err(_) => return Ok(Datum::Null),
        Ok(n) => n - 1,
    };
    Ok(match a.unwrap_array().dims().into_iter().nth(i) {
        None => Datum::Null,
        Some(dim) => Datum::Int32(
            dim.length
                .try_into()
                .map_err(|_| EvalError::Int32OutOfRange(dim.length.to_string().into()))?,
        ),
    })
}

#[sqlfunc(
    output_type = "Option<i32>",
    is_infix_op = true,
    sqlname = "array_lower",
    propagates_nulls = true,
    introduces_nulls = true
)]
// TODO(benesch): remove potentially dangerous usage of `as`.
#[allow(clippy::as_conversions)]
fn array_lower<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let i = b.unwrap_int64();
    if i < 1 {
        return Datum::Null;
    }
    match a.unwrap_array().dims().into_iter().nth(i as usize - 1) {
        Some(_) => Datum::Int32(1),
        None => Datum::Null,
    }
}

#[sqlfunc(
    output_type_expr = "input_type_a.scalar_type.without_modifiers().nullable(true)",
    sqlname = "array_remove",
    propagates_nulls = false,
    introduces_nulls = false
)]
fn array_remove<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    if a.is_null() {
        return Ok(a);
    }

    let arr = a.unwrap_array();

    // Zero-dimensional arrays are empty by definition
    if arr.dims().len() == 0 {
        return Ok(a);
    }

    // array_remove only supports one-dimensional arrays
    if arr.dims().len() > 1 {
        return Err(EvalError::MultidimensionalArrayRemovalNotSupported);
    }

    let elems: Vec<_> = arr.elements().iter().filter(|v| v != &b).collect();
    let mut dims = arr.dims().into_iter().collect::<Vec<_>>();
    // This access is safe because `dims` is guaranteed to be non-empty
    dims[0] = ArrayDimension {
        lower_bound: 1,
        length: elems.len(),
    };

    Ok(temp_storage.try_make_datum(|packer| packer.try_push_array(&dims, elems))?)
}

#[sqlfunc(
    output_type = "Option<i32>",
    is_infix_op = true,
    sqlname = "array_upper",
    propagates_nulls = true,
    introduces_nulls = true
)]
// TODO(benesch): remove potentially dangerous usage of `as`.
#[allow(clippy::as_conversions)]
fn array_upper<'a>(a: Datum<'a>, b: Datum<'a>) -> Result<Datum<'a>, EvalError> {
    let i = b.unwrap_int64();
    if i < 1 {
        return Ok(Datum::Null);
    }
    Ok(
        match a.unwrap_array().dims().into_iter().nth(i as usize - 1) {
            Some(dim) => Datum::Int32(
                dim.length
                    .try_into()
                    .map_err(|_| EvalError::Int32OutOfRange(dim.length.to_string().into()))?,
            ),
            None => Datum::Null,
        },
    )
}

// TODO(benesch): remove potentially dangerous usage of `as`.
#[allow(clippy::as_conversions)]
fn list_length_max<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    max_layer: usize,
) -> Result<Datum<'a>, EvalError> {
    fn max_len_on_layer<'a>(d: Datum<'a>, on_layer: i64) -> Option<usize> {
        match d {
            Datum::List(i) => {
                let mut i = i.iter();
                if on_layer > 1 {
                    let mut max_len = None;
                    while let Some(Datum::List(i)) = i.next() {
                        max_len =
                            std::cmp::max(max_len_on_layer(Datum::List(i), on_layer - 1), max_len);
                    }
                    max_len
                } else {
                    Some(i.count())
                }
            }
            Datum::Null => None,
            _ => unreachable!(),
        }
    }

    let b = b.unwrap_int64();

    if b as usize > max_layer || b < 1 {
        Err(EvalError::InvalidLayer { max_layer, val: b })
    } else {
        match max_len_on_layer(a, b) {
            Some(l) => match l.try_into() {
                Ok(c) => Ok(Datum::Int32(c)),
                Err(_) => Err(EvalError::Int32OutOfRange(l.to_string().into())),
            },
            None => Ok(Datum::Null),
        }
    }
}

#[sqlfunc(
    output_type = "bool",
    is_infix_op = true,
    sqlname = "array_contains",
    propagates_nulls = true,
    introduces_nulls = false
)]
fn array_contains<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let array = Datum::unwrap_array(&b);
    Datum::from(array.elements().iter().any(|e| e == a))
}

#[sqlfunc(
    output_type = "bool",
    is_infix_op = true,
    sqlname = "@>",
    propagates_nulls = true,
    introduces_nulls = false
)]
fn array_contains_array<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    let a = a.unwrap_array().elements();
    let b = b.unwrap_array().elements();

    // NULL is never equal to NULL. If NULL is an element of b, b cannot be contained in a, even if a contains NULL.
    if b.iter().contains(&Datum::Null) {
        Datum::False
    } else {
        b.iter()
            .all(|item_b| a.iter().any(|item_a| item_a == item_b))
            .into()
    }
}

#[sqlfunc(
    output_type = "bool",
    is_infix_op = true,
    sqlname = "<@",
    propagates_nulls = true,
    introduces_nulls = false
)]
fn array_contains_array_rev<'a>(a: Datum<'a>, b: Datum<'a>) -> Datum<'a> {
    array_contains_array(a, b)
}

#[sqlfunc(
    output_type_expr = "input_type_a.scalar_type.without_modifiers().nullable(true)",
    is_infix_op = true,
    sqlname = "||",
    propagates_nulls = false,
    introduces_nulls = false
)]
fn array_array_concat<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    if a.is_null() {
        return Ok(b);
    } else if b.is_null() {
        return Ok(a);
    }

    let a_array = a.unwrap_array();
    let b_array = b.unwrap_array();

    let a_dims: Vec<ArrayDimension> = a_array.dims().into_iter().collect();
    let b_dims: Vec<ArrayDimension> = b_array.dims().into_iter().collect();

    let a_ndims = a_dims.len();
    let b_ndims = b_dims.len();

    // Per PostgreSQL, if either of the input arrays is zero dimensional,
    // the output is the other array, no matter their dimensions.
    if a_ndims == 0 {
        return Ok(b);
    } else if b_ndims == 0 {
        return Ok(a);
    }

    // Postgres supports concatenating arrays of different dimensions,
    // as long as one of the arrays has the same type as an element of
    // the other array, i.e. `int[2][4] || int[4]` (or `int[4] || int[2][4]`)
    // works, because each element of `int[2][4]` is an `int[4]`.
    // This check is separate from the one below because Postgres gives a
    // specific error message if the number of dimensions differs by more
    // than one.
    // This cast is safe since MAX_ARRAY_DIMENSIONS is 6
    // Can be replaced by .abs_diff once it is stabilized
    // TODO(benesch): remove potentially dangerous usage of `as`.
    #[allow(clippy::as_conversions)]
    if (a_ndims as isize - b_ndims as isize).abs() > 1 {
        return Err(EvalError::IncompatibleArrayDimensions {
            dims: Some((a_ndims, b_ndims)),
        });
    }

    let mut dims;

    // After the checks above, we are certain that:
    // - neither array is zero dimensional nor empty
    // - both arrays have the same number of dimensions, or differ
    //   at most by one.
    match a_ndims.cmp(&b_ndims) {
        // If both arrays have the same number of dimensions, validate
        // that their inner dimensions are the same and concatenate the
        // arrays.
        Ordering::Equal => {
            if &a_dims[1..] != &b_dims[1..] {
                return Err(EvalError::IncompatibleArrayDimensions { dims: None });
            }
            dims = vec![ArrayDimension {
                lower_bound: a_dims[0].lower_bound,
                length: a_dims[0].length + b_dims[0].length,
            }];
            dims.extend(&a_dims[1..]);
        }
        // If `a` has less dimensions than `b`, this is an element-array
        // concatenation, which requires that `a` has the same dimensions
        // as an element of `b`.
        Ordering::Less => {
            if &a_dims[..] != &b_dims[1..] {
                return Err(EvalError::IncompatibleArrayDimensions { dims: None });
            }
            dims = vec![ArrayDimension {
                lower_bound: b_dims[0].lower_bound,
                // Since `a` is treated as an element of `b`, the length of
                // the first dimension of `b` is incremented by one, as `a` is
                // non-empty.
                length: b_dims[0].length + 1,
            }];
            dims.extend(a_dims);
        }
        // If `a` has more dimensions than `b`, this is an array-element
        // concatenation, which requires that `b` has the same dimensions
        // as an element of `a`.
        Ordering::Greater => {
            if &a_dims[1..] != &b_dims[..] {
                return Err(EvalError::IncompatibleArrayDimensions { dims: None });
            }
            dims = vec![ArrayDimension {
                lower_bound: a_dims[0].lower_bound,
                // Since `b` is treated as an element of `a`, the length of
                // the first dimension of `a` is incremented by one, as `b`
                // is non-empty.
                length: a_dims[0].length + 1,
            }];
            dims.extend(b_dims);
        }
    }

    let elems = a_array.elements().iter().chain(b_array.elements().iter());

    Ok(temp_storage.try_make_datum(|packer| packer.try_push_array(&dims, elems))?)
}

#[sqlfunc(
    output_type_expr = "input_type_a.scalar_type.without_modifiers().nullable(true)",
    is_infix_op = true,
    sqlname = "||",
    propagates_nulls = false,
    introduces_nulls = false
)]
fn list_list_concat<'a>(a: Datum<'a>, b: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    if a.is_null() {
        return b;
    } else if b.is_null() {
        return a;
    }

    let a = a.unwrap_list().iter();
    let b = b.unwrap_list().iter();

    temp_storage.make_datum(|packer| packer.push_list(a.chain(b)))
}

#[sqlfunc(
    output_type_expr = "input_type_a.scalar_type.without_modifiers().nullable(true)",
    is_infix_op = true,
    sqlname = "||",
    propagates_nulls = false,
    introduces_nulls = false
)]
fn list_element_concat<'a>(a: Datum<'a>, b: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    temp_storage.make_datum(|packer| {
        packer.push_list_with(|packer| {
            if !a.is_null() {
                for elem in a.unwrap_list().iter() {
                    packer.push(elem);
                }
            }
            packer.push(b);
        })
    })
}

#[sqlfunc(
    output_type_expr = "input_type_a.scalar_type.without_modifiers().nullable(true)",
    is_infix_op = true,
    sqlname = "||",
    propagates_nulls = false,
    introduces_nulls = false
)]
fn element_list_concat<'a>(a: Datum<'a>, b: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    temp_storage.make_datum(|packer| {
        packer.push_list_with(|packer| {
            packer.push(a);
            if !b.is_null() {
                for elem in b.unwrap_list().iter() {
                    packer.push(elem);
                }
            }
        })
    })
}

#[sqlfunc(
    output_type_expr = "input_type_a.scalar_type.without_modifiers().nullable(true)",
    sqlname = "list_remove",
    propagates_nulls = false,
    introduces_nulls = false
)]
fn list_remove<'a>(a: Datum<'a>, b: Datum<'a>, temp_storage: &'a RowArena) -> Datum<'a> {
    if a.is_null() {
        return a;
    }

    temp_storage.make_datum(|packer| {
        packer.push_list_with(|packer| {
            for elem in a.unwrap_list().iter() {
                if elem != b {
                    packer.push(elem);
                }
            }
        })
    })
}

#[sqlfunc(
    output_type = "Vec<u8>",
    sqlname = "digest",
    propagates_nulls = true,
    introduces_nulls = false
)]
fn digest_string<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let to_digest = a.unwrap_str().as_bytes();
    digest_inner(to_digest, b, temp_storage)
}

#[sqlfunc(
    output_type = "Vec<u8>",
    sqlname = "digest",
    propagates_nulls = true,
    introduces_nulls = false
)]
fn digest_bytes<'a>(
    a: Datum<'a>,
    b: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let to_digest = a.unwrap_bytes();
    digest_inner(to_digest, b, temp_storage)
}

fn digest_inner<'a>(
    bytes: &[u8],
    digest_fn: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let bytes = match digest_fn.unwrap_str() {
        "md5" => Md5::digest(bytes).to_vec(),
        "sha1" => Sha1::digest(bytes).to_vec(),
        "sha224" => Sha224::digest(bytes).to_vec(),
        "sha256" => Sha256::digest(bytes).to_vec(),
        "sha384" => Sha384::digest(bytes).to_vec(),
        "sha512" => Sha512::digest(bytes).to_vec(),
        other => return Err(EvalError::InvalidHashAlgorithm(other.into())),
    };
    Ok(Datum::Bytes(temp_storage.push_bytes(bytes)))
}

#[sqlfunc(
    output_type = "String",
    sqlname = "mz_render_typmod",
    propagates_nulls = true,
    introduces_nulls = false
)]
fn mz_render_typmod<'a>(
    oid: Datum<'a>,
    typmod: Datum<'a>,
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    let oid = oid.unwrap_uint32();
    let typmod = typmod.unwrap_int32();
    let s = match Type::from_oid_and_typmod(oid, typmod) {
        Ok(typ) => typ.constraint().display_or("").to_string(),
        // Match dubious PostgreSQL behavior of outputting the unmodified
        // `typmod` when positive if the type OID/typmod is invalid.
        Err(_) if typmod >= 0 => format!("({typmod})"),
        Err(_) => "".into(),
    };
    Ok(Datum::String(temp_storage.push_string(s)))
}

fn make_acl_item<'a>(datums: &[Datum<'a>]) -> Result<Datum<'a>, EvalError> {
    let grantee = Oid(datums[0].unwrap_uint32());
    let grantor = Oid(datums[1].unwrap_uint32());
    let privileges = datums[2].unwrap_str();
    let acl_mode = AclMode::parse_multiple_privileges(privileges)
        .map_err(|e: anyhow::Error| EvalError::InvalidPrivileges(e.to_string().into()))?;
    let is_grantable = datums[3].unwrap_bool();
    if is_grantable {
        return Err(EvalError::Unsupported {
            feature: "GRANT OPTION".into(),
            discussion_no: None,
        });
    }

    Ok(Datum::AclItem(AclItem {
        grantee,
        grantor,
        acl_mode,
    }))
}

fn make_mz_acl_item<'a>(datums: &[Datum<'a>]) -> Result<Datum<'a>, EvalError> {
    let grantee: RoleId = datums[0]
        .unwrap_str()
        .parse()
        .map_err(|e: anyhow::Error| EvalError::InvalidRoleId(e.to_string().into()))?;
    let grantor: RoleId = datums[1]
        .unwrap_str()
        .parse()
        .map_err(|e: anyhow::Error| EvalError::InvalidRoleId(e.to_string().into()))?;
    if grantor == RoleId::Public {
        return Err(EvalError::InvalidRoleId(
            "mz_aclitem grantor cannot be PUBLIC role".into(),
        ));
    }
    let privileges = datums[2].unwrap_str();
    let acl_mode = AclMode::parse_multiple_privileges(privileges)
        .map_err(|e: anyhow::Error| EvalError::InvalidPrivileges(e.to_string().into()))?;

    Ok(Datum::MzAclItem(MzAclItem {
        grantee,
        grantor,
        acl_mode,
    }))
}

fn array_fill<'a>(
    datums: &[Datum<'a>],
    temp_storage: &'a RowArena,
) -> Result<Datum<'a>, EvalError> {
    const MAX_SIZE: usize = 1 << 28 - 1;
    const NULL_ARR_ERR: &str = "dimension array or low bound array";
    const NULL_ELEM_ERR: &str = "dimension values";

    let fill = datums[0];
    if matches!(fill, Datum::Array(_)) {
        return Err(EvalError::Unsupported {
            feature: "array_fill with arrays".into(),
            discussion_no: None,
        });
    }

    let arr = match datums[1] {
        Datum::Null => return Err(EvalError::MustNotBeNull(NULL_ARR_ERR.into())),
        o => o.unwrap_array(),
    };

    let dimensions = arr
        .elements()
        .iter()
        .map(|d| match d {
            Datum::Null => Err(EvalError::MustNotBeNull(NULL_ELEM_ERR.into())),
            d => Ok(usize::cast_from(u32::reinterpret_cast(d.unwrap_int32()))),
        })
        .collect::<Result<Vec<_>, _>>()?;

    let lower_bounds = match datums.get(2) {
        Some(d) => {
            let arr = match d {
                Datum::Null => return Err(EvalError::MustNotBeNull(NULL_ARR_ERR.into())),
                o => o.unwrap_array(),
            };

            arr.elements()
                .iter()
                .map(|l| match l {
                    Datum::Null => Err(EvalError::MustNotBeNull(NULL_ELEM_ERR.into())),
                    l => Ok(isize::cast_from(l.unwrap_int32())),
                })
                .collect::<Result<Vec<_>, _>>()?
        }
        None => {
            vec![1isize; dimensions.len()]
        }
    };

    if lower_bounds.len() != dimensions.len() {
        return Err(EvalError::ArrayFillWrongArraySubscripts);
    }

    let fill_count: usize = dimensions
        .iter()
        .cloned()
        .map(Some)
        .reduce(|a, b| match (a, b) {
            (Some(a), Some(b)) => a.checked_mul(b),
            _ => None,
        })
        .flatten()
        .ok_or(EvalError::MaxArraySizeExceeded(MAX_SIZE))?;

    if matches!(
        mz_repr::datum_size(&fill).checked_mul(fill_count),
        None | Some(MAX_SIZE..)
    ) {
        return Err(EvalError::MaxArraySizeExceeded(MAX_SIZE));
    }

    let array_dimensions = if fill_count == 0 {
        vec![ArrayDimension {
            lower_bound: 1,
            length: 0,
        }]
    } else {
        dimensions
            .into_iter()
            .zip_eq(lower_bounds)
            .map(|(length, lower_bound)| ArrayDimension {
                lower_bound,
                length,
            })
            .collect()
    };

    Ok(temp_storage.try_make_datum(|packer| {
        packer.try_push_array(&array_dimensions, vec![fill; fill_count])
    })?)
}

#[derive(Ord, PartialOrd, Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash, MzReflect)]
pub enum VariadicFunc {
    Coalesce,
    Greatest,
    Least,
    Concat,
    ConcatWs,
    MakeTimestamp,
    PadLeading,
    Substr,
    Replace,
    JsonbBuildArray,
    JsonbBuildObject,
    MapBuild {
        value_type: ScalarType,
    },
    ArrayCreate {
        // We need to know the element type to type empty arrays.
        elem_type: ScalarType,
    },
    ArrayToString {
        elem_type: ScalarType,
    },
    ArrayIndex {
        // Adjusts the index by offset depending on whether being called on an array or an
        // Int2Vector.
        offset: i64,
    },
    ListCreate {
        // We need to know the element type to type empty lists.
        elem_type: ScalarType,
    },
    RecordCreate {
        field_names: Vec<ColumnName>,
    },
    ListIndex,
    ListSliceLinear,
    SplitPart,
    RegexpMatch,
    HmacString,
    HmacBytes,
    ErrorIfNull,
    DateBinTimestamp,
    DateBinTimestampTz,
    DateDiffTimestamp,
    DateDiffTimestampTz,
    DateDiffDate,
    DateDiffTime,
    And,
    Or,
    RangeCreate {
        elem_type: ScalarType,
    },
    MakeAclItem,
    MakeMzAclItem,
    Translate,
    ArrayPosition,
    ArrayFill {
        elem_type: ScalarType,
    },
    StringToArray,
    TimezoneTime,
    RegexpSplitToArray,
    RegexpReplace,
}

impl VariadicFunc {
    pub fn eval<'a>(
        &'a self,
        datums: &[Datum<'a>],
        temp_storage: &'a RowArena,
        exprs: &'a [MirScalarExpr],
    ) -> Result<Datum<'a>, EvalError> {
        // Evaluate all non-eager functions directly
        match self {
            VariadicFunc::Coalesce => return coalesce(datums, temp_storage, exprs),
            VariadicFunc::Greatest => return greatest(datums, temp_storage, exprs),
            VariadicFunc::And => return and(datums, temp_storage, exprs),
            VariadicFunc::Or => return or(datums, temp_storage, exprs),
            VariadicFunc::ErrorIfNull => return error_if_null(datums, temp_storage, exprs),
            VariadicFunc::Least => return least(datums, temp_storage, exprs),
            _ => {}
        };

        // Compute parameters to eager functions
        let ds = exprs
            .iter()
            .map(|e| e.eval(datums, temp_storage))
            .collect::<Result<Vec<_>, _>>()?;
        // Check NULL propagation
        if self.propagates_nulls() && ds.iter().any(|d| d.is_null()) {
            return Ok(Datum::Null);
        }

        // Evaluate eager functions
        match self {
            VariadicFunc::Coalesce
            | VariadicFunc::Greatest
            | VariadicFunc::And
            | VariadicFunc::Or
            | VariadicFunc::ErrorIfNull
            | VariadicFunc::Least => unreachable!(),
            VariadicFunc::Concat => Ok(text_concat_variadic(&ds, temp_storage)),
            VariadicFunc::ConcatWs => Ok(text_concat_ws(&ds, temp_storage)),
            VariadicFunc::MakeTimestamp => make_timestamp(&ds),
            VariadicFunc::PadLeading => pad_leading(&ds, temp_storage),
            VariadicFunc::Substr => substr(&ds),
            VariadicFunc::Replace => Ok(replace(&ds, temp_storage)),
            VariadicFunc::Translate => Ok(translate(&ds, temp_storage)),
            VariadicFunc::JsonbBuildArray => Ok(jsonb_build_array(&ds, temp_storage)),
            VariadicFunc::JsonbBuildObject => jsonb_build_object(&ds, temp_storage),
            VariadicFunc::MapBuild { .. } => Ok(map_build(&ds, temp_storage)),
            VariadicFunc::ArrayCreate {
                elem_type: ScalarType::Array(_),
            } => array_create_multidim(&ds, temp_storage),
            VariadicFunc::ArrayCreate { .. } => array_create_scalar(&ds, temp_storage),
            VariadicFunc::ArrayToString { elem_type } => {
                array_to_string(&ds, elem_type, temp_storage)
            }
            VariadicFunc::ArrayIndex { offset } => Ok(array_index(&ds, *offset)),

            VariadicFunc::ListCreate { .. } | VariadicFunc::RecordCreate { .. } => {
                Ok(list_create(&ds, temp_storage))
            }
            VariadicFunc::ListIndex => Ok(list_index(&ds)),
            VariadicFunc::ListSliceLinear => Ok(list_slice_linear(&ds, temp_storage)),
            VariadicFunc::SplitPart => split_part(&ds),
            VariadicFunc::RegexpMatch => regexp_match_dynamic(&ds, temp_storage),
            VariadicFunc::HmacString => hmac_string(&ds, temp_storage),
            VariadicFunc::HmacBytes => hmac_bytes(&ds, temp_storage),
            VariadicFunc::DateBinTimestamp => date_bin(
                ds[0].unwrap_interval(),
                ds[1].unwrap_timestamp(),
                ds[2].unwrap_timestamp(),
            ),
            VariadicFunc::DateBinTimestampTz => date_bin(
                ds[0].unwrap_interval(),
                ds[1].unwrap_timestamptz(),
                ds[2].unwrap_timestamptz(),
            ),
            VariadicFunc::DateDiffTimestamp => date_diff_timestamp(ds[0], ds[1], ds[2]),
            VariadicFunc::DateDiffTimestampTz => date_diff_timestamptz(ds[0], ds[1], ds[2]),
            VariadicFunc::DateDiffDate => date_diff_date(ds[0], ds[1], ds[2]),
            VariadicFunc::DateDiffTime => date_diff_time(ds[0], ds[1], ds[2]),
            VariadicFunc::RangeCreate { .. } => create_range(&ds, temp_storage),
            VariadicFunc::MakeAclItem => make_acl_item(&ds),
            VariadicFunc::MakeMzAclItem => make_mz_acl_item(&ds),
            VariadicFunc::ArrayPosition => array_position(&ds),
            VariadicFunc::ArrayFill { .. } => array_fill(&ds, temp_storage),
            VariadicFunc::TimezoneTime => parse_timezone(ds[0].unwrap_str(), TimezoneSpec::Posix)
                .map(|tz| {
                    timezone_time(
                        tz,
                        ds[1].unwrap_time(),
                        &ds[2].unwrap_timestamptz().naive_utc(),
                    )
                    .into()
                }),
            VariadicFunc::RegexpSplitToArray => {
                let flags = if ds.len() == 2 {
                    Datum::String("")
                } else {
                    ds[2]
                };
                regexp_split_to_array(ds[0], ds[1], flags, temp_storage)
            }
            VariadicFunc::RegexpReplace => regexp_replace_dynamic(&ds, temp_storage),
            VariadicFunc::StringToArray => {
                let null_string = if ds.len() == 2 { Datum::Null } else { ds[2] };

                string_to_array(ds[0], ds[1], null_string, temp_storage)
            }
        }
    }

    pub fn is_associative(&self) -> bool {
        match self {
            VariadicFunc::Coalesce
            | VariadicFunc::Greatest
            | VariadicFunc::Least
            | VariadicFunc::Concat
            | VariadicFunc::And
            | VariadicFunc::Or => true,

            VariadicFunc::MakeTimestamp
            | VariadicFunc::PadLeading
            | VariadicFunc::ConcatWs
            | VariadicFunc::Substr
            | VariadicFunc::Replace
            | VariadicFunc::Translate
            | VariadicFunc::JsonbBuildArray
            | VariadicFunc::JsonbBuildObject
            | VariadicFunc::MapBuild { value_type: _ }
            | VariadicFunc::ArrayCreate { elem_type: _ }
            | VariadicFunc::ArrayToString { elem_type: _ }
            | VariadicFunc::ArrayIndex { offset: _ }
            | VariadicFunc::ListCreate { elem_type: _ }
            | VariadicFunc::RecordCreate { field_names: _ }
            | VariadicFunc::ListIndex
            | VariadicFunc::ListSliceLinear
            | VariadicFunc::SplitPart
            | VariadicFunc::RegexpMatch
            | VariadicFunc::HmacString
            | VariadicFunc::HmacBytes
            | VariadicFunc::ErrorIfNull
            | VariadicFunc::DateBinTimestamp
            | VariadicFunc::DateBinTimestampTz
            | VariadicFunc::DateDiffTimestamp
            | VariadicFunc::DateDiffTimestampTz
            | VariadicFunc::DateDiffDate
            | VariadicFunc::DateDiffTime
            | VariadicFunc::RangeCreate { .. }
            | VariadicFunc::MakeAclItem
            | VariadicFunc::MakeMzAclItem
            | VariadicFunc::ArrayPosition
            | VariadicFunc::ArrayFill { .. }
            | VariadicFunc::TimezoneTime
            | VariadicFunc::RegexpSplitToArray
            | VariadicFunc::StringToArray
            | VariadicFunc::RegexpReplace => false,
        }
    }

    pub fn output_type(&self, input_types: Vec<ColumnType>) -> ColumnType {
        use VariadicFunc::*;
        let in_nullable = input_types.iter().any(|t| t.nullable);
        match self {
            Greatest | Least => input_types
                .into_iter()
                .reduce(|l, r| l.union(&r).unwrap())
                .unwrap(),
            Coalesce => {
                // Note that the parser doesn't allow empty argument lists for variadic functions
                // that use the standard function call syntax (ArrayCreate and co. are different
                // because of the special syntax for calling them).
                let nullable = input_types.iter().all(|typ| typ.nullable);
                input_types
                    .into_iter()
                    .reduce(|l, r| l.union(&r).unwrap())
                    .unwrap()
                    .nullable(nullable)
            }
            Concat | ConcatWs => ScalarType::String.nullable(in_nullable),
            MakeTimestamp => ScalarType::Timestamp { precision: None }.nullable(true),
            PadLeading => ScalarType::String.nullable(in_nullable),
            Substr => ScalarType::String.nullable(in_nullable),
            Replace => ScalarType::String.nullable(in_nullable),
            Translate => ScalarType::String.nullable(in_nullable),
            JsonbBuildArray | JsonbBuildObject => ScalarType::Jsonb.nullable(true),
            MapBuild { value_type } => ScalarType::Map {
                value_type: Box::new(value_type.clone()),
                custom_id: None,
            }
            .nullable(true),
            ArrayCreate { elem_type } => {
                debug_assert!(
                    input_types.iter().all(|t| t.scalar_type.base_eq(elem_type)),
                    "Args to ArrayCreate should have types that are compatible with the elem_type"
                );
                match elem_type {
                    ScalarType::Array(_) => elem_type.clone().nullable(false),
                    _ => ScalarType::Array(Box::new(elem_type.clone())).nullable(false),
                }
            }
            ArrayToString { .. } => ScalarType::String.nullable(in_nullable),
            ArrayIndex { .. } => input_types[0]
                .scalar_type
                .unwrap_array_element_type()
                .clone()
                .nullable(true),
            ListCreate { elem_type } => {
                // commented out to work around
                // https://github.com/MaterializeInc/database-issues/issues/2730
                // soft_assert!(
                //     input_types.iter().all(|t| t.scalar_type.base_eq(elem_type)),
                //     "{}", format!("Args to ListCreate should have types that are compatible with the elem_type.\nArgs:{:#?}\nelem_type:{:#?}", input_types, elem_type)
                // );
                ScalarType::List {
                    element_type: Box::new(elem_type.clone()),
                    custom_id: None,
                }
                .nullable(false)
            }
            ListIndex => input_types[0]
                .scalar_type
                .unwrap_list_nth_layer_type(input_types.len() - 1)
                .clone()
                .nullable(true),
            ListSliceLinear { .. } => input_types[0].scalar_type.clone().nullable(in_nullable),
            RecordCreate { field_names } => ScalarType::Record {
                fields: field_names
                    .clone()
                    .into_iter()
                    .zip_eq(input_types)
                    .collect(),
                custom_id: None,
            }
            .nullable(false),
            SplitPart => ScalarType::String.nullable(in_nullable),
            RegexpMatch => ScalarType::Array(Box::new(ScalarType::String)).nullable(true),
            HmacString | HmacBytes => ScalarType::Bytes.nullable(in_nullable),
            ErrorIfNull => input_types[0].scalar_type.clone().nullable(false),
            DateBinTimestamp => ScalarType::Timestamp { precision: None }.nullable(in_nullable),
            DateBinTimestampTz => ScalarType::TimestampTz { precision: None }.nullable(in_nullable),
            DateDiffTimestamp => ScalarType::Int64.nullable(in_nullable),
            DateDiffTimestampTz => ScalarType::Int64.nullable(in_nullable),
            DateDiffDate => ScalarType::Int64.nullable(in_nullable),
            DateDiffTime => ScalarType::Int64.nullable(in_nullable),
            And | Or => ScalarType::Bool.nullable(in_nullable),
            RangeCreate { elem_type } => ScalarType::Range {
                element_type: Box::new(elem_type.clone()),
            }
            .nullable(false),
            MakeAclItem => ScalarType::AclItem.nullable(true),
            MakeMzAclItem => ScalarType::MzAclItem.nullable(true),
            ArrayPosition => ScalarType::Int32.nullable(true),
            ArrayFill { elem_type } => {
                ScalarType::Array(Box::new(elem_type.clone())).nullable(false)
            }
            TimezoneTime => ScalarType::Time.nullable(in_nullable),
            RegexpSplitToArray => {
                ScalarType::Array(Box::new(ScalarType::String)).nullable(in_nullable)
            }
            RegexpReplace => ScalarType::String.nullable(in_nullable),
            StringToArray => ScalarType::Array(Box::new(ScalarType::String)).nullable(true),
        }
    }

    /// Whether the function output is NULL if any of its inputs are NULL.
    ///
    /// NB: if any input is NULL the output will be returned as NULL without
    /// calling the function.
    pub fn propagates_nulls(&self) -> bool {
        // NOTE: The following is a list of the variadic functions
        // that **DO NOT** propagate nulls.
        !matches!(
            self,
            VariadicFunc::And
                | VariadicFunc::Or
                | VariadicFunc::Coalesce
                | VariadicFunc::Greatest
                | VariadicFunc::Least
                | VariadicFunc::Concat
                | VariadicFunc::ConcatWs
                | VariadicFunc::JsonbBuildArray
                | VariadicFunc::JsonbBuildObject
                | VariadicFunc::MapBuild { .. }
                | VariadicFunc::ListCreate { .. }
                | VariadicFunc::RecordCreate { .. }
                | VariadicFunc::ArrayCreate { .. }
                | VariadicFunc::ArrayToString { .. }
                | VariadicFunc::ErrorIfNull
                | VariadicFunc::RangeCreate { .. }
                | VariadicFunc::ArrayPosition
                | VariadicFunc::ArrayFill { .. }
                | VariadicFunc::StringToArray
        )
    }

    /// Whether the function might return NULL even if none of its inputs are
    /// NULL.
    ///
    /// This is presently conservative, and may indicate that a function
    /// introduces nulls even when it does not.
    pub fn introduces_nulls(&self) -> bool {
        use VariadicFunc::*;
        match self {
            Concat
            | ConcatWs
            | PadLeading
            | Substr
            | Replace
            | Translate
            | JsonbBuildArray
            | JsonbBuildObject
            | MapBuild { .. }
            | ArrayCreate { .. }
            | ArrayToString { .. }
            | ListCreate { .. }
            | RecordCreate { .. }
            | ListSliceLinear
            | SplitPart
            | HmacString
            | HmacBytes
            | ErrorIfNull
            | DateBinTimestamp
            | DateBinTimestampTz
            | DateDiffTimestamp
            | DateDiffTimestampTz
            | DateDiffDate
            | DateDiffTime
            | RangeCreate { .. }
            | And
            | Or
            | MakeAclItem
            | MakeMzAclItem
            | ArrayPosition
            | ArrayFill { .. }
            | TimezoneTime
            | RegexpSplitToArray
            | RegexpReplace => false,
            Coalesce
            | Greatest
            | Least
            | MakeTimestamp
            | ArrayIndex { .. }
            | StringToArray
            | ListIndex
            | RegexpMatch => true,
        }
    }

    pub fn switch_and_or(&self) -> Self {
        match self {
            VariadicFunc::And => VariadicFunc::Or,
            VariadicFunc::Or => VariadicFunc::And,
            _ => unreachable!(),
        }
    }

    pub fn is_infix_op(&self) -> bool {
        use VariadicFunc::*;
        matches!(self, And | Or)
    }

    /// Gives the unit (u) of OR or AND, such that `u AND/OR x == x`.
    /// Note that a 0-arg AND/OR evaluates to unit_of_and_or.
    pub fn unit_of_and_or(&self) -> MirScalarExpr {
        match self {
            VariadicFunc::And => MirScalarExpr::literal_true(),
            VariadicFunc::Or => MirScalarExpr::literal_false(),
            _ => unreachable!(),
        }
    }

    /// Gives the zero (z) of OR or AND, such that `z AND/OR x == z`.
    pub fn zero_of_and_or(&self) -> MirScalarExpr {
        match self {
            VariadicFunc::And => MirScalarExpr::literal_false(),
            VariadicFunc::Or => MirScalarExpr::literal_true(),
            _ => unreachable!(),
        }
    }

    /// Returns true if the function could introduce an error on non-error inputs.
    pub fn could_error(&self) -> bool {
        match self {
            VariadicFunc::And | VariadicFunc::Or => false,
            VariadicFunc::Coalesce => false,
            VariadicFunc::Greatest | VariadicFunc::Least => false,
            VariadicFunc::Concat | VariadicFunc::ConcatWs => false,
            VariadicFunc::Replace => false,
            VariadicFunc::Translate => false,
            VariadicFunc::ArrayIndex { .. } => false,
            VariadicFunc::ListCreate { .. } | VariadicFunc::RecordCreate { .. } => false,
            // All other cases are unknown
            _ => true,
        }
    }

    /// Returns true if the function is monotone. (Non-strict; either increasing or decreasing.)
    /// Monotone functions map ranges to ranges: ie. given a range of possible inputs, we can
    /// determine the range of possible outputs just by mapping the endpoints.
    ///
    /// This describes the *pointwise* behaviour of the function:
    /// ie. if more than one argument is provided, this describes the behaviour of
    /// any specific argument as the others are held constant. (For example, `COALESCE(a, b)` is
    /// monotone in `a` because for any particular value of `b`, increasing `a` will never
    /// cause the result to decrease.)
    ///
    /// This property describes the behaviour of the function over ranges where the function is defined:
    /// ie. the arguments and the result are non-error datums.
    pub fn is_monotone(&self) -> bool {
        match self {
            VariadicFunc::Coalesce
            | VariadicFunc::Greatest
            | VariadicFunc::Least
            | VariadicFunc::And
            | VariadicFunc::Or => true,
            VariadicFunc::Concat
            | VariadicFunc::ConcatWs
            | VariadicFunc::MakeTimestamp
            | VariadicFunc::PadLeading
            | VariadicFunc::Substr
            | VariadicFunc::Replace
            | VariadicFunc::JsonbBuildArray
            | VariadicFunc::JsonbBuildObject
            | VariadicFunc::MapBuild { .. }
            | VariadicFunc::ArrayCreate { .. }
            | VariadicFunc::ArrayToString { .. }
            | VariadicFunc::ArrayIndex { .. }
            | VariadicFunc::ListCreate { .. }
            | VariadicFunc::RecordCreate { .. }
            | VariadicFunc::ListIndex
            | VariadicFunc::ListSliceLinear
            | VariadicFunc::SplitPart
            | VariadicFunc::RegexpMatch
            | VariadicFunc::HmacString
            | VariadicFunc::HmacBytes
            | VariadicFunc::ErrorIfNull
            | VariadicFunc::DateBinTimestamp
            | VariadicFunc::DateBinTimestampTz
            | VariadicFunc::RangeCreate { .. }
            | VariadicFunc::MakeAclItem
            | VariadicFunc::MakeMzAclItem
            | VariadicFunc::Translate
            | VariadicFunc::ArrayPosition
            | VariadicFunc::ArrayFill { .. }
            | VariadicFunc::DateDiffTimestamp
            | VariadicFunc::DateDiffTimestampTz
            | VariadicFunc::DateDiffDate
            | VariadicFunc::DateDiffTime
            | VariadicFunc::TimezoneTime
            | VariadicFunc::RegexpSplitToArray
            | VariadicFunc::StringToArray
            | VariadicFunc::RegexpReplace => false,
        }
    }
}

impl fmt::Display for VariadicFunc {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            VariadicFunc::Coalesce => f.write_str("coalesce"),
            VariadicFunc::Greatest => f.write_str("greatest"),
            VariadicFunc::Least => f.write_str("least"),
            VariadicFunc::Concat => f.write_str("concat"),
            VariadicFunc::ConcatWs => f.write_str("concat_ws"),
            VariadicFunc::MakeTimestamp => f.write_str("makets"),
            VariadicFunc::PadLeading => f.write_str("lpad"),
            VariadicFunc::Substr => f.write_str("substr"),
            VariadicFunc::Replace => f.write_str("replace"),
            VariadicFunc::Translate => f.write_str("translate"),
            VariadicFunc::JsonbBuildArray => f.write_str("jsonb_build_array"),
            VariadicFunc::JsonbBuildObject => f.write_str("jsonb_build_object"),
            VariadicFunc::MapBuild { .. } => f.write_str("map_build"),
            VariadicFunc::ArrayCreate { .. } => f.write_str("array_create"),
            VariadicFunc::ArrayToString { .. } => f.write_str("array_to_string"),
            VariadicFunc::ArrayIndex { .. } => f.write_str("array_index"),
            VariadicFunc::ListCreate { .. } => f.write_str("list_create"),
            VariadicFunc::RecordCreate { .. } => f.write_str("record_create"),
            VariadicFunc::ListIndex => f.write_str("list_index"),
            VariadicFunc::ListSliceLinear => f.write_str("list_slice_linear"),
            VariadicFunc::SplitPart => f.write_str("split_string"),
            VariadicFunc::RegexpMatch => f.write_str("regexp_match"),
            VariadicFunc::HmacString | VariadicFunc::HmacBytes => f.write_str("hmac"),
            VariadicFunc::ErrorIfNull => f.write_str("error_if_null"),
            VariadicFunc::DateBinTimestamp => f.write_str("timestamp_bin"),
            VariadicFunc::DateBinTimestampTz => f.write_str("timestamptz_bin"),
            VariadicFunc::DateDiffTimestamp
            | VariadicFunc::DateDiffTimestampTz
            | VariadicFunc::DateDiffDate
            | VariadicFunc::DateDiffTime => f.write_str("datediff"),
            VariadicFunc::And => f.write_str("AND"),
            VariadicFunc::Or => f.write_str("OR"),
            VariadicFunc::RangeCreate {
                elem_type: element_type,
            } => f.write_str(match element_type {
                ScalarType::Int32 => "int4range",
                ScalarType::Int64 => "int8range",
                ScalarType::Date => "daterange",
                ScalarType::Numeric { .. } => "numrange",
                ScalarType::Timestamp { .. } => "tsrange",
                ScalarType::TimestampTz { .. } => "tstzrange",
                _ => unreachable!(),
            }),
            VariadicFunc::MakeAclItem => f.write_str("makeaclitem"),
            VariadicFunc::MakeMzAclItem => f.write_str("make_mz_aclitem"),
            VariadicFunc::ArrayPosition => f.write_str("array_position"),
            VariadicFunc::ArrayFill { .. } => f.write_str("array_fill"),
            VariadicFunc::TimezoneTime => f.write_str("timezonet"),
            VariadicFunc::RegexpSplitToArray => f.write_str("regexp_split_to_array"),
            VariadicFunc::RegexpReplace => f.write_str("regexp_replace"),
            VariadicFunc::StringToArray => f.write_str("string_to_array"),
        }
    }
}

/// An explicit [`Arbitrary`] implementation needed here because of a known
/// `proptest` issue.
///
/// Revert to the derive-macro impementation once the issue[^1] is fixed.
///
/// [^1]: <https://github.com/AltSysrq/proptest/issues/152>
impl Arbitrary for VariadicFunc {
    type Parameters = ();

    type Strategy = Union<BoxedStrategy<Self>>;

    fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
        Union::new(vec![
            Just(VariadicFunc::Coalesce).boxed(),
            Just(VariadicFunc::Greatest).boxed(),
            Just(VariadicFunc::Least).boxed(),
            Just(VariadicFunc::Concat).boxed(),
            Just(VariadicFunc::ConcatWs).boxed(),
            Just(VariadicFunc::MakeTimestamp).boxed(),
            Just(VariadicFunc::PadLeading).boxed(),
            Just(VariadicFunc::Substr).boxed(),
            Just(VariadicFunc::Replace).boxed(),
            Just(VariadicFunc::JsonbBuildArray).boxed(),
            Just(VariadicFunc::JsonbBuildObject).boxed(),
            ScalarType::arbitrary()
                .prop_map(|value_type| VariadicFunc::MapBuild { value_type })
                .boxed(),
            Just(VariadicFunc::MakeAclItem).boxed(),
            Just(VariadicFunc::MakeMzAclItem).boxed(),
            ScalarType::arbitrary()
                .prop_map(|elem_type| VariadicFunc::ArrayCreate { elem_type })
                .boxed(),
            ScalarType::arbitrary()
                .prop_map(|elem_type| VariadicFunc::ArrayToString { elem_type })
                .boxed(),
            i64::arbitrary()
                .prop_map(|offset| VariadicFunc::ArrayIndex { offset })
                .boxed(),
            ScalarType::arbitrary()
                .prop_map(|elem_type| VariadicFunc::ListCreate { elem_type })
                .boxed(),
            Vec::<ColumnName>::arbitrary()
                .prop_map(|field_names| VariadicFunc::RecordCreate { field_names })
                .boxed(),
            Just(VariadicFunc::ListIndex).boxed(),
            Just(VariadicFunc::ListSliceLinear).boxed(),
            Just(VariadicFunc::SplitPart).boxed(),
            Just(VariadicFunc::RegexpMatch).boxed(),
            Just(VariadicFunc::HmacString).boxed(),
            Just(VariadicFunc::HmacBytes).boxed(),
            Just(VariadicFunc::ErrorIfNull).boxed(),
            Just(VariadicFunc::DateBinTimestamp).boxed(),
            Just(VariadicFunc::DateBinTimestampTz).boxed(),
            Just(VariadicFunc::DateDiffTimestamp).boxed(),
            Just(VariadicFunc::DateDiffTimestampTz).boxed(),
            Just(VariadicFunc::DateDiffDate).boxed(),
            Just(VariadicFunc::DateDiffTime).boxed(),
            Just(VariadicFunc::And).boxed(),
            Just(VariadicFunc::Or).boxed(),
            mz_repr::arb_range_type()
                .prop_map(|elem_type| VariadicFunc::RangeCreate { elem_type })
                .boxed(),
            Just(VariadicFunc::ArrayPosition).boxed(),
            ScalarType::arbitrary()
                .prop_map(|elem_type| VariadicFunc::ArrayFill { elem_type })
                .boxed(),
        ])
    }
}

impl RustType<ProtoVariadicFunc> for VariadicFunc {
    fn into_proto(&self) -> ProtoVariadicFunc {
        use crate::scalar::proto_variadic_func::Kind::*;
        use crate::scalar::proto_variadic_func::ProtoRecordCreate;
        let kind = match self {
            VariadicFunc::Coalesce => Coalesce(()),
            VariadicFunc::Greatest => Greatest(()),
            VariadicFunc::Least => Least(()),
            VariadicFunc::Concat => Concat(()),
            VariadicFunc::ConcatWs => ConcatWs(()),
            VariadicFunc::MakeTimestamp => MakeTimestamp(()),
            VariadicFunc::PadLeading => PadLeading(()),
            VariadicFunc::Substr => Substr(()),
            VariadicFunc::Replace => Replace(()),
            VariadicFunc::Translate => Translate(()),
            VariadicFunc::JsonbBuildArray => JsonbBuildArray(()),
            VariadicFunc::JsonbBuildObject => JsonbBuildObject(()),
            VariadicFunc::MapBuild { value_type } => MapBuild(value_type.into_proto()),
            VariadicFunc::ArrayCreate { elem_type } => ArrayCreate(elem_type.into_proto()),
            VariadicFunc::ArrayToString { elem_type } => ArrayToString(elem_type.into_proto()),
            VariadicFunc::ArrayIndex { offset } => ArrayIndex(offset.into_proto()),
            VariadicFunc::ListCreate { elem_type } => ListCreate(elem_type.into_proto()),
            VariadicFunc::RecordCreate { field_names } => RecordCreate(ProtoRecordCreate {
                field_names: field_names.into_proto(),
            }),
            VariadicFunc::ListIndex => ListIndex(()),
            VariadicFunc::ListSliceLinear => ListSliceLinear(()),
            VariadicFunc::SplitPart => SplitPart(()),
            VariadicFunc::RegexpMatch => RegexpMatch(()),
            VariadicFunc::HmacString => HmacString(()),
            VariadicFunc::HmacBytes => HmacBytes(()),
            VariadicFunc::ErrorIfNull => ErrorIfNull(()),
            VariadicFunc::DateBinTimestamp => DateBinTimestamp(()),
            VariadicFunc::DateBinTimestampTz => DateBinTimestampTz(()),
            VariadicFunc::DateDiffTimestamp => DateDiffTimestamp(()),
            VariadicFunc::DateDiffTimestampTz => DateDiffTimestampTz(()),
            VariadicFunc::DateDiffDate => DateDiffDate(()),
            VariadicFunc::DateDiffTime => DateDiffTime(()),
            VariadicFunc::And => And(()),
            VariadicFunc::Or => Or(()),
            VariadicFunc::RangeCreate { elem_type } => RangeCreate(elem_type.into_proto()),
            VariadicFunc::MakeAclItem => MakeAclItem(()),
            VariadicFunc::MakeMzAclItem => MakeMzAclItem(()),
            VariadicFunc::ArrayPosition => ArrayPosition(()),
            VariadicFunc::ArrayFill { elem_type } => ArrayFill(elem_type.into_proto()),
            VariadicFunc::TimezoneTime => TimezoneTime(()),
            VariadicFunc::RegexpSplitToArray => RegexpSplitToArray(()),
            VariadicFunc::RegexpReplace => RegexpReplace(()),
            VariadicFunc::StringToArray => StringToArray(()),
        };
        ProtoVariadicFunc { kind: Some(kind) }
    }

    fn from_proto(proto: ProtoVariadicFunc) -> Result<Self, TryFromProtoError> {
        use crate::scalar::proto_variadic_func::Kind::*;
        use crate::scalar::proto_variadic_func::ProtoRecordCreate;
        if let Some(kind) = proto.kind {
            match kind {
                Coalesce(()) => Ok(VariadicFunc::Coalesce),
                Greatest(()) => Ok(VariadicFunc::Greatest),
                Least(()) => Ok(VariadicFunc::Least),
                Concat(()) => Ok(VariadicFunc::Concat),
                ConcatWs(()) => Ok(VariadicFunc::ConcatWs),
                MakeTimestamp(()) => Ok(VariadicFunc::MakeTimestamp),
                PadLeading(()) => Ok(VariadicFunc::PadLeading),
                Substr(()) => Ok(VariadicFunc::Substr),
                Replace(()) => Ok(VariadicFunc::Replace),
                Translate(()) => Ok(VariadicFunc::Translate),
                JsonbBuildArray(()) => Ok(VariadicFunc::JsonbBuildArray),
                JsonbBuildObject(()) => Ok(VariadicFunc::JsonbBuildObject),
                MapBuild(value_type) => Ok(VariadicFunc::MapBuild {
                    value_type: value_type.into_rust()?,
                }),
                ArrayCreate(elem_type) => Ok(VariadicFunc::ArrayCreate {
                    elem_type: elem_type.into_rust()?,
                }),
                ArrayToString(elem_type) => Ok(VariadicFunc::ArrayToString {
                    elem_type: elem_type.into_rust()?,
                }),
                ArrayIndex(offset) => Ok(VariadicFunc::ArrayIndex {
                    offset: offset.into_rust()?,
                }),
                ListCreate(elem_type) => Ok(VariadicFunc::ListCreate {
                    elem_type: elem_type.into_rust()?,
                }),
                RecordCreate(ProtoRecordCreate { field_names }) => Ok(VariadicFunc::RecordCreate {
                    field_names: field_names.into_rust()?,
                }),
                ListIndex(()) => Ok(VariadicFunc::ListIndex),
                ListSliceLinear(()) => Ok(VariadicFunc::ListSliceLinear),
                SplitPart(()) => Ok(VariadicFunc::SplitPart),
                RegexpMatch(()) => Ok(VariadicFunc::RegexpMatch),
                HmacString(()) => Ok(VariadicFunc::HmacString),
                HmacBytes(()) => Ok(VariadicFunc::HmacBytes),
                ErrorIfNull(()) => Ok(VariadicFunc::ErrorIfNull),
                DateBinTimestamp(()) => Ok(VariadicFunc::DateBinTimestamp),
                DateBinTimestampTz(()) => Ok(VariadicFunc::DateBinTimestampTz),
                DateDiffTimestamp(()) => Ok(VariadicFunc::DateDiffTimestamp),
                DateDiffTimestampTz(()) => Ok(VariadicFunc::DateDiffTimestampTz),
                DateDiffDate(()) => Ok(VariadicFunc::DateDiffDate),
                DateDiffTime(()) => Ok(VariadicFunc::DateDiffTime),
                And(()) => Ok(VariadicFunc::And),
                Or(()) => Ok(VariadicFunc::Or),
                RangeCreate(elem_type) => Ok(VariadicFunc::RangeCreate {
                    elem_type: elem_type.into_rust()?,
                }),
                MakeAclItem(()) => Ok(VariadicFunc::MakeAclItem),
                MakeMzAclItem(()) => Ok(VariadicFunc::MakeMzAclItem),
                ArrayPosition(()) => Ok(VariadicFunc::ArrayPosition),
                ArrayFill(elem_type) => Ok(VariadicFunc::ArrayFill {
                    elem_type: elem_type.into_rust()?,
                }),
                TimezoneTime(()) => Ok(VariadicFunc::TimezoneTime),
                RegexpSplitToArray(()) => Ok(VariadicFunc::RegexpSplitToArray),
                RegexpReplace(()) => Ok(VariadicFunc::RegexpReplace),
                StringToArray(()) => Ok(VariadicFunc::StringToArray),
            }
        } else {
            Err(TryFromProtoError::missing_field(
                "`ProtoVariadicFunc::kind`",
            ))
        }
    }
}

#[cfg(test)]
mod test {
    use chrono::prelude::*;
    use mz_ore::assert_ok;
    use mz_proto::protobuf_roundtrip;
    use mz_repr::PropDatum;
    use proptest::prelude::*;

    use super::*;

    #[mz_ore::test]
    fn add_interval_months() {
        let dt = ym(2000, 1);

        assert_eq!(add_timestamp_months(&*dt, 0).unwrap(), dt);
        assert_eq!(add_timestamp_months(&*dt, 1).unwrap(), ym(2000, 2));
        assert_eq!(add_timestamp_months(&*dt, 12).unwrap(), ym(2001, 1));
        assert_eq!(add_timestamp_months(&*dt, 13).unwrap(), ym(2001, 2));
        assert_eq!(add_timestamp_months(&*dt, 24).unwrap(), ym(2002, 1));
        assert_eq!(add_timestamp_months(&*dt, 30).unwrap(), ym(2002, 7));

        // and negatives
        assert_eq!(add_timestamp_months(&*dt, -1).unwrap(), ym(1999, 12));
        assert_eq!(add_timestamp_months(&*dt, -12).unwrap(), ym(1999, 1));
        assert_eq!(add_timestamp_months(&*dt, -13).unwrap(), ym(1998, 12));
        assert_eq!(add_timestamp_months(&*dt, -24).unwrap(), ym(1998, 1));
        assert_eq!(add_timestamp_months(&*dt, -30).unwrap(), ym(1997, 7));

        // and going over a year boundary by less than a year
        let dt = ym(1999, 12);
        assert_eq!(add_timestamp_months(&*dt, 1).unwrap(), ym(2000, 1));
        let end_of_month_dt = NaiveDate::from_ymd_opt(1999, 12, 31)
            .unwrap()
            .and_hms_opt(9, 9, 9)
            .unwrap();
        assert_eq!(
            // leap year
            add_timestamp_months(&end_of_month_dt, 2).unwrap(),
            NaiveDate::from_ymd_opt(2000, 2, 29)
                .unwrap()
                .and_hms_opt(9, 9, 9)
                .unwrap()
                .try_into()
                .unwrap(),
        );
        assert_eq!(
            // not leap year
            add_timestamp_months(&end_of_month_dt, 14).unwrap(),
            NaiveDate::from_ymd_opt(2001, 2, 28)
                .unwrap()
                .and_hms_opt(9, 9, 9)
                .unwrap()
                .try_into()
                .unwrap(),
        );
    }

    fn ym(year: i32, month: u32) -> CheckedTimestamp<NaiveDateTime> {
        NaiveDate::from_ymd_opt(year, month, 1)
            .unwrap()
            .and_hms_opt(9, 9, 9)
            .unwrap()
            .try_into()
            .unwrap()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4096))]

        #[mz_ore::test]
        #[cfg_attr(miri, ignore)] // too slow
        fn unmaterializable_func_protobuf_roundtrip(expect in any::<UnmaterializableFunc>()) {
            let actual = protobuf_roundtrip::<_, ProtoUnmaterializableFunc>(&expect);
            assert_ok!(actual);
            assert_eq!(actual.unwrap(), expect);
        }

        #[mz_ore::test]
        #[cfg_attr(miri, ignore)] // too slow
        fn unary_func_protobuf_roundtrip(expect in any::<UnaryFunc>()) {
            let actual = protobuf_roundtrip::<_, ProtoUnaryFunc>(&expect);
            assert_ok!(actual);
            assert_eq!(actual.unwrap(), expect);
        }

        #[mz_ore::test]
        #[cfg_attr(miri, ignore)] // too slow
        fn binary_func_protobuf_roundtrip(expect in any::<BinaryFunc>()) {
            let actual = protobuf_roundtrip::<_, ProtoBinaryFunc>(&expect);
            assert_ok!(actual);
            assert_eq!(actual.unwrap(), expect);
        }

        #[mz_ore::test]
        #[cfg_attr(miri, ignore)] // too slow
        fn variadic_func_protobuf_roundtrip(expect in any::<VariadicFunc>()) {
            let actual = protobuf_roundtrip::<_, ProtoVariadicFunc>(&expect);
            assert_ok!(actual);
            assert_eq!(actual.unwrap(), expect);
        }
    }

    #[mz_ore::test]
    fn test_could_error() {
        for func in [
            UnaryFunc::IsNull(IsNull),
            UnaryFunc::CastVarCharToString(CastVarCharToString),
            UnaryFunc::Not(Not),
            UnaryFunc::IsLikeMatch(IsLikeMatch(like_pattern::compile("%hi%", false).unwrap())),
        ] {
            assert!(!func.could_error())
        }
    }

    #[mz_ore::test]
    #[cfg_attr(miri, ignore)] // unsupported operation: can't call foreign function `decNumberFromInt32` on OS `linux`
    fn test_is_monotone() {
        use proptest::prelude::*;

        /// Asserts that the function is either monotonically increasing or decreasing over
        /// the given sets of arguments.
        fn assert_monotone<'a, const N: usize>(
            expr: &MirScalarExpr,
            arena: &'a RowArena,
            datums: &[[Datum<'a>; N]],
        ) {
            // TODO: assertions for nulls, errors
            let Ok(results) = datums
                .iter()
                .map(|args| expr.eval(args.as_slice(), arena))
                .collect::<Result<Vec<_>, _>>()
            else {
                return;
            };

            let forward = results.iter().tuple_windows().all(|(a, b)| a <= b);
            let reverse = results.iter().tuple_windows().all(|(a, b)| a >= b);
            assert!(
                forward || reverse,
                "expected {expr} to be monotone, but passing {datums:?} returned {results:?}"
            );
        }

        fn proptest_unary<'a>(
            func: UnaryFunc,
            arena: &'a RowArena,
            arg: impl Strategy<Value = PropDatum>,
        ) {
            let is_monotone = func.is_monotone();
            let expr = MirScalarExpr::CallUnary {
                func,
                expr: Box::new(MirScalarExpr::column(0)),
            };
            if is_monotone {
                proptest!(|(
                    mut arg in proptest::array::uniform3(arg),
                )| {
                    arg.sort();
                    let args: Vec<_> = arg.iter().map(|a| [Datum::from(a)]).collect();
                    assert_monotone(&expr, arena, &args);
                });
            }
        }

        fn proptest_binary<'a>(
            func: BinaryFunc,
            arena: &'a RowArena,
            left: impl Strategy<Value = PropDatum>,
            right: impl Strategy<Value = PropDatum>,
        ) {
            let (left_monotone, right_monotone) = func.is_monotone();
            let expr = MirScalarExpr::CallBinary {
                func,
                expr1: Box::new(MirScalarExpr::column(0)),
                expr2: Box::new(MirScalarExpr::column(1)),
            };
            proptest!(|(
                mut left in proptest::array::uniform3(left),
                mut right in proptest::array::uniform3(right),
            )| {
                left.sort();
                right.sort();
                if left_monotone {
                    for r in &right {
                        let args: Vec<[_; 2]> = left
                            .iter()
                            .map(|l| [Datum::from(l), Datum::from(r)])
                            .collect();
                        assert_monotone(&expr, arena, &args);
                    }
                }
                if right_monotone {
                    for l in &left {
                        let args: Vec<[_; 2]> = right
                            .iter()
                            .map(|r| [Datum::from(l), Datum::from(r)])
                            .collect();
                        assert_monotone(&expr, arena, &args);
                    }
                }
            });
        }

        let interesting_strs: Vec<_> = ScalarType::String.interesting_datums().collect();
        let str_datums = proptest::strategy::Union::new([
            proptest::string::string_regex("[A-Z]{0,10}")
                .expect("valid regex")
                .prop_map(|s| PropDatum::String(s.to_string()))
                .boxed(),
            (0..interesting_strs.len())
                .prop_map(move |i| {
                    let Datum::String(val) = interesting_strs[i] else {
                        unreachable!("interesting strings has non-strings")
                    };
                    PropDatum::String(val.to_string())
                })
                .boxed(),
        ]);

        let interesting_i32s: Vec<Datum<'static>> =
            ScalarType::Int32.interesting_datums().collect();
        let i32_datums = proptest::strategy::Union::new([
            any::<i32>().prop_map(PropDatum::Int32).boxed(),
            (0..interesting_i32s.len())
                .prop_map(move |i| {
                    let Datum::Int32(val) = interesting_i32s[i] else {
                        unreachable!("interesting int32 has non-i32s")
                    };
                    PropDatum::Int32(val)
                })
                .boxed(),
            (-10i32..10).prop_map(PropDatum::Int32).boxed(),
        ]);

        let arena = RowArena::new();

        // It would be interesting to test all funcs here, but we currently need to hardcode
        // the generators for the argument types, which makes this tedious. Choose an interesting
        // subset for now.
        proptest_unary(
            UnaryFunc::CastInt32ToNumeric(CastInt32ToNumeric(None)),
            &arena,
            &i32_datums,
        );
        proptest_unary(
            UnaryFunc::CastInt32ToUint16(CastInt32ToUint16),
            &arena,
            &i32_datums,
        );
        proptest_unary(
            UnaryFunc::CastInt32ToString(CastInt32ToString),
            &arena,
            &i32_datums,
        );
        proptest_binary(BinaryFunc::AddInt32, &arena, &i32_datums, &i32_datums);
        proptest_binary(BinaryFunc::SubInt32, &arena, &i32_datums, &i32_datums);
        proptest_binary(BinaryFunc::MulInt32, &arena, &i32_datums, &i32_datums);
        proptest_binary(BinaryFunc::DivInt32, &arena, &i32_datums, &i32_datums);
        proptest_binary(BinaryFunc::TextConcat, &arena, &str_datums, &str_datums);
        proptest_binary(BinaryFunc::Left, &arena, &str_datums, &i32_datums);
    }
}
