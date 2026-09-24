#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum AllocationPolicy {
    PRICE_TIME = 0x0_u8, 
    PRO_RATA = 0x1_u8, 
    #[default]
    NullVal = 0xff_u8, 
}
impl From<u8> for AllocationPolicy {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::PRICE_TIME, 
            0x1_u8 => Self::PRO_RATA, 
            _ => Self::NullVal,
        }
    }
}
impl From<AllocationPolicy> for u8 {
    #[inline]
    fn from(v: AllocationPolicy) -> Self {
        match v {
            AllocationPolicy::PRICE_TIME => 0x0_u8, 
            AllocationPolicy::PRO_RATA => 0x1_u8, 
            AllocationPolicy::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for AllocationPolicy {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "PRICE_TIME" => Ok(Self::PRICE_TIME), 
            "PRO_RATA" => Ok(Self::PRO_RATA), 
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for AllocationPolicy {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::PRICE_TIME => write!(f, "PRICE_TIME"), 
            Self::PRO_RATA => write!(f, "PRO_RATA"), 
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
