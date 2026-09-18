//! Go's `time.Time`, which crosses gob as a `GobEncoder` (time.go, `GobEncode` → `MarshalBinary`).

use crate::decode::{Decode, ValueDecoder};
use crate::encode::{Encode, GobType, ValueEncoder};
use crate::error::{Error, Result};
use crate::types::{Describer, MarshalKind, TypeTable, WireType};

/// Seconds from 0001-01-01 to 1970-01-01 (time.go, `unixToInternal`).
pub const UNIX_TO_INTERNAL: i64 = (1969 * 365 + 1969 / 4 - 1969 / 100 + 1969 / 400) * 86_400;

/// Where a [`GoTime`] is.
///
/// Go's binary form keeps only an offset: a zone *name* and DST rules do not survive, and a
/// decoded time whose offset equals the **decoding process's** local offset becomes
/// `time.Local` there. Neither side of that is observable from Rust, so this keeps the offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Zone {
    /// `time.UTC` (and the zero `Time`'s nil location).
    Utc,
    /// Any other location, as seconds east of UTC.
    Offset(i32),
}

/// A Go `time.Time`: seconds since 0001-01-01 UTC, nanoseconds, and a zone.
///
/// The zero value is Go's zero `Time`, which [`Encode::is_zero`] omits from a struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GoTime {
    /// Seconds since January 1, year 1 00:00:00 UTC.
    pub sec: i64,
    /// Nanoseconds within the second, `0..1_000_000_000`.
    pub nsec: i32,
    pub zone: Zone,
}

impl Default for GoTime {
    fn default() -> Self {
        Self {
            sec: 0,
            nsec: 0,
            zone: Zone::Utc,
        }
    }
}

impl GoTime {
    /// From Unix seconds and nanoseconds.
    pub fn from_unix(secs: i64, nsec: i32, zone: Zone) -> Self {
        Self {
            sec: secs + UNIX_TO_INTERNAL,
            nsec,
            zone,
        }
    }

    /// Unix seconds.
    pub fn unix(&self) -> i64 {
        self.sec - UNIX_TO_INTERNAL
    }

    /// Unix milliseconds, as Mattermost stores timestamps.
    pub fn unix_millis(&self) -> i64 {
        self.unix() * 1000 + i64::from(self.nsec) / 1_000_000
    }

    /// time.go, `AppendBinary`.
    pub fn marshal_binary(&self) -> Result<Vec<u8>> {
        let mut version = 1u8;
        let mut offset_sec: i8 = 0;
        let offset_min: i16 = match self.zone {
            Zone::Utc => -1,
            Zone::Offset(offset) => {
                if offset % 60 != 0 {
                    version = 2;
                    offset_sec = (offset % 60) as i8;
                }
                let minutes = offset / 60;
                if !(-32768..=32767).contains(&minutes) || minutes == -1 {
                    return Err(Error::Marshal(
                        "Time.MarshalBinary: unexpected zone offset".into(),
                    ));
                }
                minutes as i16
            }
        };
        let mut b = Vec::with_capacity(16);
        b.push(version);
        b.extend_from_slice(&self.sec.to_be_bytes());
        b.extend_from_slice(&self.nsec.to_be_bytes());
        b.extend_from_slice(&offset_min.to_be_bytes());
        if version == 2 {
            b.push(offset_sec as u8);
        }
        Ok(b)
    }

    /// time.go, `UnmarshalBinary`.
    ///
    /// Go quirk kept: the V2 seconds byte is added back **unsigned** (`int(buf[2])`), although
    /// `AppendBinary` wrote it as a signed `offset % 60`. A negative offset with a seconds part,
    /// such as -01:00:01, does not round-trip in Go either.
    pub fn unmarshal_binary(data: &[u8]) -> Result<Self> {
        let err = |m: &str| Error::Marshal(format!("Time.UnmarshalBinary: {m}"));
        let version = *data.first().ok_or_else(|| err("no data"))?;
        if version != 1 && version != 2 {
            return Err(err("unsupported version"));
        }
        let want = if version == 2 { 16 } else { 15 };
        if data.len() != want {
            return Err(err("invalid length"));
        }
        let sec = i64::from_be_bytes(data[1..9].try_into().map_err(|_| err("invalid length"))?);
        let nsec = i32::from_be_bytes(data[9..13].try_into().map_err(|_| err("invalid length"))?);
        let mut offset = i32::from(i16::from_be_bytes([data[13], data[14]])) * 60;
        if version == 2 {
            offset += i32::from(data[15]);
        }
        let zone = if offset == -60 {
            Zone::Utc
        } else {
            Zone::Offset(offset)
        };
        Ok(Self { sec, nsec, zone })
    }
}

impl GobType for GoTime {
    fn describe(d: &mut Describer<'_>) -> Result<i64> {
        Ok(d.marshaler("time.Time", "Time", MarshalKind::Gob))
    }

    fn compatible(types: &TypeTable, wire: i64) -> bool {
        matches!(
            types.get(wire).map(|t| &**t),
            Some(WireType::Marshaler {
                kind: MarshalKind::Gob,
                ..
            })
        )
    }
}

impl Encode for GoTime {
    fn describe_value(&self, d: &mut Describer<'_>) -> Result<i64> {
        Self::describe(d)
    }

    /// reflect's `IsZero` on `time.Time`: no wall clock, no extended seconds, nil location.
    fn is_zero(&self) -> bool {
        self.sec == 0 && self.nsec == 0 && self.zone == Zone::Utc
    }

    fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()> {
        e.bytes(&self.marshal_binary()?);
        Ok(())
    }
}

impl Decode for GoTime {
    fn decode_into(&mut self, d: &mut ValueDecoder<'_>, wire: i64) -> Result<()> {
        if !Self::compatible(d.types(), wire) {
            return Err(d.mismatch::<Self>(wire));
        }
        *self = Self::unmarshal_binary(d.read_bytes()?)?;
        Ok(())
    }
}

#[cfg(feature = "chrono")]
mod chrono_impls {
    use chrono::{DateTime, FixedOffset, TimeZone, Utc};

    use super::{GoTime, UNIX_TO_INTERNAL, Zone};
    use crate::error::Error;

    impl TryFrom<GoTime> for DateTime<FixedOffset> {
        type Error = Error;

        fn try_from(t: GoTime) -> Result<Self, Error> {
            let offset = match t.zone {
                Zone::Utc => 0,
                Zone::Offset(s) => s,
            };
            let tz = FixedOffset::east_opt(offset)
                .ok_or_else(|| Error::Marshal(format!("offset {offset} out of range")))?;
            let secs = t.sec - UNIX_TO_INTERNAL;
            tz.timestamp_opt(secs, t.nsec as u32)
                .single()
                .ok_or_else(|| Error::Marshal("time out of chrono's range".into()))
        }
    }

    impl From<DateTime<Utc>> for GoTime {
        fn from(t: DateTime<Utc>) -> Self {
            GoTime::from_unix(t.timestamp(), t.timestamp_subsec_nanos() as i32, Zone::Utc)
        }
    }

    impl From<DateTime<FixedOffset>> for GoTime {
        fn from(t: DateTime<FixedOffset>) -> Self {
            GoTime::from_unix(
                t.timestamp(),
                t.timestamp_subsec_nanos() as i32,
                Zone::Offset(t.offset().local_minus_utc()),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_zero_and_unix_epoch() {
        let zero = GoTime::default();
        let b = zero.marshal_binary().unwrap();
        assert_eq!(b, [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff]);
        assert_eq!(GoTime::unmarshal_binary(&b).unwrap(), zero);
        let epoch = GoTime::from_unix(0, 0, Zone::Utc);
        assert_eq!(epoch.sec, 62_135_596_800);
    }

    #[test]
    fn offsets() {
        let ist = GoTime::from_unix(1, 5, Zone::Offset(19_800));
        let b = ist.marshal_binary().unwrap();
        assert_eq!(b.len(), 15);
        assert_eq!(&b[13..], &330i16.to_be_bytes());
        assert_eq!(GoTime::unmarshal_binary(&b).unwrap(), ist);

        // Offset -1 minute is the UTC sentinel, so Go refuses to marshal it.
        assert!(
            GoTime::from_unix(0, 0, Zone::Offset(-60))
                .marshal_binary()
                .is_err()
        );
        // A zero offset that is not UTC round-trips as an offset.
        let fixed0 = GoTime::from_unix(0, 0, Zone::Offset(0));
        assert_eq!(
            GoTime::unmarshal_binary(&fixed0.marshal_binary().unwrap()).unwrap(),
            fixed0
        );
    }

    #[test]
    fn lmt_seconds_use_version_two() {
        let lmt = GoTime::from_unix(0, 0, Zone::Offset(3_601));
        let b = lmt.marshal_binary().unwrap();
        assert_eq!((b[0], b.len(), b[15]), (2, 16, 1));
        assert_eq!(GoTime::unmarshal_binary(&b).unwrap(), lmt);
        // Negative with seconds: written signed, read unsigned — Go's own asymmetry.
        let neg = GoTime::from_unix(0, 0, Zone::Offset(-3_601));
        let b = neg.marshal_binary().unwrap();
        assert_eq!(b[15], 0xff);
        assert_eq!(
            GoTime::unmarshal_binary(&b).unwrap().zone,
            Zone::Offset(-3_600 + 255)
        );
    }

    #[test]
    fn rejects_bad_input() {
        assert!(GoTime::unmarshal_binary(&[]).is_err());
        assert!(GoTime::unmarshal_binary(&[3; 15]).is_err());
        assert!(GoTime::unmarshal_binary(&[1; 16]).is_err());
        assert!(GoTime::unmarshal_binary(&[2; 15]).is_err());
    }
}
