use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use btleplug::api::{Central, Manager as _, Peripheral as _, PeripheralProperties, ScanFilter};
#[cfg(target_vendor = "apple")]
use btleplug::platform::PeripheralId;
use btleplug::platform::{Adapter, Manager};
use uuid::Uuid;
use wizpr_ring_core::WizprBle;

use crate::{Error, Result, RingConnection};

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const RING_NAME_PREFIX: &str = "WIZPR RING";

/// Discovers WIZPR Rings on the system's first Bluetooth adapter.
pub struct RingScanner {
    adapter: Adapter,
}

/// A WIZPR Ring discovered during BLE scanning.
///
/// Applications should present discovered candidates to the user or match them
/// against a remembered device id before connecting.
#[derive(Clone, Debug)]
pub struct RingDevice {
    peripheral: btleplug::platform::Peripheral,
    properties: PeripheralProperties,
}

impl RingDevice {
    fn new(peripheral: btleplug::platform::Peripheral, properties: PeripheralProperties) -> Self {
        Self {
            peripheral,
            properties,
        }
    }

    /// Stable platform identifier for this BLE peripheral.
    pub fn id(&self) -> String {
        self.peripheral.id().to_string()
    }

    /// Advertised local name, when present.
    pub fn name(&self) -> Option<&str> {
        self.properties.local_name.as_deref()
    }

    /// Advertised Bluetooth address as reported by the host platform.
    pub fn address(&self) -> String {
        self.properties.address.to_string()
    }

    /// Latest RSSI observed during scanning, when available.
    pub const fn rssi(&self) -> Option<i16> {
        self.properties.rssi
    }

    /// Advertised service UUIDs.
    pub fn advertised_services(&self) -> &[Uuid] {
        &self.properties.services
    }

    /// Whether this candidate advertised the WIZPR Ring service UUID.
    pub fn advertises_wizpr_service(&self) -> bool {
        self.properties.services.contains(&WizprBle::SERVICE)
    }

    /// Connect to this explicitly selected ring candidate.
    pub async fn connect(self) -> Result<RingConnection> {
        RingConnection::open(self.peripheral).await
    }
}

/// A previously connected ring, retrieved by its stable id without scanning.
///
/// On macOS this is backed by CoreBluetooth's
/// `retrievePeripheralsWithIdentifiers:`, so the device does not need to be
/// powered on or advertising at retrieval time.
#[derive(Clone)]
pub struct KnownRing {
    peripheral: btleplug::platform::Peripheral,
}

impl KnownRing {
    /// Stable platform identifier for this BLE peripheral.
    pub fn id(&self) -> String {
        self.peripheral.id().to_string()
    }

    /// Connect to the ring, waiting indefinitely until it becomes reachable.
    ///
    /// The underlying OS connect request has no timeout ("pending connect"):
    /// if the ring is powered off, this future resolves as soon as the ring
    /// powers on and the system completes the connection. Use [`cancel`] from
    /// another task/clone to abort the wait.
    ///
    /// [`cancel`]: Self::cancel
    pub async fn connect(&self) -> Result<RingConnection> {
        RingConnection::open(self.peripheral.clone()).await
    }

    /// Cancel a pending connect issued by [`connect`] (or disconnect if the
    /// connection already completed).
    ///
    /// [`connect`]: Self::connect
    pub async fn cancel(&self) -> Result<()> {
        self.peripheral.disconnect().await?;
        Ok(())
    }
}

impl RingScanner {
    /// Initialize the scanner using the first available Bluetooth adapter.
    pub async fn new() -> Result<Self> {
        let manager = Manager::new().await?;
        let adapters = manager.adapters().await?;
        let adapter = adapters.into_iter().next().ok_or(Error::NoAdapter)?;
        Ok(Self { adapter })
    }

    /// Retrieve a previously connected ring by its stable id without scanning.
    ///
    /// `device_id` is the value previously obtained from [`RingDevice::id`].
    /// Returns [`Error::RingNotFound`] when the platform has no record of the
    /// identifier. This retrieval flow is currently supported on Apple
    /// platforms only.
    pub async fn known_ring(&self, device_id: &str) -> Result<KnownRing> {
        #[cfg(target_vendor = "apple")]
        {
            let uuid = Uuid::parse_str(device_id).map_err(|_| Error::RingNotFound)?;
            let peripheral = self
                .adapter
                .add_peripheral(&PeripheralId::from(uuid))
                .await
                .map_err(|err| match err {
                    btleplug::Error::DeviceNotFound => Error::RingNotFound,
                    other => Error::Btle(other),
                })?;
            Ok(KnownRing { peripheral })
        }

        #[cfg(not(target_vendor = "apple"))]
        {
            let _ = device_id;
            Err(Error::Btle(btleplug::Error::NotSupported(
                "retrieving a known ring without scanning is supported on Apple platforms only"
                    .to_string(),
            )))
        }
    }

    /// Scan for WIZPR Ring candidates for the full `timeout` window.
    ///
    /// This is the preferred public flow: scan, let the app or user choose a
    /// candidate, then call [`RingDevice::connect`] on the selected device.
    /// An empty vector means no candidate was observed before the timeout.
    pub async fn scan(&self, timeout: Duration) -> Result<Vec<RingDevice>> {
        self.adapter.start_scan(ScanFilter::default()).await?;

        let scan_result = self.collect_candidates(timeout).await;
        let stop_result = self.adapter.stop_scan().await;

        match scan_result {
            Ok(candidates) => {
                stop_result?;
                Ok(candidates)
            }
            Err(err) => {
                let _ = stop_result;
                Err(err)
            }
        }
    }

    /// Scan until a WIZPR Ring candidate satisfies `predicate`, or `timeout` elapses.
    ///
    /// This is useful when an app already has a remembered device id and wants
    /// to connect as soon as that device is observed.
    pub async fn scan_until<F>(
        &self,
        timeout: Duration,
        mut predicate: F,
    ) -> Result<Option<RingDevice>>
    where
        F: FnMut(&RingDevice) -> bool,
    {
        self.adapter.start_scan(ScanFilter::default()).await?;

        let scan_result = self.find_candidate(timeout, &mut predicate).await;
        let stop_result = self.adapter.stop_scan().await;

        match scan_result {
            Ok(candidate) => {
                stop_result?;
                Ok(candidate)
            }
            Err(err) => {
                let _ = stop_result;
                Err(err)
            }
        }
    }

    /// Scan for and connect to the first ring discovered within `timeout`.
    ///
    /// This is retained as a convenience for diagnostics. Product apps should
    /// prefer [`scan`] and connect to an explicitly selected [`RingDevice`].
    ///
    /// [`scan`]: Self::scan
    pub async fn connect_first(&self, timeout: Duration) -> Result<RingConnection> {
        self.adapter.start_scan(ScanFilter::default()).await?;

        let scan_result = self.find_first(timeout).await;
        let stop_result = self.adapter.stop_scan().await;

        let peripheral = match scan_result {
            Ok(peripheral) => {
                stop_result?;
                peripheral
            }
            Err(err) => {
                let _ = stop_result;
                return Err(err);
            }
        };

        RingConnection::open(peripheral).await
    }

    async fn collect_candidates(&self, timeout: Duration) -> Result<Vec<RingDevice>> {
        let deadline = Instant::now() + timeout;
        let mut seen = BTreeSet::new();
        let mut candidates = Vec::new();

        loop {
            for p in self.adapter.peripherals().await? {
                let Ok(Some(props)) = p.properties().await else {
                    continue;
                };
                if !is_ring_candidate(&props) {
                    continue;
                }

                let id = p.id().to_string();
                if seen.insert(id) {
                    candidates.push(RingDevice::new(p, props));
                }
            }

            if Instant::now() >= deadline {
                return Ok(candidates);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn find_candidate<F>(
        &self,
        timeout: Duration,
        predicate: &mut F,
    ) -> Result<Option<RingDevice>>
    where
        F: FnMut(&RingDevice) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            for p in self.adapter.peripherals().await? {
                let Ok(Some(props)) = p.properties().await else {
                    continue;
                };
                if !is_ring_candidate(&props) {
                    continue;
                }

                let candidate = RingDevice::new(p, props);
                if predicate(&candidate) {
                    return Ok(Some(candidate));
                }
            }

            if Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn find_first(&self, timeout: Duration) -> Result<btleplug::platform::Peripheral> {
        let deadline = Instant::now() + timeout;
        loop {
            for p in self.adapter.peripherals().await? {
                if let Ok(Some(props)) = p.properties().await {
                    if is_ring_candidate(&props) {
                        return Ok(p);
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(Error::RingNotFound);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

fn is_ring_candidate(props: &PeripheralProperties) -> bool {
    props.services.contains(&WizprBle::SERVICE)
        || props
            .local_name
            .as_deref()
            .map(is_ring_name)
            .unwrap_or(false)
}

fn is_ring_name(name: &str) -> bool {
    name.to_ascii_uppercase().contains(RING_NAME_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_ring_service_uuid() {
        let props = PeripheralProperties {
            services: vec![WizprBle::SERVICE],
            ..PeripheralProperties::default()
        };

        assert!(is_ring_candidate(&props));
    }

    #[test]
    fn matches_ring_local_name() {
        let props = PeripheralProperties {
            local_name: Some("WIZPR RING".to_string()),
            ..PeripheralProperties::default()
        };

        assert!(is_ring_candidate(&props));
    }

    #[test]
    fn rejects_non_ring_name_without_service() {
        let props = PeripheralProperties {
            local_name: Some("WIZPR CASE".to_string()),
            ..PeripheralProperties::default()
        };

        assert!(!is_ring_candidate(&props));
    }
}
