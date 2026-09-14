#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum InstrumentClass {
    BINARY = 0x0_u8, 
    CATEGORICAL_OUTCOME = 0x1_u8, 
    SCALAR_BAND = 0x2_u8, 
    PARLAY = 0x3_u8, 
    #[default]
    NullVal = 0xff_u8, 
}
impl From<u8> for InstrumentClass {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::BINARY, 
            0x1_u8 => Self::CATEGORICAL_OUTCOME, 
            0x2_u8 => Self::SCALAR_BAND, 
            0x3_u8 => Self::PARLAY, 
            _ => Self::NullVal,
        }
    }
}
impl From<InstrumentClass> for u8 {
    #[inline]
    fn from(v: InstrumentClass) -> Self {
        match v {
            InstrumentClass::BINARY => 0x0_u8, 
            InstrumentClass::CATEGORICAL_OUTCOME => 0x1_u8, 
            InstrumentClass::SCALAR_BAND => 0x2_u8, 
            InstrumentClass::PARLAY => 0x3_u8, 
            InstrumentClass::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for InstrumentClass {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "BINARY" => Ok(Self::BINARY), 
            "CATEGORICAL_OUTCOME" => Ok(Self::CATEGORICAL_OUTCOME), 
            "SCALAR_BAND" => Ok(Self::SCALAR_BAND), 
            "PARLAY" => Ok(Self::PARLAY), 
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for InstrumentClass {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BINARY => write!(f, "BINARY"), 
            Self::CATEGORICAL_OUTCOME => write!(f, "CATEGORICAL_OUTCOME"), 
            Self::SCALAR_BAND => write!(f, "SCALAR_BAND"), 
            Self::PARLAY => write!(f, "PARLAY"), 
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
