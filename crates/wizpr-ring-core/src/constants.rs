//! WIZPR Ring BLE service and characteristic UUIDs, plus protocol constants.

use uuid::{uuid, Uuid};

/// BLE identifiers for the WIZPR Ring.
pub struct WizprBle;

impl WizprBle {
    pub const SERVICE: Uuid = uuid!("00000000-dc2e-4362-93d3-df429eb3ad10");
    pub const AUDIO_CHAR: Uuid = uuid!("00000001-dc2e-4362-93d3-df429eb3ad10");
    pub const TRANSFER_STATUS_CHAR: Uuid = uuid!("00000005-dc2e-4362-93d3-df429eb3ad10");
    pub const OPERATION_CHAR: Uuid = uuid!("00000007-dc2e-4362-93d3-df429eb3ad10");
}

/// Single-byte values written to the transfer-status characteristic.
pub mod transfer_status {
    pub const START: u8 = b'1'; // 49
    pub const STOP: u8 = b'0'; // 48
}
