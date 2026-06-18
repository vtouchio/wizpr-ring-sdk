use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("no Bluetooth adapter found on this system")]
    NoAdapter,

    #[error("no WIZPR Ring found within the scan timeout")]
    RingNotFound,

    #[error("required characteristic {0} not present on the ring")]
    MissingCharacteristic(&'static str),

    #[error("{0} receiver has already been taken from this connection")]
    ReceiverAlreadyTaken(&'static str),

    #[error("BLE error: {0}")]
    Btle(#[from] btleplug::Error),
}

impl Error {
    /// Whether this error came from the OS denying Bluetooth access.
    pub fn is_bluetooth_permission_denied(&self) -> bool {
        matches!(self, Self::Btle(btleplug::Error::PermissionDenied))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
