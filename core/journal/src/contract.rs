/// What the customer entered, before reflection to YES
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Contract {
    YES = 0x0_u8, 
    NO = 0x1_u8, 
    #[default]
    NullVal = 0xff_u8, 
}
impl From<u8> for Contract {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::YES, 
            0x1_u8 => Self::NO, 
            _ => Self::NullVal,
        }
    }
}
impl From<Contract> for u8 {
    #[inline]
    fn from(v: Contract) -> Self {
        match v {
            Contract::YES => 0x0_u8, 
            Contract::NO => 0x1_u8, 
            Contract::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for Contract {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "YES" => Ok(Self::YES), 
            "NO" => Ok(Self::NO), 
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for Contract {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::YES => write!(f, "YES"), 
            Self::NO => write!(f, "NO"), 
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
