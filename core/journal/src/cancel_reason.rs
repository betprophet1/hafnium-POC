#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum CancelReason {
    CLIENT_REQUEST = 0x0_u8,
    MASS_CANCEL = 0x1_u8,
    SESSION_DISCONNECT = 0x2_u8,
    HEARTBEAT_MISSED = 0x3_u8,
    EVENT_START = 0x4_u8,
    EXPIRED = 0x5_u8,
    IOC_REMAINDER = 0x6_u8,
    SELF_MATCH_PREVENTED = 0x7_u8,
    MAKER_AUTO_PULL = 0x8_u8,
    KILL_SWITCH = 0x9_u8,
    MARKET_RESOLVED = 0xa_u8,
    /// A reduce-only order had nothing left to reduce
    POSITION_CLOSED = 0xb_u8,
    MARKET_CLOSED = 0xc_u8,
    ORDER_GROUP_TRIGGERED = 0xd_u8,
    /// A spend-capped order cannot afford another lot
    SPEND_CAP_REACHED = 0xe_u8,
    #[default]
    NullVal = 0xff_u8,
}
impl From<u8> for CancelReason {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::CLIENT_REQUEST,
            0x1_u8 => Self::MASS_CANCEL,
            0x2_u8 => Self::SESSION_DISCONNECT,
            0x3_u8 => Self::HEARTBEAT_MISSED,
            0x4_u8 => Self::EVENT_START,
            0x5_u8 => Self::EXPIRED,
            0x6_u8 => Self::IOC_REMAINDER,
            0x7_u8 => Self::SELF_MATCH_PREVENTED,
            0x8_u8 => Self::MAKER_AUTO_PULL,
            0x9_u8 => Self::KILL_SWITCH,
            0xa_u8 => Self::MARKET_RESOLVED,
            0xb_u8 => Self::POSITION_CLOSED,
            0xc_u8 => Self::MARKET_CLOSED,
            0xd_u8 => Self::ORDER_GROUP_TRIGGERED,
            0xe_u8 => Self::SPEND_CAP_REACHED,
            _ => Self::NullVal,
        }
    }
}
impl From<CancelReason> for u8 {
    #[inline]
    fn from(v: CancelReason) -> Self {
        match v {
            CancelReason::CLIENT_REQUEST => 0x0_u8,
            CancelReason::MASS_CANCEL => 0x1_u8,
            CancelReason::SESSION_DISCONNECT => 0x2_u8,
            CancelReason::HEARTBEAT_MISSED => 0x3_u8,
            CancelReason::EVENT_START => 0x4_u8,
            CancelReason::EXPIRED => 0x5_u8,
            CancelReason::IOC_REMAINDER => 0x6_u8,
            CancelReason::SELF_MATCH_PREVENTED => 0x7_u8,
            CancelReason::MAKER_AUTO_PULL => 0x8_u8,
            CancelReason::KILL_SWITCH => 0x9_u8,
            CancelReason::MARKET_RESOLVED => 0xa_u8,
            CancelReason::POSITION_CLOSED => 0xb_u8,
            CancelReason::MARKET_CLOSED => 0xc_u8,
            CancelReason::ORDER_GROUP_TRIGGERED => 0xd_u8,
            CancelReason::SPEND_CAP_REACHED => 0xe_u8,
            CancelReason::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for CancelReason {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "CLIENT_REQUEST" => Ok(Self::CLIENT_REQUEST),
            "MASS_CANCEL" => Ok(Self::MASS_CANCEL),
            "SESSION_DISCONNECT" => Ok(Self::SESSION_DISCONNECT),
            "HEARTBEAT_MISSED" => Ok(Self::HEARTBEAT_MISSED),
            "EVENT_START" => Ok(Self::EVENT_START),
            "EXPIRED" => Ok(Self::EXPIRED),
            "IOC_REMAINDER" => Ok(Self::IOC_REMAINDER),
            "SELF_MATCH_PREVENTED" => Ok(Self::SELF_MATCH_PREVENTED),
            "MAKER_AUTO_PULL" => Ok(Self::MAKER_AUTO_PULL),
            "KILL_SWITCH" => Ok(Self::KILL_SWITCH),
            "MARKET_RESOLVED" => Ok(Self::MARKET_RESOLVED),
            "POSITION_CLOSED" => Ok(Self::POSITION_CLOSED),
            "MARKET_CLOSED" => Ok(Self::MARKET_CLOSED),
            "ORDER_GROUP_TRIGGERED" => Ok(Self::ORDER_GROUP_TRIGGERED),
            "SPEND_CAP_REACHED" => Ok(Self::SPEND_CAP_REACHED),
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for CancelReason {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::CLIENT_REQUEST => write!(f, "CLIENT_REQUEST"),
            Self::MASS_CANCEL => write!(f, "MASS_CANCEL"),
            Self::SESSION_DISCONNECT => write!(f, "SESSION_DISCONNECT"),
            Self::HEARTBEAT_MISSED => write!(f, "HEARTBEAT_MISSED"),
            Self::EVENT_START => write!(f, "EVENT_START"),
            Self::EXPIRED => write!(f, "EXPIRED"),
            Self::IOC_REMAINDER => write!(f, "IOC_REMAINDER"),
            Self::SELF_MATCH_PREVENTED => write!(f, "SELF_MATCH_PREVENTED"),
            Self::MAKER_AUTO_PULL => write!(f, "MAKER_AUTO_PULL"),
            Self::KILL_SWITCH => write!(f, "KILL_SWITCH"),
            Self::MARKET_RESOLVED => write!(f, "MARKET_RESOLVED"),
            Self::POSITION_CLOSED => write!(f, "POSITION_CLOSED"),
            Self::MARKET_CLOSED => write!(f, "MARKET_CLOSED"),
            Self::ORDER_GROUP_TRIGGERED => write!(f, "ORDER_GROUP_TRIGGERED"),
            Self::SPEND_CAP_REACHED => write!(f, "SPEND_CAP_REACHED"),
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
