#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum RejectReason {
    NONE = 0x0_u8,
    MALFORMED = 0x1_u8,
    UNKNOWN_INSTRUMENT = 0x2_u8,
    UNKNOWN_ORDER = 0x3_u8,
    MARKET_NOT_OPEN = 0x4_u8,
    VENUE_MODE = 0x5_u8,
    /// Reserved for in-play restrictions; not emitted in v0
    EVENT_STARTED = 0x6_u8,
    TICK_NOT_ON_LADDER = 0x7_u8,
    QUANTITY_NOT_ON_LOT_STEP = 0x8_u8,
    EXPIRY_IN_PAST = 0x9_u8,
    INSUFFICIENT_SLEEVE = 0xa_u8,
    POST_ONLY_WOULD_TAKE = 0xb_u8,
    FOK_NOT_FILLABLE = 0xc_u8,
    REDUCE_ONLY_NO_POSITION = 0xd_u8,
    CLOSED_ONLY_MODE = 0xe_u8,
    SPEND_CAP_BELOW_ONE_CONTRACT = 0xf_u8,
    DUPLICATE_CLIENT_ORDER_ID = 0x10_u8,
    RULE_LIMIT = 0x11_u8,
    POSITION_LIMIT = 0x12_u8,
    /// Only for an order that would rest; never a fill, cancel or reduction (blueprint §6)
    ORDER_CAPACITY = 0x13_u8,
    PROTOCOL_VERSION = 0x14_u8,
    INVALID_COMMAND = 0x15_u8,
    ORDER_GROUP_TRIGGERED = 0x16_u8,
    /// A DefineEvent or ListInstrument past maxEvents, maxInstruments or maxPriceLevels (spec E-6.2, E-6.3)
    LISTING_CAPACITY = 0x17_u8,
    #[default]
    NullVal = 0xff_u8,
}
impl From<u8> for RejectReason {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::NONE,
            0x1_u8 => Self::MALFORMED,
            0x2_u8 => Self::UNKNOWN_INSTRUMENT,
            0x3_u8 => Self::UNKNOWN_ORDER,
            0x4_u8 => Self::MARKET_NOT_OPEN,
            0x5_u8 => Self::VENUE_MODE,
            0x6_u8 => Self::EVENT_STARTED,
            0x7_u8 => Self::TICK_NOT_ON_LADDER,
            0x8_u8 => Self::QUANTITY_NOT_ON_LOT_STEP,
            0x9_u8 => Self::EXPIRY_IN_PAST,
            0xa_u8 => Self::INSUFFICIENT_SLEEVE,
            0xb_u8 => Self::POST_ONLY_WOULD_TAKE,
            0xc_u8 => Self::FOK_NOT_FILLABLE,
            0xd_u8 => Self::REDUCE_ONLY_NO_POSITION,
            0xe_u8 => Self::CLOSED_ONLY_MODE,
            0xf_u8 => Self::SPEND_CAP_BELOW_ONE_CONTRACT,
            0x10_u8 => Self::DUPLICATE_CLIENT_ORDER_ID,
            0x11_u8 => Self::RULE_LIMIT,
            0x12_u8 => Self::POSITION_LIMIT,
            0x13_u8 => Self::ORDER_CAPACITY,
            0x14_u8 => Self::PROTOCOL_VERSION,
            0x15_u8 => Self::INVALID_COMMAND,
            0x16_u8 => Self::ORDER_GROUP_TRIGGERED,
            0x17_u8 => Self::LISTING_CAPACITY,
            _ => Self::NullVal,
        }
    }
}
impl From<RejectReason> for u8 {
    #[inline]
    fn from(v: RejectReason) -> Self {
        match v {
            RejectReason::NONE => 0x0_u8,
            RejectReason::MALFORMED => 0x1_u8,
            RejectReason::UNKNOWN_INSTRUMENT => 0x2_u8,
            RejectReason::UNKNOWN_ORDER => 0x3_u8,
            RejectReason::MARKET_NOT_OPEN => 0x4_u8,
            RejectReason::VENUE_MODE => 0x5_u8,
            RejectReason::EVENT_STARTED => 0x6_u8,
            RejectReason::TICK_NOT_ON_LADDER => 0x7_u8,
            RejectReason::QUANTITY_NOT_ON_LOT_STEP => 0x8_u8,
            RejectReason::EXPIRY_IN_PAST => 0x9_u8,
            RejectReason::INSUFFICIENT_SLEEVE => 0xa_u8,
            RejectReason::POST_ONLY_WOULD_TAKE => 0xb_u8,
            RejectReason::FOK_NOT_FILLABLE => 0xc_u8,
            RejectReason::REDUCE_ONLY_NO_POSITION => 0xd_u8,
            RejectReason::CLOSED_ONLY_MODE => 0xe_u8,
            RejectReason::SPEND_CAP_BELOW_ONE_CONTRACT => 0xf_u8,
            RejectReason::DUPLICATE_CLIENT_ORDER_ID => 0x10_u8,
            RejectReason::RULE_LIMIT => 0x11_u8,
            RejectReason::POSITION_LIMIT => 0x12_u8,
            RejectReason::ORDER_CAPACITY => 0x13_u8,
            RejectReason::PROTOCOL_VERSION => 0x14_u8,
            RejectReason::INVALID_COMMAND => 0x15_u8,
            RejectReason::ORDER_GROUP_TRIGGERED => 0x16_u8,
            RejectReason::LISTING_CAPACITY => 0x17_u8,
            RejectReason::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for RejectReason {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "NONE" => Ok(Self::NONE),
            "MALFORMED" => Ok(Self::MALFORMED),
            "UNKNOWN_INSTRUMENT" => Ok(Self::UNKNOWN_INSTRUMENT),
            "UNKNOWN_ORDER" => Ok(Self::UNKNOWN_ORDER),
            "MARKET_NOT_OPEN" => Ok(Self::MARKET_NOT_OPEN),
            "VENUE_MODE" => Ok(Self::VENUE_MODE),
            "EVENT_STARTED" => Ok(Self::EVENT_STARTED),
            "TICK_NOT_ON_LADDER" => Ok(Self::TICK_NOT_ON_LADDER),
            "QUANTITY_NOT_ON_LOT_STEP" => Ok(Self::QUANTITY_NOT_ON_LOT_STEP),
            "EXPIRY_IN_PAST" => Ok(Self::EXPIRY_IN_PAST),
            "INSUFFICIENT_SLEEVE" => Ok(Self::INSUFFICIENT_SLEEVE),
            "POST_ONLY_WOULD_TAKE" => Ok(Self::POST_ONLY_WOULD_TAKE),
            "FOK_NOT_FILLABLE" => Ok(Self::FOK_NOT_FILLABLE),
            "REDUCE_ONLY_NO_POSITION" => Ok(Self::REDUCE_ONLY_NO_POSITION),
            "CLOSED_ONLY_MODE" => Ok(Self::CLOSED_ONLY_MODE),
            "SPEND_CAP_BELOW_ONE_CONTRACT" => Ok(Self::SPEND_CAP_BELOW_ONE_CONTRACT),
            "DUPLICATE_CLIENT_ORDER_ID" => Ok(Self::DUPLICATE_CLIENT_ORDER_ID),
            "RULE_LIMIT" => Ok(Self::RULE_LIMIT),
            "POSITION_LIMIT" => Ok(Self::POSITION_LIMIT),
            "ORDER_CAPACITY" => Ok(Self::ORDER_CAPACITY),
            "PROTOCOL_VERSION" => Ok(Self::PROTOCOL_VERSION),
            "INVALID_COMMAND" => Ok(Self::INVALID_COMMAND),
            "ORDER_GROUP_TRIGGERED" => Ok(Self::ORDER_GROUP_TRIGGERED),
            "LISTING_CAPACITY" => Ok(Self::LISTING_CAPACITY),
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for RejectReason {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NONE => write!(f, "NONE"),
            Self::MALFORMED => write!(f, "MALFORMED"),
            Self::UNKNOWN_INSTRUMENT => write!(f, "UNKNOWN_INSTRUMENT"),
            Self::UNKNOWN_ORDER => write!(f, "UNKNOWN_ORDER"),
            Self::MARKET_NOT_OPEN => write!(f, "MARKET_NOT_OPEN"),
            Self::VENUE_MODE => write!(f, "VENUE_MODE"),
            Self::EVENT_STARTED => write!(f, "EVENT_STARTED"),
            Self::TICK_NOT_ON_LADDER => write!(f, "TICK_NOT_ON_LADDER"),
            Self::QUANTITY_NOT_ON_LOT_STEP => write!(f, "QUANTITY_NOT_ON_LOT_STEP"),
            Self::EXPIRY_IN_PAST => write!(f, "EXPIRY_IN_PAST"),
            Self::INSUFFICIENT_SLEEVE => write!(f, "INSUFFICIENT_SLEEVE"),
            Self::POST_ONLY_WOULD_TAKE => write!(f, "POST_ONLY_WOULD_TAKE"),
            Self::FOK_NOT_FILLABLE => write!(f, "FOK_NOT_FILLABLE"),
            Self::REDUCE_ONLY_NO_POSITION => write!(f, "REDUCE_ONLY_NO_POSITION"),
            Self::CLOSED_ONLY_MODE => write!(f, "CLOSED_ONLY_MODE"),
            Self::SPEND_CAP_BELOW_ONE_CONTRACT => write!(f, "SPEND_CAP_BELOW_ONE_CONTRACT"),
            Self::DUPLICATE_CLIENT_ORDER_ID => write!(f, "DUPLICATE_CLIENT_ORDER_ID"),
            Self::RULE_LIMIT => write!(f, "RULE_LIMIT"),
            Self::POSITION_LIMIT => write!(f, "POSITION_LIMIT"),
            Self::ORDER_CAPACITY => write!(f, "ORDER_CAPACITY"),
            Self::PROTOCOL_VERSION => write!(f, "PROTOCOL_VERSION"),
            Self::INVALID_COMMAND => write!(f, "INVALID_COMMAND"),
            Self::ORDER_GROUP_TRIGGERED => write!(f, "ORDER_GROUP_TRIGGERED"),
            Self::LISTING_CAPACITY => write!(f, "LISTING_CAPACITY"),
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
