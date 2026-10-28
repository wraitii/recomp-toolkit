use serde::Serialize;

use crate::{Addr, Hex};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Input(String),
    #[error("{path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("{0}")]
    Diagnostic(Diagnostic),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn input(message: impl Into<String>) -> Self {
        Error::Input(message.into())
    }

    pub fn io(path: impl AsRef<std::path::Path>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.as_ref().display().to_string(),
            source,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Reason {
    EntryOutsideFunctions,
    EntryNotInstructionBoundary,
    OverlappingEntryDoesNotRejoin { entry: Addr },
    DecoderCannotTile { span: Addr },
    LiftFailed { detail: String },
    Unsupported { detail: String },
    Unmodelled { mnemonic: String },
    Conservative { detail: String },
    UnsupportedDirectRam { detail: String },
    UnsupportedReadModifyWrite { detail: String },
    UnsupportedArithmetic { mnemonic: String },
    UnsupportedShift { mnemonic: String },
    UnsupportedDivide { mnemonic: String },
    UnsupportedStringOp { detail: String },
    UnsupportedScalar { detail: String },
    UnsupportedCall { detail: String },
    UnsupportedControlFlow { detail: String },
    UnsupportedVarnode { detail: String },
    UnsupportedX87 { detail: String },
    SsaBudget { detail: String },
    SsaUniqueBeforeDef { detail: String },
    SsaVerify { detail: String },
    ContractNonConvergence { component: Addr },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Diagnostic {
    #[serde(serialize_with = "hex")]
    pub addr: Addr,
    #[serde(flatten)]
    pub reason: Reason,
}

fn hex<S: serde::Serializer>(addr: &Addr, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.collect_str(&Hex(*addr))
}

impl Diagnostic {
    pub fn new(addr: Addr, reason: Reason) -> Self {
        Self { addr, reason }
    }

    pub fn error(addr: Addr, reason: Reason) -> Error {
        Error::Diagnostic(Self::new(addr, reason))
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: ", Hex(self.addr))?;
        match &self.reason {
            Reason::EntryOutsideFunctions => {
                write!(
                    f,
                    "entry outside every mapped function; fix Ghidra and re-export"
                )
            }
            Reason::EntryNotInstructionBoundary => {
                write!(
                    f,
                    "entry is not an instruction boundary; fix Ghidra and re-export"
                )
            }
            Reason::OverlappingEntryDoesNotRejoin { entry } => {
                write!(
                    f,
                    "overlapping entry {} does not rejoin the function",
                    Hex(*entry)
                )
            }
            Reason::DecoderCannotTile { span } => {
                write!(f, "decoder cannot tile the mapped span at {}", Hex(*span))
            }
            Reason::LiftFailed { detail } => write!(f, "lift failed: {detail}"),
            Reason::Unsupported { detail } => write!(f, "unsupported: {detail}"),
            Reason::Unmodelled { mnemonic } => write!(f, "unmodelled instruction {mnemonic}"),
            Reason::Conservative { detail } => write!(f, "conservative policy: {detail}"),
            Reason::ContractNonConvergence { component } => write!(
                f,
                "call-contract fixpoint did not converge for component containing {}",
                Hex(*component)
            ),
            Reason::UnsupportedX87 { detail } => write!(f, "unsupported x87: {detail}"),
            Reason::UnsupportedDirectRam { detail } => {
                write!(f, "unsupported direct-ram shape: {detail}")
            }
            Reason::UnsupportedReadModifyWrite { detail } => {
                write!(f, "unsupported read-modify-write shape: {detail}")
            }
            Reason::UnsupportedArithmetic { mnemonic } => {
                write!(f, "unsupported {mnemonic} arithmetic shape")
            }
            Reason::UnsupportedShift { mnemonic } => {
                write!(f, "unsupported {mnemonic} shift shape")
            }
            Reason::UnsupportedDivide { mnemonic } => {
                write!(f, "unsupported {mnemonic} divide shape")
            }
            Reason::UnsupportedStringOp { detail } => write!(f, "unsupported string op: {detail}"),
            Reason::UnsupportedScalar { detail } => write!(f, "unsupported scalar shape: {detail}"),
            Reason::UnsupportedCall { detail } => write!(f, "unsupported call shape: {detail}"),
            Reason::UnsupportedControlFlow { detail } => {
                write!(f, "unsupported control flow: {detail}")
            }
            Reason::UnsupportedVarnode { detail } => write!(f, "unsupported varnode: {detail}"),
            Reason::SsaBudget { detail } => write!(f, "SSA budget exceeded: {detail}"),
            Reason::SsaUniqueBeforeDef { detail } => {
                write!(f, "SSA unique read before definition: {detail}")
            }
            Reason::SsaVerify { detail } => write!(f, "SSA verification failed: {detail}"),
        }
    }
}
