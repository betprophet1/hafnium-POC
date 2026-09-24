#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum SeedPurpose {
    CLIENT_ORDER_ID_HASH = 0x0_u8,
    PRNG = 0x1_u8,
    #[default]
    NullVal = 0xff_u8,
}
impl From<u8> for SeedPurpose {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::CLIENT_ORDER_ID_HASH,
            0x1_u8 => Self::PRNG,
            _ => Self::NullVal,
        }
    }
}
impl From<SeedPurpose> for u8 {
    #[inline]
    fn from(v: SeedPurpose) -> Self {
        match v {
            SeedPurpose::CLIENT_ORDER_ID_HASH => 0x0_u8,
            SeedPurpose::PRNG => 0x1_u8,
            SeedPurpose::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for SeedPurpose {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "CLIENT_ORDER_ID_HASH" => Ok(Self::CLIENT_ORDER_ID_HASH),
            "PRNG" => Ok(Self::PRNG),
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for SeedPurpose {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::CLIENT_ORDER_ID_HASH => write!(f, "CLIENT_ORDER_ID_HASH"),
            Self::PRNG => write!(f, "PRNG"),
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
