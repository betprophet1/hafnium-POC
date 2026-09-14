#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum PhaseSource {
    VENDOR_SIGNAL = 0x0_u8, 
    OPERATOR = 0x1_u8, 
    SCHEDULE_BACKSTOP = 0x2_u8, 
    DUMMY_ORACLE = 0x3_u8, 
    #[default]
    NullVal = 0xff_u8, 
}
impl From<u8> for PhaseSource {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::VENDOR_SIGNAL, 
            0x1_u8 => Self::OPERATOR, 
            0x2_u8 => Self::SCHEDULE_BACKSTOP, 
            0x3_u8 => Self::DUMMY_ORACLE, 
            _ => Self::NullVal,
        }
    }
}
impl From<PhaseSource> for u8 {
    #[inline]
    fn from(v: PhaseSource) -> Self {
        match v {
            PhaseSource::VENDOR_SIGNAL => 0x0_u8, 
            PhaseSource::OPERATOR => 0x1_u8, 
            PhaseSource::SCHEDULE_BACKSTOP => 0x2_u8, 
            PhaseSource::DUMMY_ORACLE => 0x3_u8, 
            PhaseSource::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for PhaseSource {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "VENDOR_SIGNAL" => Ok(Self::VENDOR_SIGNAL), 
            "OPERATOR" => Ok(Self::OPERATOR), 
            "SCHEDULE_BACKSTOP" => Ok(Self::SCHEDULE_BACKSTOP), 
            "DUMMY_ORACLE" => Ok(Self::DUMMY_ORACLE), 
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for PhaseSource {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::VENDOR_SIGNAL => write!(f, "VENDOR_SIGNAL"), 
            Self::OPERATOR => write!(f, "OPERATOR"), 
            Self::SCHEDULE_BACKSTOP => write!(f, "SCHEDULE_BACKSTOP"), 
            Self::DUMMY_ORACLE => write!(f, "DUMMY_ORACLE"), 
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
