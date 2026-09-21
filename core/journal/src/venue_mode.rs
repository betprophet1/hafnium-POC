#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum VenueMode {
    NORMAL = 0x0_u8, 
    NEW_ORDER_REJECT = 0x1_u8, 
    CANCEL_ONLY = 0x2_u8, 
    KILL_SWITCH = 0x3_u8, 
    #[default]
    NullVal = 0xff_u8, 
}
impl From<u8> for VenueMode {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::NORMAL, 
            0x1_u8 => Self::NEW_ORDER_REJECT, 
            0x2_u8 => Self::CANCEL_ONLY, 
            0x3_u8 => Self::KILL_SWITCH, 
            _ => Self::NullVal,
        }
    }
}
impl From<VenueMode> for u8 {
    #[inline]
    fn from(v: VenueMode) -> Self {
        match v {
            VenueMode::NORMAL => 0x0_u8, 
            VenueMode::NEW_ORDER_REJECT => 0x1_u8, 
            VenueMode::CANCEL_ONLY => 0x2_u8, 
            VenueMode::KILL_SWITCH => 0x3_u8, 
            VenueMode::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for VenueMode {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "NORMAL" => Ok(Self::NORMAL), 
            "NEW_ORDER_REJECT" => Ok(Self::NEW_ORDER_REJECT), 
            "CANCEL_ONLY" => Ok(Self::CANCEL_ONLY), 
            "KILL_SWITCH" => Ok(Self::KILL_SWITCH), 
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for VenueMode {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NORMAL => write!(f, "NORMAL"), 
            Self::NEW_ORDER_REJECT => write!(f, "NEW_ORDER_REJECT"), 
            Self::CANCEL_ONLY => write!(f, "CANCEL_ONLY"), 
            Self::KILL_SWITCH => write!(f, "KILL_SWITCH"), 
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
