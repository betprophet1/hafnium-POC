/// FOK fills in full at once or is rejected; it never rests or waits
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum TimeInForce {
    GTC = 0x0_u8,
    GTD = 0x1_u8,
    IOC = 0x2_u8,
    FOK = 0x3_u8,
    #[default]
    NullVal = 0xff_u8,
}
impl From<u8> for TimeInForce {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::GTC,
            0x1_u8 => Self::GTD,
            0x2_u8 => Self::IOC,
            0x3_u8 => Self::FOK,
            _ => Self::NullVal,
        }
    }
}
impl From<TimeInForce> for u8 {
    #[inline]
    fn from(v: TimeInForce) -> Self {
        match v {
            TimeInForce::GTC => 0x0_u8,
            TimeInForce::GTD => 0x1_u8,
            TimeInForce::IOC => 0x2_u8,
            TimeInForce::FOK => 0x3_u8,
            TimeInForce::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for TimeInForce {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "GTC" => Ok(Self::GTC),
            "GTD" => Ok(Self::GTD),
            "IOC" => Ok(Self::IOC),
            "FOK" => Ok(Self::FOK),
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for TimeInForce {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::GTC => write!(f, "GTC"),
            Self::GTD => write!(f, "GTD"),
            Self::IOC => write!(f, "IOC"),
            Self::FOK => write!(f, "FOK"),
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
