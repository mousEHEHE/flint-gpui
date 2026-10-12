use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum VaultError {
    #[error("invalid master password")]
    InvalidPassword,

    #[error("invalid recovery phrase")]
    InvalidRecoveryPhrase,

    #[error("the master password cannot be empty")]
    EmptyPassword,

    #[error("the master password was changed while this change was being prepared")]
    StalePasswordSlot,

    #[error("vault file is corrupted: {0}")]
    Corrupted(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("unsupported vault version: {0}")]
    UnsupportedVersion(u32),

    #[error("this host is no longer in the vault")]
    EntryNotFound(Uuid),

    #[error("this SSH key is no longer in the vault")]
    KeyNotFound(Uuid),

    #[error("this snippet is no longer in the vault")]
    SnippetNotFound(Uuid),

    #[error("this proxy is no longer in the vault")]
    ProxyNotFound(Uuid),

    #[error("invalid SSH key: {0}")]
    InvalidKey(String),

    #[error("'{0}' would be reached through itself")]
    JumpCycle(String),

    #[error("that would put this host behind more than {0} jump hosts")]
    JumpTooDeep(usize),

    #[error(transparent)]
    JumpChain(#[from] crate::vault::format::JumpChainError),
}
