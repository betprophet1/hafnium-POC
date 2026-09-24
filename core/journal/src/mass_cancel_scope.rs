#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum MassCancelScope {
    SESSION = 0x0_u8, 
    ACCOUNT = 0x1_u8, 
    INSTRUMENT = 0x2_u8, 
    EVENT = 0x3_u8, 
    CLIENT_ORDER_GROUP = 0x4_u8, 
    #[default]
    NullVal = 0xff_u8, 
}
impl From<u8> for MassCancelScope {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::SESSION, 
            0x1_u8 => Self::ACCOUNT, 
            0x2_u8 => Self::INSTRUMENT, 
            0x3_u8 => Self::EVENT, 
            0x4_u8 => Self::CLIENT_ORDER_GROUP, 
            _ => Self::NullVal,
        }
    }
}
impl From<MassCancelScope> for u8 {
    #[inline]
    fn from(v: MassCancelScope) -> Self {
        match v {
            MassCancelScope::SESSION => 0x0_u8, 
            MassCancelScope::ACCOUNT => 0x1_u8, 
            MassCancelScope::INSTRUMENT => 0x2_u8, 
            MassCancelScope::EVENT => 0x3_u8, 
            MassCancelScope::CLIENT_ORDER_GROUP => 0x4_u8, 
            MassCancelScope::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for MassCancelScope {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "SESSION" => Ok(Self::SESSION), 
            "ACCOUNT" => Ok(Self::ACCOUNT), 
            "INSTRUMENT" => Ok(Self::INSTRUMENT), 
            "EVENT" => Ok(Self::EVENT), 
            "CLIENT_ORDER_GROUP" => Ok(Self::CLIENT_ORDER_GROUP), 
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for MassCancelScope {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::SESSION => write!(f, "SESSION"), 
            Self::ACCOUNT => write!(f, "ACCOUNT"), 
            Self::INSTRUMENT => write!(f, "INSTRUMENT"), 
            Self::EVENT => write!(f, "EVENT"), 
            Self::CLIENT_ORDER_GROUP => write!(f, "CLIENT_ORDER_GROUP"), 
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
