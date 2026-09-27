/// Automatic burns across the books of a partition event
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum ReleaseKind {
    /// One YES of every book pays $1.00 in every state
    YES_SET_BURN = 0x0_u8,
    /// One NO of every book pays $1.00 times (books - 1) in every state
    NO_SET_BURN = 0x1_u8,
    #[default]
    NullVal = 0xff_u8,
}
impl From<u8> for ReleaseKind {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::YES_SET_BURN,
            0x1_u8 => Self::NO_SET_BURN,
            _ => Self::NullVal,
        }
    }
}
impl From<ReleaseKind> for u8 {
    #[inline]
    fn from(v: ReleaseKind) -> Self {
        match v {
            ReleaseKind::YES_SET_BURN => 0x0_u8,
            ReleaseKind::NO_SET_BURN => 0x1_u8,
            ReleaseKind::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for ReleaseKind {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "YES_SET_BURN" => Ok(Self::YES_SET_BURN),
            "NO_SET_BURN" => Ok(Self::NO_SET_BURN),
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for ReleaseKind {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::YES_SET_BURN => write!(f, "YES_SET_BURN"),
            Self::NO_SET_BURN => write!(f, "NO_SET_BURN"),
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
