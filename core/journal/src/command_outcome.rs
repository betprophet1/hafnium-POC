#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum CommandOutcome {
    APPLIED = 0x0_u8, 
    REJECTED = 0x1_u8, 
    /// The command ID was already applied; nothing changed
    DUPLICATE = 0x2_u8, 
    #[default]
    NullVal = 0xff_u8, 
}
impl From<u8> for CommandOutcome {
    #[inline]
    fn from(v: u8) -> Self {
        match v {
            0x0_u8 => Self::APPLIED, 
            0x1_u8 => Self::REJECTED, 
            0x2_u8 => Self::DUPLICATE, 
            _ => Self::NullVal,
        }
    }
}
impl From<CommandOutcome> for u8 {
    #[inline]
    fn from(v: CommandOutcome) -> Self {
        match v {
            CommandOutcome::APPLIED => 0x0_u8, 
            CommandOutcome::REJECTED => 0x1_u8, 
            CommandOutcome::DUPLICATE => 0x2_u8, 
            CommandOutcome::NullVal => 0xff_u8,
        }
    }
}
impl core::str::FromStr for CommandOutcome {
    type Err = ();

    #[inline]
    fn from_str(v: &str) -> core::result::Result<Self, Self::Err> {
        match v {
            "APPLIED" => Ok(Self::APPLIED), 
            "REJECTED" => Ok(Self::REJECTED), 
            "DUPLICATE" => Ok(Self::DUPLICATE), 
            _ => Ok(Self::NullVal),
        }
    }
}
impl core::fmt::Display for CommandOutcome {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::APPLIED => write!(f, "APPLIED"), 
            Self::REJECTED => write!(f, "REJECTED"), 
            Self::DUPLICATE => write!(f, "DUPLICATE"), 
            Self::NullVal => write!(f, "NullVal"),
        }
    }
}
