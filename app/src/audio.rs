use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use s3::bucket::Bucket;
use s3::creds::Credentials;
use s3::region::Region;
use tokio::runtime::Runtime;
use tracing::{error, info, warn};

use crate::config::{AppConfig, S3Config};

pub struct AudioEngine {
    config: AppConfig,
    stream: Option<cpal::Stream>,
    amplitude: Arc<Mutex<f32>>,
    detector: Arc<Mutex<Detector>>,
    upload_runtime: Runtime,
    pending_uploads: Vec<tokio::task::JoinHandle<Result<UploadOutcome>>>,
    last_status: Option<String>,
    activity_events: VecDeque<String>,
    stopping: bool,
}

#[derive(Debug, Clone)]
pub struct ClipInfo {
    pub key: String,
    pub last_modified: String,
    pub last_modified_display: String,
    pub size_bytes: u64,
    pub source: ClipSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipSource {
    InMemoryActive,
    InMemoryPendingUpload,
    Uploaded,
}

#[derive(Debug, Clone)]
pub struct DownloadedClip {
    pub file_path: PathBuf,
    pub key: String,
}

#[derive(Debug)]
struct FinishedClip {
    bytes: Vec<u8>,
    duration: Duration,
    size_bytes: usize,
    captured_at: DateTime<Utc>,
}

#[derive(Debug)]
enum UploadOutcome {
    Uploaded { key: String, size_bytes: usize },
    Skipped { size_bytes: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceOption {
    pub label: String,
    pub raw_name: String,
}

pub const DEFAULT_DEVICE_NAME: &str = "__default__";

impl std::fmt::Display for DeviceOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

impl AudioEngine {
    pub fn new(config: AppConfig) -> Self {
        Self {
            detector: Arc::new(Mutex::new(Detector::new(&config))),
            config,
            stream: None,
            amplitude: Arc::new(Mutex::new(0.0)),
            upload_runtime: Runtime::new().expect("tokio runtime"),
            pending_uploads: Vec::new(),
            last_status: None,
            activity_events: VecDeque::new(),
            stopping: false,
        }
    }

    pub fn apply_config(&mut self, config: AppConfig) {
        self.config = config.clone();
        self.detector = Arc::new(Mutex::new(Detector::new(&config)));
    }

    pub fn start(&mut self) -> Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }

        let host = cpal::default_host();
        let devices: Vec<_> = host.input_devices()?.collect();
        let device = resolve_input_device(&self.config.input_device, &devices)
            .cloned()
            .context("configured input device not found")?;

        let supported = device.default_input_config()?;
        let sample_format = supported.sample_format();
        let sample_rate = supported.sample_rate().0;
        let stream_config: cpal::StreamConfig = supported.into();
        let amplitude = self.amplitude.clone();
        let detector = self.detector.clone();

        if let Ok(mut d) = detector.lock() {
            d.set_sample_rate(sample_rate);
        }

        let err_fn = |err| error!("audio stream error: {err}");

        let stream = match sample_format {
            cpal::SampleFormat::F32 => device.build_input_stream(
                &stream_config,
                move |data: &[f32], _| process_input(data, &amplitude, &detector),
                err_fn,
                None,
            )?,
            cpal::SampleFormat::I16 => device.build_input_stream(
                &stream_config,
                move |data: &[i16], _| {
                    let converted: Vec<f32> = data.iter().map(|s| *s as f32 / i16::MAX as f32).collect();
                    process_input(&converted, &amplitude, &detector)
                },
                err_fn,
                None,
            )?,
            cpal::SampleFormat::U16 => device.build_input_stream(
                &stream_config,
                move |data: &[u16], _| {
                    let converted: Vec<f32> = data.iter().map(|s| *s as f32 / u16::MAX as f32 - 0.5).collect();
                    process_input(&converted, &amplitude, &detector)
                },
                err_fn,
                None,
            )?,
            _ => anyhow::bail!("unsupported sample format"),
        };

        stream.play()?;
        self.stream = Some(stream);
        self.stopping = false;
        self.activity_events.push_back("Monitoring started".into());
        Ok(())
    }

    pub fn stop(&mut self) {
        self.stream = None;
        self.stopping = true;
        self.activity_events
            .push_back("Monitoring stopped; waiting for keepalive to finish current clip".into());
    }

    pub fn current_amplitude(&self) -> f32 {
        self.amplitude.lock().map(|v| *v).unwrap_or(0.0)
    }

    pub fn status_text(&mut self) -> Option<String> {
        self.last_status.take()
    }

    pub fn take_activity_events(&mut self) -> Vec<String> {
        self.activity_events.drain(..).collect()
    }

    pub fn clip_snapshot(&self) -> Option<ClipInfo> {
        let detector = self.detector.lock().ok()?;
        detector.clip_snapshot()
    }

    pub fn has_pending_uploads(&self) -> bool {
        !self.pending_uploads.is_empty()
    }

    pub fn poll_uploads(&mut self) -> Result<()> {
        let detector_events = self
            .detector
            .lock()
            .ok()
            .map(|mut d| {
                if self.stopping {
                    d.process_silence_tick();
                }
                d.take_activity_events()
            })
            .unwrap_or_default();
        for event in detector_events {
            self.activity_events.push_back(event);
        }

        let mut remaining = Vec::new();
        for handle in self.pending_uploads.drain(..) {
            if handle.is_finished() {
                match self.upload_runtime.block_on(handle) {
                    Ok(Ok(UploadOutcome::Uploaded { key, size_bytes })) => {
                        self.last_status = Some("Uploaded clip".into());
                        self.activity_events
                            .push_back(format!("Uploaded clip: {} ({} KB)", key, size_bytes / 1024));
                        if self.stopping && remaining.is_empty() {
                            self.last_status = Some("Idle".into());
                        }
                    }
                    Ok(Ok(UploadOutcome::Skipped { size_bytes })) => {
                        self.last_status = Some("Captured clip locally".into());
                        self.activity_events.push_back(format!(
                            "Captured clip locally; upload skipped ({} KB)",
                            size_bytes / 1024
                        ));
                        if self.stopping && remaining.is_empty() {
                            self.last_status = Some("Idle".into());
                        }
                    }
                    Ok(Err(err)) => return Err(err),
                    Err(err) => return Err(anyhow::anyhow!(err.to_string())),
                }
            } else {
                remaining.push(handle);
            }
        }
        self.pending_uploads = remaining;
        if self.stopping
            && self.pending_uploads.is_empty()
            && self.detector.lock().ok().and_then(|d| d.clip_snapshot()).is_none()
        {
            self.stopping = false;
            self.last_status = Some("Idle".into());
        }

        let maybe_clip = self.detector.lock().ok().and_then(|mut d| d.take_finished_clip());
        if let Some(clip) = maybe_clip {
            self.activity_events.push_back(format!(
                "Clip captured: {:.1}s ({} KB)",
                clip.duration.as_secs_f32(),
                clip.size_bytes / 1024
            ));

            let upload_enabled = !(self.config.s3.endpoint_url.is_empty()
                || self.config.s3.bucket.is_empty()
                || self.config.s3.access_key.is_empty()
                || self.config.s3.secret_key.is_empty());

            if upload_enabled {
                self.last_status = Some("Uploading clip".into());
                self.activity_events.push_back("Uploading clip".into());
            } else {
                self.last_status = Some("Captured clip locally".into());
                self.activity_events
                    .push_back("Upload skipped; S3 not configured".into());
            }

            let cfg = self.config.s3.clone();
            let handle = self.upload_runtime.spawn(async move { upload_clip(cfg, clip).await });
            self.pending_uploads.push(handle);
        }
        Ok(())
    }
}

fn process_input(samples: &[f32], amplitude: &Arc<Mutex<f32>>, detector: &Arc<Mutex<Detector>>) {
    let rms = if samples.is_empty() {
        0.0
    } else {
        let sum = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
        sum.sqrt().clamp(0.0, 1.0)
    };

    if let Ok(mut a) = amplitude.lock() {
        *a = rms;
    }
    if let Ok(mut d) = detector.lock() {
        d.process(samples, rms);
    }
}

struct Detector {
    ring: VecDeque<f32>,
    recording: Option<RecordingSession>,
    finished_clip: Option<FinishedClip>,
    threshold: f32,
    keepalive: Duration,
    max_clip_duration: Duration,
    sample_rate: u32,
    ring_seconds: usize,
    activity_events: VecDeque<String>,
}

impl Detector {
    fn new(config: &AppConfig) -> Self {
        let ring_seconds = config.ring_buffer_secs.parse::<usize>().unwrap_or(3);
        Self {
            ring: VecDeque::new(),
            recording: None,
            finished_clip: None,
            threshold: config.input_threshold,
            keepalive: Duration::from_secs(config.keepalive_secs.parse::<u64>().unwrap_or(10)),
            max_clip_duration: Duration::from_secs(60),
            sample_rate: 48_000,
            ring_seconds,
            activity_events: VecDeque::new(),
        }
    }

    fn set_sample_rate(&mut self, sample_rate: u32) {
        self.sample_rate = sample_rate;
        self.trim_ring();
    }

    fn ring_capacity(&self) -> usize {
        self.sample_rate as usize * self.ring_seconds
    }

    fn trim_ring(&mut self) {
        let capacity = self.ring_capacity();
        while self.ring.len() > capacity {
            self.ring.pop_front();
        }
    }

    fn process(&mut self, samples: &[f32], rms: f32) {
        for sample in samples {
            if self.ring.len() >= self.ring_capacity() {
                self.ring.pop_front();
            }
            self.ring.push_back(*sample);
        }

        let now = Instant::now();
        let active_audio = rms >= self.threshold;
        if rms >= self.threshold {
            if self.recording.is_none() {
                let mut data: Vec<f32> = self.ring.iter().copied().collect();
                data.extend_from_slice(samples);
                self.recording = Some(RecordingSession { data, last_activity: now, started_at: now });
                self.activity_events.push_back("Clip recording started".into());
                info!("clip recording started");
            } else if let Some(rec) = &mut self.recording {
                rec.data.extend_from_slice(samples);
                if active_audio {
                    rec.last_activity = now;
                }
                if now.duration_since(rec.started_at) >= self.max_clip_duration {
                    self.finish_recording(now, "Clip reached 60 second limit; starting a new clip");

                    let mut data: Vec<f32> = self.ring.iter().copied().collect();
                    data.extend_from_slice(samples);
                    self.recording = Some(RecordingSession { data, last_activity: now, started_at: now });
                    self.activity_events.push_back("Clip recording started".into());
                    info!("clip recording restarted after max duration");
                }
            }
        } else if let Some(rec) = &mut self.recording {
            rec.data.extend_from_slice(samples);
            if active_audio {
                rec.last_activity = now;
            }
            if now.duration_since(rec.last_activity) >= self.keepalive {
                self.finish_recording(now, "Clip recording finished");
            }
        }
    }

    fn process_silence_tick(&mut self) {
        if let Some(rec) = &self.recording {
            let now = Instant::now();
            if now.duration_since(rec.last_activity) >= self.keepalive {
                self.finish_recording(now, "Clip recording finished after monitoring stopped");
            }
        }
    }

    fn clip_snapshot(&self) -> Option<ClipInfo> {
        if let Some(rec) = &self.recording {
            let duration = Instant::now().duration_since(rec.started_at);
            let sample_bytes = rec.data.len() as u64 * 2;
            return Some(ClipInfo {
                key: format!("In-memory clip ({:.1}s)", duration.as_secs_f32()),
                last_modified: Utc::now().to_rfc3339(),
                last_modified_display: "Recording now".into(),
                size_bytes: sample_bytes,
                source: ClipSource::InMemoryActive,
            });
        }

        if self.finished_clip.is_some() {
            return Some(ClipInfo {
                key: "Pending upload".into(),
                last_modified: Utc::now().to_rfc3339(),
                last_modified_display: "Waiting to upload".into(),
                size_bytes: self.finished_clip.as_ref().map(|clip| clip.size_bytes as u64).unwrap_or(0),
                source: ClipSource::InMemoryPendingUpload,
            });
        }

        None
    }

    fn finish_recording(&mut self, now: Instant, reason: &str) {
        let Some(rec) = self.recording.take() else {
            return;
        };

        let bytes = encode_wav(&rec.data, self.sample_rate);
        let duration = now.duration_since(rec.started_at);
        let size_bytes = bytes.len();
        self.finished_clip = Some(FinishedClip {
            bytes,
            duration,
            size_bytes,
            captured_at: Utc::now(),
        });
        self.activity_events.push_back(format!(
            "{}: {:.1}s ({} KB)",
            reason,
            duration.as_secs_f32(),
            size_bytes / 1024
        ));
        info!("clip recording finished");
    }

    fn take_finished_clip(&mut self) -> Option<FinishedClip> {
        self.finished_clip.take()
    }

    fn take_activity_events(&mut self) -> Vec<String> {
        self.activity_events.drain(..).collect()
    }
}

struct RecordingSession {
    data: Vec<f32>,
    last_activity: Instant,
    started_at: Instant,
}

fn encode_wav(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
    let data_len = (samples.len() * 2) as u32;
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        let s = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    bytes
}

async fn upload_clip(config: S3Config, clip: FinishedClip) -> Result<UploadOutcome> {
    if config.endpoint_url.is_empty()
        || config.bucket.is_empty()
        || config.access_key.is_empty()
        || config.secret_key.is_empty()
    {
        info!("S3 not configured; skipping upload");
        return Ok(UploadOutcome::Skipped {
            size_bytes: clip.size_bytes,
        });
    }

    let region = Region::Custom {
        region: "us-east-1".into(),
        endpoint: config.endpoint_url.clone(),
    };
    let credentials = Credentials::new(
        Some(&config.access_key),
        Some(&config.secret_key),
        None,
        None,
        None,
    )?;
    let bucket = Bucket::new(&config.bucket, region, credentials)?.with_path_style();
    let key = format!("recordings/{}.wav", clip.captured_at.format("%Y%m%dT%H%M%S%.3fZ"));

    let response = bucket.put_object(key.as_str(), &clip.bytes).await?;
    if !(200..300).contains(&response.status_code()) {
        anyhow::bail!("S3 upload failed with status {}", response.status_code());
    }

    Ok(UploadOutcome::Uploaded {
        key,
        size_bytes: clip.size_bytes,
    })
}

pub async fn download_clip(config: &S3Config, key: &str) -> Result<DownloadedClip> {
    let region = Region::Custom {
        region: "us-east-1".into(),
        endpoint: config.endpoint_url.clone(),
    };
    let credentials = Credentials::new(
        Some(&config.access_key),
        Some(&config.secret_key),
        None,
        None,
        None,
    )?;
    let bucket = Bucket::new(&config.bucket, region, credentials)?.with_path_style();
    let response = bucket.get_object(key).await?;
    if !(200..300).contains(&response.status_code()) {
        anyhow::bail!("S3 download failed with status {}", response.status_code());
    }

    let mut path = std::env::temp_dir();
    let file_name = key.rsplit('/').next().unwrap_or("clip.wav");
    path.push(file_name);
    fs::write(&path, response.bytes())?;

    Ok(DownloadedClip {
        file_path: path,
        key: key.to_string(),
    })
}

pub async fn list_clips(config: &S3Config) -> Result<Vec<ClipInfo>> {
    if config.endpoint_url.is_empty()
        || config.bucket.is_empty()
        || config.access_key.is_empty()
        || config.secret_key.is_empty()
    {
        return Ok(Vec::new());
    }

    let region = Region::Custom {
        region: "us-east-1".into(),
        endpoint: config.endpoint_url.clone(),
    };
    let credentials = Credentials::new(
        Some(&config.access_key),
        Some(&config.secret_key),
        None,
        None,
        None,
    )?;
    let bucket = Bucket::new(&config.bucket, region, credentials)?.with_path_style();
    let pages = bucket.list("recordings/".into(), None).await?;

    let mut clips = pages
        .into_iter()
        .flat_map(|page| page.contents)
        .map(|object| {
            let last_modified = object.last_modified;
            ClipInfo {
                key: object.key,
                last_modified_display: format_clip_timestamp(&last_modified),
                last_modified,
                size_bytes: object.size,
                source: ClipSource::Uploaded,
            }
        })
        .collect::<Vec<_>>();
    clips.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));
    Ok(clips)
}

fn format_clip_timestamp(value: &str) -> String {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Local).format("%b %-d, %Y at %-I:%M:%S %p").to_string())
        .unwrap_or_else(|_| value.to_string())
}

pub fn available_input_devices() -> Vec<DeviceOption> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(devices) => collect_device_options(devices, DeviceKind::Input),
        Err(err) => {
            error!("failed to enumerate input devices: {err}");
            Vec::new()
        }
    }
}

pub fn available_output_devices() -> Vec<DeviceOption> {
    let host = cpal::default_host();
    match host.output_devices() {
        Ok(devices) => collect_device_options(devices, DeviceKind::Output),
        Err(err) => {
            error!("failed to enumerate output devices: {err}");
            Vec::new()
        }
    }
}

pub fn selected_input_device(raw_name: &str, options: &[DeviceOption]) -> Option<DeviceOption> {
    find_selected_device(raw_name, options)
}

pub fn selected_output_device(raw_name: &str, options: &[DeviceOption]) -> Option<DeviceOption> {
    find_selected_device(raw_name, options)
}

#[derive(Clone, Copy)]
enum DeviceKind {
    Input,
    Output,
}

fn collect_device_options(
    devices: impl Iterator<Item = cpal::Device>,
    kind: DeviceKind,
) -> Vec<DeviceOption> {
    let host = cpal::default_host();
    let default_name = match kind {
        DeviceKind::Input => host.default_input_device().and_then(|d| d.name().ok()),
        DeviceKind::Output => host.default_output_device().and_then(|d| d.name().ok()),
    };

    let mut seen = HashSet::new();
    let mut options = vec![DeviceOption {
        label: "Default".into(),
        raw_name: DEFAULT_DEVICE_NAME.into(),
    }];

    for device in devices {
        let Some(raw_name) = valid_device_name(device, kind) else {
            continue;
        };

        if !seen.insert(raw_name.clone()) {
            continue;
        }

        let cleaned = clean_device_label(&raw_name);
        let label = if default_name.as_deref() == Some(raw_name.as_str()) {
            format!("{cleaned} (system default)")
        } else {
            cleaned
        };

        options.push(DeviceOption { label, raw_name });
    }

    if options.len() > 1 {
        options[1..].sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()));
    }
    options
}

fn find_selected_device(raw_name: &str, options: &[DeviceOption]) -> Option<DeviceOption> {
    options.iter().find(|device| device.raw_name == raw_name).cloned()
}

fn valid_device_name(device: cpal::Device, kind: DeviceKind) -> Option<String> {
    let supported = match kind {
        DeviceKind::Input => device.default_input_config().is_ok(),
        DeviceKind::Output => device.default_output_config().is_ok(),
    };

    if supported {
        device.name().ok()
    } else {
        None
    }
}

pub fn resolve_input_device<'a>(configured: &str, devices: &'a [cpal::Device]) -> Option<&'a cpal::Device> {
    if configured.is_empty() || configured == DEFAULT_DEVICE_NAME {
        return devices.first();
    }

    devices
        .iter()
        .find(|device| device.name().map(|name| name == configured).unwrap_or(false))
}

fn clean_device_label(name: &str) -> String {
    let mut cleaned = name.trim().replace("pipewire", "PipeWire").replace("pulseaudio", "PulseAudio");

    for suffix in [" on PipeWire", " on PulseAudio", " (PipeWire)", " (PulseAudio)"] {
        if cleaned.ends_with(suffix) {
            cleaned.truncate(cleaned.len() - suffix.len());
        }
    }

    for prefix in ["alsa_input.", "alsa_output."] {
        if let Some(rest) = cleaned.strip_prefix(prefix) {
            cleaned = rest.replace('.', " ");
        }
    }

    cleaned = cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches('-')
        .trim()
        .to_string();

    if cleaned.is_empty() {
        warn!("device name was empty after cleanup; using raw device label");
        name.to_string()
    } else {
        cleaned
    }
}
