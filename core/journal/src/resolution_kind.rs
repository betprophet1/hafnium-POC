#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum ResolutionKind {
    RESOLVED = 0x0_u8, 
    /// Voids the whole event, never one book (blueprint §7)
    VOID = 0x1_u8, 
    #[default]
    NullVal = 0xff_u8, 
}
impl From<u8> for ResolutionKind {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::RESOLVED, 
            0x1_u8 => Self::VOID, 
            _ => Self::NullVal,
        }
    }
}
impl From<ResolutionKind> for u8 {
    #[inline]
    fn from(v: ResolutionKind) -> Self {
        match v {
            ResolutionKind::RESOLVED => 0x0_u8, 
            ResolutionKind::VOID => 0x1_u8, 
            ResolutionKind::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for ResolutionKind {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "RESOLVED" => Ok(Self::RESOLVED), 
            "VOID" => Ok(Self::VOID), 
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for ResolutionKind {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::RESOLVED => write!(f, "RESOLVED"), 
            Self::VOID => write!(f, "VOID"), 
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
