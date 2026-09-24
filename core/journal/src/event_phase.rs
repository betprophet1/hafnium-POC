#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum EventPhase {
    SCHEDULED = 0x0_u8,
    STARTED = 0x1_u8,
    SUSPENDED = 0x2_u8,
    RESUMED = 0x3_u8,
    ENDED = 0x4_u8,
    #[default]
    NullVal = 0xff_u8,
}
impl From<u8> for EventPhase {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::SCHEDULED,
            0x1_u8 => Self::STARTED,
            0x2_u8 => Self::SUSPENDED,
            0x3_u8 => Self::RESUMED,
            0x4_u8 => Self::ENDED,
            _ => Self::NullVal,
        }
    }
}
impl From<EventPhase> for u8 {
    #[inline]
    fn from(v: EventPhase) -> Self {
        match v {
            EventPhase::SCHEDULED => 0x0_u8,
            EventPhase::STARTED => 0x1_u8,
            EventPhase::SUSPENDED => 0x2_u8,
            EventPhase::RESUMED => 0x3_u8,
            EventPhase::ENDED => 0x4_u8,
            EventPhase::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for EventPhase {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "SCHEDULED" => Ok(Self::SCHEDULED),
            "STARTED" => Ok(Self::STARTED),
            "SUSPENDED" => Ok(Self::SUSPENDED),
            "RESUMED" => Ok(Self::RESUMED),
            "ENDED" => Ok(Self::ENDED),
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for EventPhase {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::SCHEDULED => write!(f, "SCHEDULED"),
            Self::STARTED => write!(f, "STARTED"),
            Self::SUSPENDED => write!(f, "SUSPENDED"),
            Self::RESUMED => write!(f, "RESUMED"),
            Self::ENDED => write!(f, "ENDED"),
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
