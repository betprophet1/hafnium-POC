#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum SelfMatchPrevention {
    CANCEL_RESTING = 0x0_u8, 
    CANCEL_INCOMING = 0x1_u8, 
    CANCEL_BOTH = 0x2_u8, 
    #[default]
    NullVal = 0xff_u8, 
}
impl From<u8> for SelfMatchPrevention {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::CANCEL_RESTING, 
            0x1_u8 => Self::CANCEL_INCOMING, 
            0x2_u8 => Self::CANCEL_BOTH, 
            _ => Self::NullVal,
        }
    }
}
impl From<SelfMatchPrevention> for u8 {
    #[inline]
    fn from(v: SelfMatchPrevention) -> Self {
        match v {
            SelfMatchPrevention::CANCEL_RESTING => 0x0_u8, 
            SelfMatchPrevention::CANCEL_INCOMING => 0x1_u8, 
            SelfMatchPrevention::CANCEL_BOTH => 0x2_u8, 
            SelfMatchPrevention::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for SelfMatchPrevention {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "CANCEL_RESTING" => Ok(Self::CANCEL_RESTING), 
            "CANCEL_INCOMING" => Ok(Self::CANCEL_INCOMING), 
            "CANCEL_BOTH" => Ok(Self::CANCEL_BOTH), 
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for SelfMatchPrevention {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::CANCEL_RESTING => write!(f, "CANCEL_RESTING"), 
            Self::CANCEL_INCOMING => write!(f, "CANCEL_INCOMING"), 
            Self::CANCEL_BOTH => write!(f, "CANCEL_BOTH"), 
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
