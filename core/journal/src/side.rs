/// Canonical side, always in YES
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Side {
    BUY = 0x0_u8,
    SELL = 0x1_u8,
    #[default]
    NullVal = 0xff_u8,
}
impl From<u8> for Side {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::BUY,
            0x1_u8 => Self::SELL,
            _ => Self::NullVal,
        }
    }
}
impl From<Side> for u8 {
    #[inline]
    fn from(v: Side) -> Self {
        match v {
            Side::BUY => 0x0_u8,
            Side::SELL => 0x1_u8,
            Side::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for Side {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "BUY" => Ok(Self::BUY),
            "SELL" => Ok(Self::SELL),
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for Side {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BUY => write!(f, "BUY"),
            Self::SELL => write!(f, "SELL"),
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
