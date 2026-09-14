#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum BasketDirection {
    MINT = 0x0_u8, 
    REDEEM = 0x1_u8, 
    #[default]
    NullVal = 0xff_u8, 
}
impl From<u8> for BasketDirection {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::MINT, 
            0x1_u8 => Self::REDEEM, 
            _ => Self::NullVal,
        }
    }
}
impl From<BasketDirection> for u8 {
    #[inline]
    fn from(v: BasketDirection) -> Self {
        match v {
            BasketDirection::MINT => 0x0_u8, 
            BasketDirection::REDEEM => 0x1_u8, 
            BasketDirection::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for BasketDirection {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "MINT" => Ok(Self::MINT), 
            "REDEEM" => Ok(Self::REDEEM), 
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for BasketDirection {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MINT => write!(f, "MINT"), 
            Self::REDEEM => write!(f, "REDEEM"), 
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
