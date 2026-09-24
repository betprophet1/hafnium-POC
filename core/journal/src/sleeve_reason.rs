#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum SleeveReason {
    GRANT = 0x0_u8,
    RECLAIM = 0x1_u8,
    #[default]
    NullVal = 0xff_u8,
}
impl From<u8> for SleeveReason {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::GRANT,
            0x1_u8 => Self::RECLAIM,
            _ => Self::NullVal,
        }
    }
}
impl From<SleeveReason> for u8 {
    #[inline]
    fn from(v: SleeveReason) -> Self {
        match v {
            SleeveReason::GRANT => 0x0_u8,
            SleeveReason::RECLAIM => 0x1_u8,
            SleeveReason::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for SleeveReason {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "GRANT" => Ok(Self::GRANT),
            "RECLAIM" => Ok(Self::RECLAIM),
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for SleeveReason {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::GRANT => write!(f, "GRANT"),
            Self::RECLAIM => write!(f, "RECLAIM"),
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
