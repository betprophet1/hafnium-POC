#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum BooleanType {
    FALSE = 0x0_u8, 
    TRUE = 0x1_u8, 
    #[default]
    NullVal = 0xff_u8, 
}
impl From<u8> for BooleanType {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::FALSE, 
            0x1_u8 => Self::TRUE, 
            _ => Self::NullVal,
        }
    }
}
impl From<BooleanType> for u8 {
    #[inline]
    fn from(v: BooleanType) -> Self {
        match v {
            BooleanType::FALSE => 0x0_u8, 
            BooleanType::TRUE => 0x1_u8, 
            BooleanType::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for BooleanType {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "FALSE" => Ok(Self::FALSE), 
            "TRUE" => Ok(Self::TRUE), 
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for BooleanType {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::FALSE => write!(f, "FALSE"), 
            Self::TRUE => write!(f, "TRUE"), 
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
