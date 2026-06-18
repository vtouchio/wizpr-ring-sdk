use std::sync::Mutex;
use std::time::Duration;

use btleplug::api::{Characteristic, Peripheral as _, WriteType};
use btleplug::platform::Peripheral;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use uuid::Uuid;

use wizpr_ring_core::{
    audio::SAMPLE_RATE_HZ, codec, codec::CodecState, events::parse_operation, transfer_status,
    AudioChunk, RingEvent, WizprBle,
};

use crate::{Error, Result};

const AUDIO_CHANNEL_CAPACITY: usize = 4096;
const EVENT_CHANNEL_CAPACITY: usize = 64;
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(1000);
const SLEEP_PARK: Duration = Duration::from_secs(60 * 60);

pub type AudioRx = mpsc::Receiver<AudioChunk>;
pub type EventRx = mpsc::Receiver<RingEvent>;

/// An active BLE connection to a WIZPR Ring.
///
/// Audio and event streams are exposed as bounded async channels: a
/// background task decodes ADPCM packets and forwards them. The audio
/// channel is sized to absorb temporary scheduling or logging delays, but
/// callers should still drain it promptly because BLE audio is real-time.
/// Call [`audio`] and [`events`] *once each* to take ownership of the receivers.
///
/// Call [`disconnect`] when you want to explicitly close the BLE connection.
/// Dropping this value stops the background dispatcher, but async BLE
/// disconnect must be requested explicitly.
///
/// [`audio`]: Self::audio
/// [`events`]: Self::events
/// [`disconnect`]: Self::disconnect
pub struct RingConnection {
    peripheral: Peripheral,
    operation_char: Characteristic,
    audio_rx: Mutex<Option<AudioRx>>,
    event_rx: Mutex<Option<EventRx>>,
    dispatcher: Mutex<Option<JoinHandle<()>>>,
}

impl RingConnection {
    /// Connect, discover characteristics, subscribe, and start the dispatcher task.
    pub(crate) async fn open(peripheral: Peripheral) -> Result<Self> {
        peripheral.connect().await?;

        let init_result = async {
            peripheral.discover_services().await?;

            let chars = peripheral.characteristics();
            let audio_char = find_char(&chars, WizprBle::AUDIO_CHAR, "audio")?;
            let transfer_char =
                find_char(&chars, WizprBle::TRANSFER_STATUS_CHAR, "transfer_status")?;
            let operation_char = find_char(&chars, WizprBle::OPERATION_CHAR, "operation")?;

            peripheral.subscribe(&audio_char).await?;
            peripheral.subscribe(&transfer_char).await?;
            peripheral.subscribe(&operation_char).await?;

            let notifications = peripheral.notifications().await?;
            Ok((operation_char, notifications))
        }
        .await;

        let (operation_char, notifications) = match init_result {
            Ok(init) => init,
            Err(err) => {
                let _ = peripheral.disconnect().await;
                return Err(err);
            }
        };

        let (audio_tx, audio_rx) = mpsc::channel(AUDIO_CHANNEL_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);

        let dispatcher = tokio::spawn(run_dispatcher(notifications, audio_tx, event_tx));

        let conn = Self {
            peripheral,
            operation_char,
            audio_rx: Mutex::new(Some(audio_rx)),
            event_rx: Mutex::new(Some(event_rx)),
            dispatcher: Mutex::new(Some(dispatcher)),
        };

        if let Err(err) = conn.configure_ring().await {
            let _ = conn.shutdown().await;
            return Err(err);
        }

        Ok(conn)
    }

    async fn configure_ring(&self) -> Result<()> {
        self.write_operation_command(OperationCommand::SampleRate16)
            .await?;
        self.request_battery_update().await?;
        Ok(())
    }

    /// Take the audio receiver.
    ///
    /// Returns [`Error::ReceiverAlreadyTaken`] if called more than once.
    pub fn audio(&self) -> Result<AudioRx> {
        self.audio_rx
            .lock()
            .unwrap()
            .take()
            .ok_or(Error::ReceiverAlreadyTaken("audio"))
    }

    /// Take the events receiver.
    ///
    /// Returns [`Error::ReceiverAlreadyTaken`] if called more than once.
    pub fn events(&self) -> Result<EventRx> {
        self.event_rx
            .lock()
            .unwrap()
            .take()
            .ok_or(Error::ReceiverAlreadyTaken("events"))
    }

    async fn write_operation_command(&self, command: OperationCommand) -> Result<()> {
        self.peripheral
            .write(
                &self.operation_char,
                command.as_str().as_bytes(),
                WriteType::WithResponse,
            )
            .await?;
        Ok(())
    }

    /// Ask the ring to publish its current battery voltage.
    ///
    /// This is an allowlisted status request. The public SDK intentionally
    /// does not expose raw firmware command transport.
    pub async fn request_battery_update(&self) -> Result<()> {
        self.write_operation_command(OperationCommand::BatteryAdc)
            .await
    }

    /// Whether the underlying GATT connection is still up.
    pub async fn is_connected(&self) -> bool {
        self.peripheral.is_connected().await.unwrap_or(false)
    }

    /// Disconnect from the ring and stop the dispatcher task.
    pub async fn disconnect(self) -> Result<()> {
        self.shutdown().await
    }

    async fn shutdown(&self) -> Result<()> {
        self.abort_dispatcher();
        self.peripheral.disconnect().await?;
        Ok(())
    }

    fn abort_dispatcher(&self) {
        if let Some(h) = self.dispatcher.lock().unwrap().take() {
            h.abort();
        }
    }
}

impl Drop for RingConnection {
    fn drop(&mut self) {
        self.abort_dispatcher();
    }
}

#[derive(Debug, Clone, Copy)]
enum OperationCommand {
    SampleRate16,
    BatteryAdc,
}

impl OperationCommand {
    const fn as_str(self) -> &'static str {
        match self {
            Self::SampleRate16 => "sample_rate 16",
            Self::BatteryAdc => "batt_adc",
        }
    }
}

fn find_char(
    chars: &std::collections::BTreeSet<Characteristic>,
    uuid: Uuid,
    name: &'static str,
) -> Result<Characteristic> {
    chars
        .iter()
        .find(|c| c.uuid == uuid)
        .cloned()
        .ok_or(Error::MissingCharacteristic(name))
}

async fn run_dispatcher(
    mut notifications: std::pin::Pin<
        Box<dyn futures::Stream<Item = btleplug::api::ValueNotification> + Send>,
    >,
    audio_tx: mpsc::Sender<AudioChunk>,
    event_tx: mpsc::Sender<RingEvent>,
) {
    let mut state = DispatchState::new();
    let mut click_armed = false;

    let click_sleep = tokio::time::sleep(SLEEP_PARK);
    tokio::pin!(click_sleep);

    loop {
        tokio::select! {
            biased;

            notif = notifications.next() => {
                let Some(notif) = notif else { break };
                let out = state.handle_notification(notif.uuid, &notif.value, now_ms());
                if let Some(chunk) = out.audio {
                    if audio_tx.send(chunk).await.is_err() { break; }
                }
                for event in out.events {
                    if event_tx.send(event).await.is_err() { break; }
                }
                match out.click_timer {
                    ClickTimerAction::None => {}
                    ClickTimerAction::Arm => {
                        click_sleep.as_mut().reset(Instant::now() + DOUBLE_CLICK_WINDOW);
                        click_armed = true;
                    }
                    ClickTimerAction::Disarm => {
                        click_sleep.as_mut().reset(Instant::now() + SLEEP_PARK);
                        click_armed = false;
                    }
                }
            }

            _ = &mut click_sleep, if click_armed => {
                click_armed = false;
                if let Some(event) = state.handle_click_timeout() {
                    if event_tx.send(event).await.is_err() {
                        break;
                    }
                }
                click_sleep.as_mut().reset(Instant::now() + SLEEP_PARK);
            }
        }
    }
}

#[derive(Debug, Default)]
struct DispatchState {
    codec_state: CodecState,
    click_count: u32,
}

impl DispatchState {
    fn new() -> Self {
        Self::default()
    }

    fn handle_notification(
        &mut self,
        uuid: Uuid,
        value: &[u8],
        timestamp_ms: u64,
    ) -> DispatchOutput {
        if uuid == WizprBle::AUDIO_CHAR {
            return self.handle_audio(value, timestamp_ms);
        }

        if uuid == WizprBle::TRANSFER_STATUS_CHAR {
            return self.handle_transfer_status(value);
        }

        if uuid == WizprBle::OPERATION_CHAR {
            return self.handle_operation(value);
        }

        DispatchOutput::default()
    }

    fn handle_audio(&mut self, value: &[u8], timestamp_ms: u64) -> DispatchOutput {
        let pcm = codec::decode(value, &mut self.codec_state);
        DispatchOutput {
            audio: Some(AudioChunk {
                pcm_samples: pcm,
                sample_rate: SAMPLE_RATE_HZ,
                timestamp_ms,
            }),
            ..DispatchOutput::default()
        }
    }

    fn handle_transfer_status(&mut self, value: &[u8]) -> DispatchOutput {
        let Some(&first) = value.first() else {
            return DispatchOutput::default();
        };

        let event = match first {
            transfer_status::START => {
                self.codec_state = CodecState::new();
                RingEvent::RecordingStarted
            }
            transfer_status::STOP => RingEvent::RecordingStopped,
            _ => return DispatchOutput::default(),
        };

        DispatchOutput {
            events: vec![event],
            ..DispatchOutput::default()
        }
    }

    fn handle_operation(&mut self, value: &[u8]) -> DispatchOutput {
        let text = String::from_utf8_lossy(value);
        if text.contains("CLICK") {
            return self.handle_click();
        }

        DispatchOutput {
            events: vec![parse_operation(&text)],
            ..DispatchOutput::default()
        }
    }

    fn handle_click(&mut self) -> DispatchOutput {
        self.click_count += 1;
        if self.click_count >= 2 {
            self.click_count = 0;
            DispatchOutput {
                events: vec![RingEvent::DoubleClick],
                click_timer: ClickTimerAction::Disarm,
                ..DispatchOutput::default()
            }
        } else {
            DispatchOutput {
                click_timer: ClickTimerAction::Arm,
                ..DispatchOutput::default()
            }
        }
    }

    fn handle_click_timeout(&mut self) -> Option<RingEvent> {
        let event = (self.click_count == 1).then_some(RingEvent::Click);
        self.click_count = 0;
        event
    }
}

#[derive(Debug, Default)]
struct DispatchOutput {
    audio: Option<AudioChunk>,
    events: Vec<RingEvent>,
    click_timer: ClickTimerAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ClickTimerAction {
    #[default]
    None,
    Arm,
    Disarm,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_notification_decodes_pcm_chunk() {
        let mut state = DispatchState::new();
        let out = state.handle_notification(WizprBle::AUDIO_CHAR, &[0x12, 0x34], 123);

        let chunk = out.audio.expect("expected audio chunk");
        assert_eq!(chunk.pcm_samples.len(), 4);
        assert_eq!(chunk.sample_rate, SAMPLE_RATE_HZ);
        assert_eq!(chunk.timestamp_ms, 123);
        assert!(out.events.is_empty());
    }

    #[test]
    fn transfer_start_resets_codec_and_emits_event() {
        let mut state = DispatchState::new();
        state.handle_notification(WizprBle::AUDIO_CHAR, &[0x77; 4], 1);
        assert_ne!(state.codec_state.step_index(), 0);

        let out =
            state.handle_notification(WizprBle::TRANSFER_STATUS_CHAR, &[transfer_status::START], 2);

        assert_eq!(out.events, vec![RingEvent::RecordingStarted]);
        assert_eq!(state.codec_state.step_index(), 0);
        assert_eq!(state.codec_state.predicted_sample(), 0);
    }

    #[test]
    fn transfer_stop_emits_event() {
        let mut state = DispatchState::new();
        let out =
            state.handle_notification(WizprBle::TRANSFER_STATUS_CHAR, &[transfer_status::STOP], 1);

        assert_eq!(out.events, vec![RingEvent::RecordingStopped]);
    }

    #[test]
    fn unknown_transfer_status_is_ignored() {
        let mut state = DispatchState::new();
        let out = state.handle_notification(WizprBle::TRANSFER_STATUS_CHAR, b"x", 1);

        assert!(out.audio.is_none());
        assert!(out.events.is_empty());
        assert_eq!(out.click_timer, ClickTimerAction::None);
    }

    #[test]
    fn operation_notification_uses_core_parser() {
        let mut state = DispatchState::new();
        let out = state.handle_notification(WizprBle::OPERATION_CHAR, b"MIC_ON", 1);

        assert_eq!(out.events, vec![RingEvent::MicOn]);
    }

    #[test]
    fn first_click_arms_timer_and_timeout_emits_single_click() {
        let mut state = DispatchState::new();
        let out = state.handle_notification(WizprBle::OPERATION_CHAR, b"CLICK", 1);

        assert!(out.events.is_empty());
        assert_eq!(out.click_timer, ClickTimerAction::Arm);
        assert_eq!(state.handle_click_timeout(), Some(RingEvent::Click));
        assert_eq!(state.handle_click_timeout(), None);
    }

    #[test]
    fn second_click_emits_double_click_and_disarms_timer() {
        let mut state = DispatchState::new();
        let first = state.handle_notification(WizprBle::OPERATION_CHAR, b"CLICK", 1);
        let second = state.handle_notification(WizprBle::OPERATION_CHAR, b"CLICK", 2);

        assert_eq!(first.click_timer, ClickTimerAction::Arm);
        assert_eq!(second.events, vec![RingEvent::DoubleClick]);
        assert_eq!(second.click_timer, ClickTimerAction::Disarm);
        assert_eq!(state.handle_click_timeout(), None);
    }
}
