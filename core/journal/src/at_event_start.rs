#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum AtEventStart {
    CANCEL = 0x0_u8, 
    KEEP = 0x1_u8, 
    /// AmendOrder only
    UNCHANGED = 0x2_u8, 
    #[default]
    NullVal = 0xff_u8, 
}
impl From<u8> for AtEventStart {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::CANCEL, 
            0x1_u8 => Self::KEEP, 
            0x2_u8 => Self::UNCHANGED, 
            _ => Self::NullVal,
        }
    }
}
impl From<AtEventStart> for u8 {
    #[inline]
    fn from(v: AtEventStart) -> Self {
        match v {
            AtEventStart::CANCEL => 0x0_u8, 
            AtEventStart::KEEP => 0x1_u8, 
            AtEventStart::UNCHANGED => 0x2_u8, 
            AtEventStart::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for AtEventStart {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "CANCEL" => Ok(Self::CANCEL), 
            "KEEP" => Ok(Self::KEEP), 
            "UNCHANGED" => Ok(Self::UNCHANGED), 
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for AtEventStart {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::CANCEL => write!(f, "CANCEL"), 
            Self::KEEP => write!(f, "KEEP"), 
            Self::UNCHANGED => write!(f, "UNCHANGED"), 
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
