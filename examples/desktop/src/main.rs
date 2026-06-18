//! Desktop diagnostic demo: scan for a WIZPR Ring, log every event, and
//! save each recording (RecordingStarted → RecordingStopped) as a WAV file.

use std::error::Error;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use tokio::time::{Instant, MissedTickBehavior};
use wizpr_ring::core::{write_wav, SAMPLE_RATE_HZ};
use wizpr_ring::{RingConnection, RingDevice, RingEvent, RingScanner};

const DEFAULT_RECORDING_DRAIN_MS: u64 = 500;
const DEFAULT_WAV_GAIN: f32 = 3.0;
const CONNECTION_CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const RECORDING_PROGRESS_FLUSH_INTERVAL: Duration = Duration::from_millis(250);
const SLEEP_PARK: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Parser)]
#[command(author, version, about = "WIZPR Ring desktop validation CLI")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List WIZPR Ring candidates discovered during scanning.
    List(ListArgs),

    /// Scan, connect, log ring events, and save recordings as WAV files.
    Listen(ListenArgs),
}

#[derive(Debug, Clone, Parser)]
struct ListArgs {
    /// Scan timeout in seconds.
    #[arg(long, default_value_t = 20)]
    scan_timeout: u64,
}

#[derive(Debug, Clone, Parser)]
struct ListenArgs {
    /// Directory where WAV recordings will be written.
    #[arg(short, long, default_value = ".")]
    output_dir: PathBuf,

    /// Connect to a specific device id or address from the list command.
    #[arg(long)]
    device_id: Option<String>,

    /// Scan timeout in seconds.
    #[arg(long, default_value_t = 20, hide = true)]
    scan_timeout: u64,

    /// Time to keep collecting audio after a stop event.
    #[arg(long, default_value_t = DEFAULT_RECORDING_DRAIN_MS, hide = true)]
    recording_drain_ms: u64,

    /// Write per-audio-chunk diagnostics to audio_chunks.jsonl.
    #[arg(long)]
    log_audio_chunks: bool,

    /// Gain applied only when writing WAV files.
    #[arg(long, default_value_t = DEFAULT_WAV_GAIN)]
    wav_gain: f32,
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        print_error(err.as_ref());
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let command = cli
        .command
        .unwrap_or_else(|| Command::Listen(ListenArgs::parse_from(["listen"])));

    match command {
        Command::List(args) => list_devices(args).await,
        Command::Listen(args) => listen(args).await,
    }
}

async fn list_devices(args: ListArgs) -> Result<(), Box<dyn Error>> {
    println!("Initializing Bluetooth adapter...");
    flush_stdout()?;
    let scanner = RingScanner::new().await?;

    let devices = scan_with_progress(&scanner, Duration::from_secs(args.scan_timeout)).await?;
    if devices.is_empty() {
        println!("No WIZPR Ring candidates found.");
        return Ok(());
    }

    println!("Use `listen --device-id <id>` to connect to a specific candidate.");
    for (idx, device) in devices.iter().enumerate() {
        print_device(idx + 1, device);
    }
    Ok(())
}

async fn listen(args: ListenArgs) -> Result<(), Box<dyn Error>> {
    if !args.wav_gain.is_finite() || args.wav_gain <= 0.0 {
        return Err("--wav-gain must be a finite positive number".into());
    }

    fs::create_dir_all(&args.output_dir)?;
    let session_dir = create_session_dir(&args.output_dir)?;
    let mut event_log = JsonlLog::create(session_dir.join("events.jsonl"))?;
    let mut operation_log = BufWriter::new(File::create(session_dir.join("operations.log"))?);
    let mut audio_chunk_log = if args.log_audio_chunks {
        Some(JsonlLog::create_buffered(
            session_dir.join("audio_chunks.jsonl"),
            Duration::from_secs(1),
        )?)
    } else {
        None
    };
    println!("Saving session to {}", session_dir.display());
    flush_stdout()?;

    println!("Initializing Bluetooth adapter...");
    flush_stdout()?;
    event_log.write("adapter_initializing", [])?;
    let scanner = RingScanner::new().await?;

    let mut scan_fields = vec![("timeout_seconds", args.scan_timeout.to_string())];
    if let Some(device_id) = args.device_id.as_deref() {
        scan_fields.push(("device_id", json_string(device_id)));
    }
    event_log.write("scan_started", scan_fields)?;
    let conn = connect_with_progress(
        &scanner,
        Duration::from_secs(args.scan_timeout),
        args.device_id.as_deref(),
    )
    .await?;
    flush_stdout()?;
    event_log.write("connected", [])?;

    let mut audio = conn.audio()?;
    let mut events = conn.events()?;
    println!("Audio and event receivers are ready.");
    flush_stdout()?;
    event_log.write("receivers_ready", [])?;

    let mut recording: Option<ActiveRecording> = None;
    let mut total_chunks: u64 = 0;
    let mut total_samples: u64 = 0;
    let mut recordings_saved: u64 = 0;
    let mut recording_index: u64 = 0;
    let mut stop_pending = false;
    let mut recording_progress_open = false;
    let mut last_progress_flush = Instant::now();
    let recording_drain_timeout = Duration::from_millis(args.recording_drain_ms);

    let recording_drain_sleep = tokio::time::sleep(SLEEP_PARK);
    tokio::pin!(recording_drain_sleep);

    let connection_check = tokio::time::interval(Duration::from_secs(1));
    tokio::pin!(connection_check);

    let disconnect_reason = 'listen_loop: loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                finish_progress_line(&mut recording_progress_open);
                println!("→ interrupt received, disconnecting.");
                event_log.write("interrupt", [])?;
                break 'listen_loop "interrupt";
            }
            _ = connection_check.tick() => {
                match tokio::time::timeout(CONNECTION_CHECK_TIMEOUT, conn.is_connected()).await {
                    Ok(true) => {}
                    Ok(false) => {
                        finish_progress_line(&mut recording_progress_open);
                        println!("→ connection lost.");
                        event_log.write("connection_lost", [])?;
                        break 'listen_loop "connection_lost";
                    }
                    Err(_) => {
                        finish_progress_line(&mut recording_progress_open);
                        println!("→ connection check timed out, disconnecting.");
                        event_log.write(
                            "connection_check_timeout",
                            [(
                                "timeout_ms",
                                CONNECTION_CHECK_TIMEOUT.as_millis().to_string(),
                            )],
                        )?;
                        break 'listen_loop "connection_check_timeout";
                    }
                }
            }
            chunk = audio.recv() => {
                let Some(chunk) = chunk else {
                    finish_progress_line(&mut recording_progress_open);
                    println!("→ audio stream closed, disconnecting.");
                    event_log.write("audio_stream_closed", [])?;
                    break 'listen_loop "audio_stream_closed";
                };
                total_chunks += 1;
                total_samples += chunk.pcm_samples.len() as u64;
                let recording_state = if stop_pending {
                    "draining"
                } else if recording.is_some() {
                    "recording"
                } else {
                    "idle"
                };
                if let Some(log) = audio_chunk_log.as_mut() {
                    log.write(
                        "audio_chunk",
                        [
                            ("index", total_chunks.to_string()),
                            ("timestamp_ms", chunk.timestamp_ms.to_string()),
                            ("samples", chunk.pcm_samples.len().to_string()),
                            ("state", json_string(recording_state)),
                        ],
                    )?;
                }
                if let Some(recording) = recording.as_mut() {
                    recording.observe_chunk(chunk.timestamp_ms);
                    recording.chunks += 1;
                    recording.samples.extend_from_slice(&chunk.pcm_samples);
                    print!("+");
                    if last_progress_flush.elapsed() >= RECORDING_PROGRESS_FLUSH_INTERVAL {
                        flush_stdout()?;
                        last_progress_flush = Instant::now();
                    }
                    recording_progress_open = true;
                    if stop_pending {
                        recording_drain_sleep
                            .as_mut()
                            .reset(Instant::now() + recording_drain_timeout);
                    }
                }
            }
            evt = events.recv() => {
                let Some(evt) = evt else {
                    finish_progress_line(&mut recording_progress_open);
                    println!("→ event stream closed, disconnecting.");
                    event_log.write("event_stream_closed", [])?;
                    break 'listen_loop "event_stream_closed";
                };
                match evt {
                    RingEvent::MicOn => {
                        finish_progress_line(&mut recording_progress_open);
                        println!("→ MicOn");
                        event_log.write("mic_on", [])?;
                    }
                    RingEvent::MicOff => {
                        finish_progress_line(&mut recording_progress_open);
                        println!("→ MicOff");
                        event_log.write("mic_off", [])?;
                    }
                    RingEvent::PowerOff => {
                        finish_progress_line(&mut recording_progress_open);
                        println!("→ PowerOff, disconnecting.");
                        event_log.write("power_off", [])?;
                        break 'listen_loop "power_off";
                    }
                    RingEvent::RecordingStarted => {
                        finish_progress_line(&mut recording_progress_open);
                        print!("→ recording started ");
                        flush_stdout()?;
                        last_progress_flush = Instant::now();
                        recording_progress_open = true;
                        event_log.write("recording_started", [])?;
                        recording = Some(ActiveRecording::new(unix_millis()));
                        stop_pending = false;
                        recording_drain_sleep.as_mut().reset(Instant::now() + SLEEP_PARK);
                    }
                    RingEvent::RecordingStopped => {
                        finish_progress_line(&mut recording_progress_open);
                        if recording.is_none() {
                            println!("→ recording stopped without start, ignored");
                            event_log.write("recording_stop_without_start", [])?;
                            continue;
                        }
                        println!(
                            "→ recording stop received, draining audio for {}ms",
                            recording_drain_timeout.as_millis()
                        );
                        event_log.write(
                            "recording_stop_received",
                            [
                                (
                                    "drain_timeout_ms",
                                    recording_drain_timeout.as_millis().to_string(),
                                ),
                                (
                                    "chunks_so_far",
                                    recording.as_ref().map(|r| r.chunks).unwrap_or(0).to_string(),
                                ),
                                (
                                    "samples_so_far",
                                    recording
                                        .as_ref()
                                        .map(|r| r.samples.len())
                                        .unwrap_or(0)
                                        .to_string(),
                                ),
                            ],
                        )?;
                        if let Some(recording) = recording.as_mut() {
                            recording.stop_received_timestamp_ms = Some(unix_millis());
                        }
                        stop_pending = true;
                        recording_drain_sleep
                            .as_mut()
                            .reset(Instant::now() + recording_drain_timeout);
                    }
                    RingEvent::BatteryUpdate { voltage, level } => {
                        finish_progress_line(&mut recording_progress_open);
                        println!("→ battery {:.2}V ({}%)", voltage, level);
                        event_log.write(
                            "battery",
                            [
                                ("voltage", format!("{voltage:.3}")),
                                ("level", level.to_string()),
                            ],
                        )?;
                    }
                    RingEvent::Operation { raw } => {
                        finish_progress_line(&mut recording_progress_open);
                        println!("→ operation event {:?}", raw);
                        writeln!(operation_log, "{raw:?}")?;
                        operation_log.flush()?;
                        event_log.write("operation_event", [("event", json_string(&raw))])?;
                    }
                    other => {
                        finish_progress_line(&mut recording_progress_open);
                        println!("→ {:?}", other);
                        event_log.write("event", [("name", json_string(&format!("{other:?}")))])?;
                    }
                }
            }
            _ = &mut recording_drain_sleep, if stop_pending => {
                let Some(active) = recording.take() else {
                    stop_pending = false;
                    recording_drain_sleep.as_mut().reset(Instant::now() + SLEEP_PARK);
                    continue;
                };
                recording_index += 1;
                recordings_saved += 1;
                save_recording(
                    &session_dir,
                    &mut event_log,
                    recording_index,
                    active,
                    args.wav_gain,
                )?;
                stop_pending = false;
                recording_drain_sleep.as_mut().reset(Instant::now() + SLEEP_PARK);
            }
        }
    };

    finish_progress_line(&mut recording_progress_open);
    if let Some(mut active) = recording.take() {
        if !active.samples.is_empty() {
            if active.stop_received_timestamp_ms.is_none() {
                active.stop_received_timestamp_ms = Some(unix_millis());
            }
            event_log.write(
                "recording_interrupted",
                [("reason", json_string(disconnect_reason))],
            )?;
            recording_index += 1;
            recordings_saved += 1;
            save_recording(
                &session_dir,
                &mut event_log,
                recording_index,
                active,
                args.wav_gain,
            )?;
        }
    }
    let disconnect_error = match tokio::time::timeout(DISCONNECT_TIMEOUT, conn.disconnect()).await {
        Ok(Ok(())) => None,
        Ok(Err(err)) => Some(err.to_string()),
        Err(_) => Some(format!(
            "disconnect request timed out after {}ms",
            DISCONNECT_TIMEOUT.as_millis()
        )),
    };
    let mut disconnected_fields = vec![
        ("reason", json_string(disconnect_reason)),
        ("audio_chunks", total_chunks.to_string()),
        ("audio_samples", total_samples.to_string()),
        ("recordings_saved", recordings_saved.to_string()),
    ];
    if let Some(err) = disconnect_error.as_deref() {
        disconnected_fields.push(("disconnect_error", json_string(err)));
    }
    event_log.write("disconnected", disconnected_fields)?;
    if let Some(log) = audio_chunk_log.as_mut() {
        log.flush()?;
    }
    println!(
        "Disconnected ({disconnect_reason}). Received {} audio chunks, {} samples, saved {} recordings.",
        total_chunks, total_samples, recordings_saved
    );
    if let Some(err) = disconnect_error {
        println!("→ disconnect request returned error: {err}");
    }
    Ok(())
}

async fn connect_with_progress(
    scanner: &RingScanner,
    timeout: Duration,
    device_id: Option<&str>,
) -> Result<RingConnection, Box<dyn Error>> {
    if let Some(device_id) = device_id {
        let Some(device) = scan_for_device_with_progress(scanner, timeout, device_id).await? else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no WIZPR Ring matching device id or address `{device_id}` was found"),
            )
            .into());
        };
        return connect_device_with_progress(device).await;
    }

    connect_first_with_progress(scanner, timeout).await
}

async fn scan_with_progress(
    scanner: &RingScanner,
    timeout: Duration,
) -> Result<Vec<RingDevice>, Box<dyn Error>> {
    print!("Scanning for WIZPR Rings (up to {}s)", timeout.as_secs());
    flush_stdout()?;

    let mut scan = Box::pin(scanner.scan(timeout));
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    tick.tick().await;

    loop {
        tokio::select! {
            devices = &mut scan => {
                match devices {
                    Ok(devices) => {
                        println!(" found {}.", devices.len());
                        return Ok(devices);
                    }
                    Err(err) => {
                        println!();
                        return Err(Box::new(err));
                    }
                }
            }
            _ = tick.tick() => {
                print!(".");
                flush_stdout()?;
            }
        }
    }
}

async fn scan_for_device_with_progress(
    scanner: &RingScanner,
    timeout: Duration,
    device_id: &str,
) -> Result<Option<RingDevice>, Box<dyn Error>> {
    print!(
        "Scanning for WIZPR Ring {} (up to {}s)",
        device_id,
        timeout.as_secs()
    );
    flush_stdout()?;

    let mut scan =
        Box::pin(scanner.scan_until(timeout, |device| matches_device_selector(device, device_id)));
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    tick.tick().await;

    loop {
        tokio::select! {
            device = &mut scan => {
                match device {
                    Ok(Some(device)) => {
                        println!(" found.");
                        return Ok(Some(device));
                    }
                    Ok(None) => {
                        println!(" not found.");
                        return Ok(None);
                    }
                    Err(err) => {
                        println!();
                        return Err(Box::new(err));
                    }
                }
            }
            _ = tick.tick() => {
                print!(".");
                flush_stdout()?;
            }
        }
    }
}

async fn connect_first_with_progress(
    scanner: &RingScanner,
    timeout: Duration,
) -> Result<RingConnection, Box<dyn Error>> {
    print!("Scanning for WIZPR Ring (up to {}s)", timeout.as_secs());
    flush_stdout()?;

    let mut connect = Box::pin(scanner.connect_first(timeout));
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    tick.tick().await;

    loop {
        tokio::select! {
            conn = &mut connect => {
                match conn {
                    Ok(conn) => {
                        println!(" connected.");
                        return Ok(conn);
                    }
                    Err(err) => {
                        println!();
                        return Err(Box::new(err));
                    }
                }
            }
            _ = tick.tick() => {
                print!(".");
                flush_stdout()?;
            }
        }
    }
}

async fn connect_device_with_progress(
    device: RingDevice,
) -> Result<RingConnection, Box<dyn Error>> {
    let label = device_label(&device);
    print!("Connecting to {label}");
    flush_stdout()?;

    let mut connect = Box::pin(device.connect());
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    tick.tick().await;

    loop {
        tokio::select! {
            conn = &mut connect => {
                match conn {
                    Ok(conn) => {
                        println!(" connected.");
                        return Ok(conn);
                    }
                    Err(err) => {
                        println!();
                        return Err(Box::new(err));
                    }
                }
            }
            _ = tick.tick() => {
                print!(".");
                flush_stdout()?;
            }
        }
    }
}

fn print_device(index: usize, device: &RingDevice) {
    let rssi = device
        .rssi()
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_string());
    let service = if device.advertises_wizpr_service() {
        "yes"
    } else {
        "no"
    };

    println!(
        "{index}. id={} name={} address={} rssi={} wizpr_service={}",
        device.id(),
        device.name().unwrap_or("-"),
        device.address(),
        rssi,
        service
    );
}

fn matches_device_selector(device: &RingDevice, selector: &str) -> bool {
    device.id().eq_ignore_ascii_case(selector) || device.address().eq_ignore_ascii_case(selector)
}

fn device_label(device: &RingDevice) -> String {
    match device.name() {
        Some(name) => format!("{name} ({})", device.id()),
        None => device.id(),
    }
}

fn finish_progress_line(open: &mut bool) {
    if *open {
        println!();
        *open = false;
    }
}

fn save_recording(
    session_dir: &Path,
    event_log: &mut JsonlLog,
    recording_index: u64,
    recording: ActiveRecording,
    wav_gain: f32,
) -> Result<(), Box<dyn Error>> {
    let filename = format!("recording-{recording_index:03}.wav");
    let path = session_dir.join(&filename);
    let mut w = BufWriter::new(File::create(&path)?);
    let (wav_samples, gain_stats) = apply_wav_gain(&recording.samples, wav_gain);
    write_wav(&mut w, &wav_samples, SAMPLE_RATE_HZ)?;
    let seconds = recording.samples.len() as f32 / SAMPLE_RATE_HZ as f32;
    event_log.write(
        "recording_saved",
        [
            ("file", json_string(&filename)),
            ("chunks", recording.chunks.to_string()),
            ("samples", recording.samples.len().to_string()),
            ("duration_seconds", format!("{seconds:.3}")),
            (
                "start_to_stop_ms",
                recording
                    .stop_received_timestamp_ms
                    .map(|stop| stop.saturating_sub(recording.start_timestamp_ms))
                    .unwrap_or(0)
                    .to_string(),
            ),
            (
                "first_to_last_audio_ms",
                recording
                    .first_audio_timestamp_ms
                    .zip(recording.last_audio_timestamp_ms)
                    .map(|(first, last)| last.saturating_sub(first))
                    .unwrap_or(0)
                    .to_string(),
            ),
            (
                "tail_gap_ms",
                recording
                    .stop_received_timestamp_ms
                    .zip(recording.last_audio_timestamp_ms)
                    .map(|(stop, last)| stop.saturating_sub(u128::from(last)))
                    .unwrap_or(0)
                    .to_string(),
            ),
            (
                "max_audio_gap_ms",
                recording.max_audio_gap_ms.unwrap_or(0).to_string(),
            ),
            ("wav_gain", format!("{wav_gain:.3}")),
            ("raw_peak", gain_stats.raw_peak.to_string()),
            ("wav_peak", gain_stats.wav_peak.to_string()),
            ("clipped_samples", gain_stats.clipped_samples.to_string()),
        ],
    )?;
    println!(
        "→ recording saved {} ({} chunks, {} samples, {:.2}s, gain {:.1}x, {} clipped)",
        path.display(),
        recording.chunks,
        recording.samples.len(),
        seconds,
        wav_gain,
        gain_stats.clipped_samples
    );
    Ok(())
}

#[derive(Debug, Clone, Copy, Default)]
struct GainStats {
    raw_peak: i32,
    wav_peak: i32,
    clipped_samples: u64,
}

fn apply_wav_gain(samples: &[i16], gain: f32) -> (Vec<i16>, GainStats) {
    let mut stats = GainStats::default();
    let mut out = Vec::with_capacity(samples.len());

    for &sample in samples {
        let raw = i32::from(sample);
        stats.raw_peak = stats.raw_peak.max(raw.abs());

        let scaled = (f32::from(sample) * gain).round();
        let clamped = scaled.clamp(f32::from(i16::MIN), f32::from(i16::MAX));
        if scaled != clamped {
            stats.clipped_samples += 1;
        }

        let wav_sample = clamped as i16;
        stats.wav_peak = stats.wav_peak.max(i32::from(wav_sample).abs());
        out.push(wav_sample);
    }

    (out, stats)
}

#[derive(Debug, Default)]
struct ActiveRecording {
    chunks: u64,
    samples: Vec<i16>,
    start_timestamp_ms: u128,
    stop_received_timestamp_ms: Option<u128>,
    first_audio_timestamp_ms: Option<u64>,
    last_audio_timestamp_ms: Option<u64>,
    max_audio_gap_ms: Option<u64>,
}

impl ActiveRecording {
    fn new(start_timestamp_ms: u128) -> Self {
        Self {
            start_timestamp_ms,
            ..Self::default()
        }
    }

    fn observe_chunk(&mut self, timestamp_ms: u64) {
        if self.first_audio_timestamp_ms.is_none() {
            self.first_audio_timestamp_ms = Some(timestamp_ms);
        }
        if let Some(last) = self.last_audio_timestamp_ms {
            let gap = timestamp_ms.saturating_sub(last);
            self.max_audio_gap_ms = Some(self.max_audio_gap_ms.unwrap_or(0).max(gap));
        }
        self.last_audio_timestamp_ms = Some(timestamp_ms);
    }
}

struct JsonlLog {
    writer: BufWriter<File>,
    flush_interval: Option<Duration>,
    last_flush: Instant,
}

impl JsonlLog {
    fn create(path: PathBuf) -> Result<Self, Box<dyn Error>> {
        Self::new(path, None)
    }

    fn create_buffered(path: PathBuf, flush_interval: Duration) -> Result<Self, Box<dyn Error>> {
        Self::new(path, Some(flush_interval))
    }

    fn new(path: PathBuf, flush_interval: Option<Duration>) -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            writer: BufWriter::new(File::create(path)?),
            flush_interval,
            last_flush: Instant::now(),
        })
    }

    fn write<'a, I>(&mut self, event: &str, fields: I) -> Result<(), Box<dyn Error>>
    where
        I: IntoIterator<Item = (&'a str, String)>,
    {
        write!(
            self.writer,
            "{{\"timestamp_ms\":{},\"event\":{}",
            unix_millis(),
            json_string(event)
        )?;
        for (key, value) in fields {
            write!(self.writer, ",\"{key}\":{value}")?;
        }
        writeln!(self.writer, "}}")?;
        match self.flush_interval {
            Some(interval) if self.last_flush.elapsed() >= interval => self.flush()?,
            None => self.flush()?,
            Some(_) => {}
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Box<dyn Error>> {
        self.writer.flush()?;
        self.last_flush = Instant::now();
        Ok(())
    }
}

fn create_session_dir(output_dir: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let base_name = format!("session-{}", unix_secs());
    for suffix in 0..100 {
        let name = if suffix == 0 {
            base_name.clone()
        } else {
            format!("{base_name}-{suffix}")
        };
        let path = output_dir.join(name);
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(Box::new(err)),
        }
    }

    Err("failed to create unique session directory".into())
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn flush_stdout() -> Result<(), Box<dyn Error>> {
    io::stdout().flush()?;
    Ok(())
}

fn print_error(err: &(dyn Error + 'static)) {
    eprintln!("wizpr-ring-desktop failed: {err}");

    let mut source = err.source();
    while let Some(err) = source {
        eprintln!("  caused by: {err}");
        source = err.source();
    }

    if let Some(err) = err.downcast_ref::<wizpr_ring::Error>() {
        if err.is_bluetooth_permission_denied() {
            print_bluetooth_permission_help();
        }
    }
}

fn print_bluetooth_permission_help() {
    eprintln!();
    eprintln!("Bluetooth permission was denied by the operating system.");

    #[cfg(target_os = "macos")]
    {
        eprintln!(
            "On macOS, grant Bluetooth access to the terminal app you used to run this command:"
        );
        eprintln!("  System Settings > Privacy & Security > Bluetooth");
        eprintln!(
            "Then enable Terminal, iTerm, VS Code, or your current shell host and run again."
        );
        eprintln!("If it is already listed, toggle it off/on or restart the terminal app.");
    }

    #[cfg(not(target_os = "macos"))]
    {
        eprintln!(
            "Check that Bluetooth is enabled and that this process has permission to use it."
        );
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
