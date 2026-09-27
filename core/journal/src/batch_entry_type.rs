#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum BatchEntryType {
    NEW = 0x0_u8,
    CANCEL = 0x1_u8,
    #[default]
    NullVal = 0xff_u8,
}
impl From<u8> for BatchEntryType {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::NEW,
            0x1_u8 => Self::CANCEL,
            _ => Self::NullVal,
        }
    }
}
impl From<BatchEntryType> for u8 {
    #[inline]
    fn from(v: BatchEntryType) -> Self {
        match v {
            BatchEntryType::NEW => 0x0_u8,
            BatchEntryType::CANCEL => 0x1_u8,
            BatchEntryType::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for BatchEntryType {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "NEW" => Ok(Self::NEW),
            "CANCEL" => Ok(Self::CANCEL),
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for BatchEntryType {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NEW => write!(f, "NEW"),
            Self::CANCEL => write!(f, "CANCEL"),
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
