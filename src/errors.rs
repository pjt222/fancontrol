use thiserror::Error;

#[derive(Error, Debug)]
pub enum FanControlError {
    #[error("fan '{0}' not found")]
    FanNotFound(String),

    #[error("fan '{0}' is not controllable")]
    NotControllable(String),

    #[error("pwm value {0} out of range (0–255)")]
    PwmOutOfRange(u16),

    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("platform error: {0}")]
    Platform(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
