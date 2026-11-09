use std::sync::{Arc, Mutex};
use std::time::Duration;

use btleplug::api::{Characteristic, Peripheral as _, WriteType};
use btleplug::platform::Peripheral;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use uuid::Uuid;

use wizpr_ring_core::{
    audio::SAMPLE_RATE_HZ, events::parse_operation, transfer_status, AudioChunk, AudioDecoder,
    AudioProcessing, RingEvent, WizprBle,
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
    processing: Arc<ProcessingSetting>,
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

        let processing = Arc::new(ProcessingSetting::default());
        let dispatcher = tokio::spawn(run_dispatcher(
            notifications,
            audio_tx,
            event_tx,
            Arc::clone(&processing),
        ));

        let conn = Self {
            peripheral,
            operation_char,
            audio_rx: Mutex::new(Some(audio_rx)),
            event_rx: Mutex::new(Some(event_rx)),
            dispatcher: Mutex::new(Some(dispatcher)),
            processing,
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
        self.write_operation_command(OperationCommand::BatteryStatus)
            .await
    }

    /// Send the same LOCK operation used by the mobile apps.
    /// The host owns input gating and unlock policy; this command alone does
    /// not guarantee that audio/recording notifications stop arriving.
    pub async fn request_lock(&self) -> Result<()> {
        self.write_operation_command(OperationCommand::Lock).await
    }

    /// Choose how decoded audio is post-processed. Defaults to
    /// [`AudioProcessing::Raw`].
    ///
    /// The change takes effect at the next recording start (or right away
    /// if no audio has been decoded since the last one), so a recording is
    /// never half raw and half cleaned.
    pub fn set_audio_processing(&self, processing: AudioProcessing) {
        self.processing.set(processing);
    }

    /// Processing requested with [`set_audio_processing`](Self::set_audio_processing).
    pub fn audio_processing(&self) -> AudioProcessing {
        self.processing.get()
    }

    /// Whether the underlying GATT connection is still up.
    pub async fn is_connected(&self) -> bool {
        self.peripheral.is_connected().await.unwrap_or(false)
    }

    /// Disconnect from the ring and stop the dispatcher task.
    ///
    /// Takes `&self` so callers holding the connection behind an `Arc` (or any
    /// shared handle) can still cancel the OS-level BLE link explicitly —
    /// dropping a `RingConnection` only aborts the dispatcher and leaves the
    /// BLE connection up.
    pub async fn disconnect(&self) -> Result<()> {
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
    BatteryStatus,
    Lock,
}

impl OperationCommand {
    const fn as_str(self) -> &'static str {
        match self {
            Self::SampleRate16 => "sample_rate 16",
            Self::BatteryStatus => "BATTERY",
            Self::Lock => "LOCK",
        }
    }
}

/// Processing mode shared between the connection handle and the dispatcher.
#[derive(Debug, Default)]
struct ProcessingSetting(Mutex<AudioProcessing>);

impl ProcessingSetting {
    fn set(&self, processing: AudioProcessing) {
        *self.0.lock().unwrap() = processing;
    }

    fn get(&self) -> AudioProcessing {
        *self.0.lock().unwrap()
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
    processing: Arc<ProcessingSetting>,
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
                state.set_processing(processing.get());
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
    decoder: AudioDecoder,
    /// Between a recording start (`'1'`) and its stop (`'0'` / `MIC_OFF`).
    recording: bool,
    /// `MIC_ON` seen while not recording, waiting for its `'1'`.
    mic_on_pending: bool,
    /// The current recording already got its own `MIC_ON`.
    mic_on_paired: bool,
    click_count: u32,
}

impl DispatchState {
    fn new() -> Self {
        Self::default()
    }

    fn set_processing(&mut self, processing: AudioProcessing) {
        self.decoder.set_processing(processing);
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
        let pcm = self.decoder.decode(value);
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
                self.decoder.reset_segment();
                self.recording = true;
                self.mic_on_paired = std::mem::take(&mut self.mic_on_pending);
                RingEvent::RecordingStarted
            }
            transfer_status::STOP => {
                self.end_recording();
                RingEvent::RecordingStopped
            }
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

        let event = parse_operation(&text);
        match event {
            RingEvent::MicOn => self.handle_mic_on(),
            RingEvent::MicOff => self.end_recording(),
            _ => {}
        }

        DispatchOutput {
            events: vec![event],
            ..DispatchOutput::default()
        }
    }

    /// `MIC_ON` and `'1'` arrive on different characteristics in either
    /// order. The first `MIC_ON` of a recording is its pair and leaves the
    /// decoder alone; another one while recording means the previous stop
    /// was lost and the ring restarted its encoder, so start a new segment.
    fn handle_mic_on(&mut self) {
        if !self.recording {
            self.mic_on_pending = true;
        } else if !self.mic_on_paired {
            self.mic_on_paired = true;
        } else {
            // This MIC_ON belongs to the new recording; its '1' may follow.
            self.decoder.reset_segment();
            self.mic_on_pending = true;
        }
    }

    fn end_recording(&mut self) {
        self.recording = false;
        self.mic_on_pending = false;
        self.mic_on_paired = false;
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
    use wizpr_ring_core::CodecState;

    #[test]
    fn lock_command_matches_mobile_protocol() {
        assert_eq!(OperationCommand::Lock.as_str(), "LOCK");
    }

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
        assert_ne!(state.decoder.codec_state().step_index(), 0);

        let out =
            state.handle_notification(WizprBle::TRANSFER_STATUS_CHAR, &[transfer_status::START], 2);

        assert_eq!(out.events, vec![RingEvent::RecordingStarted]);
        assert_eq!(state.decoder.codec_state().step_index(), 0);
        assert_eq!(state.decoder.codec_state().predicted_sample(), 0);
    }

    /// Normal recording start: `MIC_ON`, then `'1'`, then audio.
    fn start_recording_with_audio(state: &mut DispatchState) {
        state.handle_notification(WizprBle::OPERATION_CHAR, b"MIC_ON", 1);
        state.handle_notification(WizprBle::TRANSFER_STATUS_CHAR, &[transfer_status::START], 1);
        state.handle_notification(WizprBle::AUDIO_CHAR, &[0x77; 4], 2);
        assert_ne!(state.decoder.codec_state().step_index(), 0);
    }

    #[test]
    fn mic_on_during_recording_resets_decoder() {
        let mut state = DispatchState::new();
        start_recording_with_audio(&mut state);

        // The stop was lost; a new recording starts with MIC_ON.
        let out = state.handle_notification(WizprBle::OPERATION_CHAR, b"MIC_ON", 3);

        assert_eq!(out.events, vec![RingEvent::MicOn]);
        assert_eq!(state.decoder.codec_state().step_index(), 0);
        assert_eq!(state.decoder.codec_state().predicted_sample(), 0);
    }

    #[test]
    fn mic_on_arriving_after_its_start_keeps_decoder() {
        let mut state = DispatchState::new();
        state.handle_notification(WizprBle::TRANSFER_STATUS_CHAR, &[transfer_status::START], 1);
        state.handle_notification(WizprBle::AUDIO_CHAR, &[0x77; 4], 2);
        let before = state.decoder.codec_state();

        state.handle_notification(WizprBle::OPERATION_CHAR, b"MIC_ON", 3);
        assert_eq!(state.decoder.codec_state(), before);

        // A second MIC_ON in the same recording is a new encoder run.
        state.handle_notification(WizprBle::OPERATION_CHAR, b"MIC_ON", 4);
        assert_eq!(state.decoder.codec_state().step_index(), 0);
    }

    #[test]
    fn mic_on_after_recording_stopped_keeps_decoder() {
        for stop in [&b"MIC_OFF"[..], &[transfer_status::STOP][..]] {
            let mut state = DispatchState::new();
            start_recording_with_audio(&mut state);
            let uuid = if stop == b"MIC_OFF" {
                WizprBle::OPERATION_CHAR
            } else {
                WizprBle::TRANSFER_STATUS_CHAR
            };
            state.handle_notification(uuid, stop, 3);
            let before = state.decoder.codec_state();

            state.handle_notification(WizprBle::OPERATION_CHAR, b"MIC_ON", 4);

            assert_eq!(state.decoder.codec_state(), before);
        }
    }

    #[test]
    fn lost_stops_in_a_row_each_reset_on_mic_on() {
        let mut state = DispatchState::new();
        start_recording_with_audio(&mut state);

        for t in 0..3 {
            // Stop lost; next recording sends MIC_ON then '1'.
            state.handle_notification(WizprBle::OPERATION_CHAR, b"MIC_ON", 10 + t);
            assert_eq!(state.decoder.codec_state(), CodecState::new());
            state.handle_notification(
                WizprBle::TRANSFER_STATUS_CHAR,
                &[transfer_status::START],
                10 + t,
            );
            state.handle_notification(WizprBle::AUDIO_CHAR, &[0x77; 4], 10 + t);
            assert_ne!(state.decoder.codec_state(), CodecState::new());
        }

        // Stop and '1' both lost: MIC_ON alone must still reset.
        state.handle_notification(WizprBle::OPERATION_CHAR, b"MIC_ON", 20);
        assert_eq!(state.decoder.codec_state(), CodecState::new());
    }

    /// Raw output of normal recordings, with MIC_ON before or after '1',
    /// must equal a plain decode reset at each '1'.
    #[test]
    fn raw_dispatch_output_matches_plain_decode() {
        let packets: Vec<Vec<u8>> = (0u8..40)
            .map(|i| {
                (0..112)
                    .map(|j: u8| i.wrapping_mul(31).wrapping_add(j.wrapping_mul(7)))
                    .collect()
            })
            .collect();
        let start = (WizprBle::TRANSFER_STATUS_CHAR, vec![transfer_status::START]);
        let stop = (WizprBle::TRANSFER_STATUS_CHAR, vec![transfer_status::STOP]);
        let mic_on = (WizprBle::OPERATION_CHAR, b"MIC_ON".to_vec());
        let mic_off = (WizprBle::OPERATION_CHAR, b"MIC_OFF".to_vec());
        let audio = |p: &Vec<u8>| (WizprBle::AUDIO_CHAR, p.clone());

        let mut script = Vec::new();
        let mut expected = Vec::new();
        for (rec, chunk) in packets.chunks(10).enumerate() {
            if rec % 2 == 0 {
                script.extend([mic_on.clone(), start.clone()]);
                script.extend(chunk.iter().map(audio));
            } else {
                // MIC_ON arrives after '1' and the first audio packets.
                script.push(start.clone());
                script.extend(chunk[..3].iter().map(audio));
                script.push(mic_on.clone());
                script.extend(chunk[3..].iter().map(audio));
            }
            script.extend([mic_off.clone(), stop.clone()]);

            let mut reference = CodecState::new();
            for p in chunk {
                expected.extend(wizpr_ring_core::codec::decode(p, &mut reference));
            }
        }

        let mut state = DispatchState::new();
        let got: Vec<i16> = script
            .iter()
            .filter_map(|(uuid, value)| state.handle_notification(*uuid, value, 0).audio)
            .flat_map(|chunk| chunk.pcm_samples)
            .collect();

        let to_bytes = |s: &[i16]| s.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
        assert_eq!(to_bytes(&got), to_bytes(&expected));
    }

    #[test]
    fn mic_pre_on_is_an_event_and_keeps_decoder() {
        let mut state = DispatchState::new();
        start_recording_with_audio(&mut state);
        let before = state.decoder.codec_state();

        let out = state.handle_notification(WizprBle::OPERATION_CHAR, b"MIC_PRE_ON", 3);

        assert_eq!(out.events, vec![RingEvent::MicPreOn]);
        assert_eq!(state.decoder.codec_state(), before);
    }

    #[test]
    fn processing_switches_at_next_recording_start() {
        let mut state = DispatchState::new();
        start_recording_with_audio(&mut state);

        state.set_processing(AudioProcessing::Clean);
        let raw = state.handle_notification(WizprBle::AUDIO_CHAR, &[0x77; 4], 3);
        assert!(raw.audio.unwrap().pcm_samples.iter().any(|&s| s != 0));

        state.handle_notification(WizprBle::TRANSFER_STATUS_CHAR, &[transfer_status::START], 4);
        let clean = state.handle_notification(WizprBle::AUDIO_CHAR, &[0x77; 4], 5);
        assert!(clean.audio.unwrap().pcm_samples.iter().all(|&s| s == 0));
    }

    #[test]
    fn processing_setting_round_trips() {
        let setting = ProcessingSetting::default();
        assert_eq!(setting.get(), AudioProcessing::Raw);
        setting.set(AudioProcessing::Clean);
        assert_eq!(setting.get(), AudioProcessing::Clean);
        setting.set(AudioProcessing::Raw);
        assert_eq!(setting.get(), AudioProcessing::Raw);
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
    fn operation_commands_stay_allowlisted() {
        assert_eq!(OperationCommand::SampleRate16.as_str(), "sample_rate 16");
        assert_eq!(OperationCommand::BatteryStatus.as_str(), "BATTERY");
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
