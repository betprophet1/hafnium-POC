#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum MarketState {
    LISTED = 0x0_u8,
    OPEN = 0x1_u8,
    HALTED = 0x2_u8,
    CLOSED = 0x3_u8,
    RESOLVING = 0x4_u8,
    SETTLED = 0x5_u8,
    VOIDED = 0x6_u8,
    #[default]
    NullVal = 0xff_u8,
}
impl From<u8> for MarketState {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::LISTED,
            0x1_u8 => Self::OPEN,
            0x2_u8 => Self::HALTED,
            0x3_u8 => Self::CLOSED,
            0x4_u8 => Self::RESOLVING,
            0x5_u8 => Self::SETTLED,
            0x6_u8 => Self::VOIDED,
            _ => Self::NullVal,
        }
    }
}
impl From<MarketState> for u8 {
    #[inline]
    fn from(v: MarketState) -> Self {
        match v {
            MarketState::LISTED => 0x0_u8,
            MarketState::OPEN => 0x1_u8,
            MarketState::HALTED => 0x2_u8,
            MarketState::CLOSED => 0x3_u8,
            MarketState::RESOLVING => 0x4_u8,
            MarketState::SETTLED => 0x5_u8,
            MarketState::VOIDED => 0x6_u8,
            MarketState::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for MarketState {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "LISTED" => Ok(Self::LISTED),
            "OPEN" => Ok(Self::OPEN),
            "HALTED" => Ok(Self::HALTED),
            "CLOSED" => Ok(Self::CLOSED),
            "RESOLVING" => Ok(Self::RESOLVING),
            "SETTLED" => Ok(Self::SETTLED),
            "VOIDED" => Ok(Self::VOIDED),
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for MarketState {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::LISTED => write!(f, "LISTED"),
            Self::OPEN => write!(f, "OPEN"),
            Self::HALTED => write!(f, "HALTED"),
            Self::CLOSED => write!(f, "CLOSED"),
            Self::RESOLVING => write!(f, "RESOLVING"),
            Self::SETTLED => write!(f, "SETTLED"),
            Self::VOIDED => write!(f, "VOIDED"),
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
