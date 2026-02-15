use thiserror::Error;

#[derive(Error, Debug)]
pub enum FanControlError {
    #[error("fan '{0}' not found")]
    FanNotFound(String),

    #[error("fan '{0}' is not controllable")]
    NotControllable(String),

    #[error("invalid fan curve: {0}")]
    InvalidCurve(String),

    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("platform error: {0}")]
    Platform(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
