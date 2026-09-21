#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum OutcomeValue {
    NO = 0x0_u8, 
    YES = 0x1_u8, 
    #[default]
    NullVal = 0xff_u8, 
}
impl From<u8> for OutcomeValue {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::NO, 
            0x1_u8 => Self::YES, 
            _ => Self::NullVal,
        }
    }
}
impl From<OutcomeValue> for u8 {
    #[inline]
    fn from(v: OutcomeValue) -> Self {
        match v {
            OutcomeValue::NO => 0x0_u8, 
            OutcomeValue::YES => 0x1_u8, 
            OutcomeValue::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for OutcomeValue {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "NO" => Ok(Self::NO), 
            "YES" => Ok(Self::YES), 
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for OutcomeValue {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NO => write!(f, "NO"), 
            Self::YES => write!(f, "YES"), 
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
