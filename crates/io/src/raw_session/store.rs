//! Shared bounded source-read error and limit; durable storage is added by the finish feature.

pub(super) const MAX_BOUND_XMP_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("Camera Raw recovery I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("Camera Raw recovery validation: {0}")]
    Invalid(String),
    #[error("Unsupported recovery schema; original records retained: {0}")]
    Unsupported(String),
    #[error("Recovery publication may have completed; reconcile before continuing: {0}")]
    Uncertain(String),
    #[error("XMP publication was attempted but its outcome is not acknowledged: {0}")]
    SidecarUnknown(String),
    #[error("Camera Raw session is owned by another process")]
    Owned,
}
