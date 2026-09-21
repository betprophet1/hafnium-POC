#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum MassCancelCause {
    CLIENT_REQUEST = 0x0_u8, 
    SESSION_DISCONNECT = 0x1_u8, 
    HEARTBEAT_MISSED = 0x2_u8, 
    OPERATOR_KILL_SWITCH = 0x3_u8, 
    #[default]
    NullVal = 0xff_u8, 
}
impl From<u8> for MassCancelCause {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::CLIENT_REQUEST, 
            0x1_u8 => Self::SESSION_DISCONNECT, 
            0x2_u8 => Self::HEARTBEAT_MISSED, 
            0x3_u8 => Self::OPERATOR_KILL_SWITCH, 
            _ => Self::NullVal,
        }
    }
}
impl From<MassCancelCause> for u8 {
    #[inline]
    fn from(v: MassCancelCause) -> Self {
        match v {
            MassCancelCause::CLIENT_REQUEST => 0x0_u8, 
            MassCancelCause::SESSION_DISCONNECT => 0x1_u8, 
            MassCancelCause::HEARTBEAT_MISSED => 0x2_u8, 
            MassCancelCause::OPERATOR_KILL_SWITCH => 0x3_u8, 
            MassCancelCause::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for MassCancelCause {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "CLIENT_REQUEST" => Ok(Self::CLIENT_REQUEST), 
            "SESSION_DISCONNECT" => Ok(Self::SESSION_DISCONNECT), 
            "HEARTBEAT_MISSED" => Ok(Self::HEARTBEAT_MISSED), 
            "OPERATOR_KILL_SWITCH" => Ok(Self::OPERATOR_KILL_SWITCH), 
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for MassCancelCause {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::CLIENT_REQUEST => write!(f, "CLIENT_REQUEST"), 
            Self::SESSION_DISCONNECT => write!(f, "SESSION_DISCONNECT"), 
            Self::HEARTBEAT_MISSED => write!(f, "HEARTBEAT_MISSED"), 
            Self::OPERATOR_KILL_SWITCH => write!(f, "OPERATOR_KILL_SWITCH"), 
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
