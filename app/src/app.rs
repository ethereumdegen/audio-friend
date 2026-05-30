use std::collections::VecDeque;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iced::{daemon, window, Element, Settings, Subscription, Task, Theme};

use crate::activity::{push_activity, ActivityEntry};
use crate::audio::{
    available_input_devices, available_output_devices, download_clip, list_clips, AudioEngine, ClipInfo,
    ClipSource, DeviceOption, DownloadedClip, DEFAULT_DEVICE_NAME,
};
use crate::config::AppConfig;
use crate::ui::{activity_view, clips_view, main_view, settings_view};

pub fn run() -> iced::Result {
    tracing_subscriber::fmt().with_env_filter("info").init();

    daemon(title, update, view)
        .theme(|_, _| Theme::Dark)
        .subscription(subscription)
        .settings(Settings::default())
        .run_with(|| {
            let mut config = AppConfig::load().unwrap_or_default();
            if config.input_device.is_empty() {
                config.input_device = DEFAULT_DEVICE_NAME.into();
            }
            if config.output_device.is_empty() {
                config.output_device = DEFAULT_DEVICE_NAME.into();
            }
            let audio = Arc::new(Mutex::new(AudioEngine::new(config.clone())));
            let (main_window, open_main) = window::open(main_window_settings());

            (
                AudioFriend {
                    enabled: false,
                    amplitude: 0.0,
                    config,
                    status: "Idle".into(),
                    audio,
                    main_window,
                    settings_window: None,
                    activity_window: None,
                    clips_window: None,
                    settings_error: None,
                    input_devices: available_input_devices(),
                    output_devices: available_output_devices(),
                    activity_log: VecDeque::new(),
                    clips: Vec::new(),
                    clips_status: "Not loaded".into(),
                    active_clip: None,
                },
                open_main.discard(),
            )
        })
}

#[derive(Debug, Clone)]
pub enum Message {
    ToggleEnabled,
    Tick,
    OpenSettings,
    CloseSettings,
    OpenActivity,
    CloseActivity,
    OpenClips,
    CloseClips,
    RefreshClips,
    OpenClip(String),
    ClipOpened(Result<DownloadedClip, String>),
    ClipsLoaded(Result<Vec<ClipInfo>, String>),
    WindowCloseRequested(window::Id),
    WindowClosed(window::Id),
    InputDeviceChanged(DeviceOption),
    OutputDeviceChanged(DeviceOption),
    InputThresholdChanged(f32),
    OutputThresholdChanged(f32),
    RingSecondsChanged(String),
    KeepaliveSecondsChanged(String),
    S3UrlChanged(String),
    S3BucketChanged(String),
    S3AccessKeyChanged(String),
    S3SecretKeyChanged(String),
    SaveSettings,
}

pub struct AudioFriend {
    pub enabled: bool,
    pub amplitude: f32,
    pub config: AppConfig,
    pub status: String,
    pub audio: Arc<Mutex<AudioEngine>>,
    pub main_window: window::Id,
    pub settings_window: Option<window::Id>,
    pub activity_window: Option<window::Id>,
    pub clips_window: Option<window::Id>,
    pub settings_error: Option<String>,
    pub input_devices: Vec<DeviceOption>,
    pub output_devices: Vec<DeviceOption>,
    pub activity_log: VecDeque<ActivityEntry>,
    pub clips: Vec<ClipInfo>,
    pub clips_status: String,
    pub active_clip: Option<ClipInfo>,
}

fn title(_state: &AudioFriend, id: window::Id) -> String {
    if Some(id) == _state.settings_window {
        "Audio Friend Settings".into()
    } else if Some(id) == _state.activity_window {
        "Audio Friend Activity".into()
    } else if Some(id) == _state.clips_window {
        "Audio Friend Clips".into()
    } else {
        "Audio Friend".into()
    }
}

fn update(state: &mut AudioFriend, message: Message) -> Task<Message> {
    match message {
        Message::ToggleEnabled => {
            state.enabled = !state.enabled;
            if let Ok(mut audio) = state.audio.lock() {
                if state.enabled {
                    match audio.start() {
                        Ok(()) => {
                            state.status = "Monitoring".into();
                            push_activity(&mut state.activity_log, "Monitoring started");
                        }
                        Err(err) => {
                            state.status = format!("Error: {err}");
                            state.enabled = false;
                            push_activity(&mut state.activity_log, format!("Monitoring failed: {err}"));
                        }
                    }
                } else {
                    audio.stop();
                    state.status = "Stopping after keepalive".into();
                    push_activity(&mut state.activity_log, "Monitoring stopped; waiting for current clip to finish");
                }
            }
            Task::none()
        }
        Message::Tick => {
            if let Ok(mut audio) = state.audio.lock() {
                state.amplitude = audio.current_amplitude();
                for event in audio.take_activity_events() {
                    push_activity(&mut state.activity_log, event);
                }
                if let Err(err) = audio.poll_uploads() {
                    state.status = format!("Upload error: {err}");
                    push_activity(&mut state.activity_log, format!("Upload error: {err}"));
                } else if let Some(status) = audio.status_text() {
                    state.status = status;
                }
                state.active_clip = audio.clip_snapshot();
            }
            Task::none()
        }
        Message::OpenSettings => {
            if state.settings_window.is_some() {
                return Task::none();
            }

            state.settings_error = None;
            state.input_devices = available_input_devices();
            state.output_devices = available_output_devices();
            let (id, task) = window::open(settings_window_settings());
            state.settings_window = Some(id);
            task.discard()
        }
        Message::CloseSettings => {
            if let Some(id) = state.settings_window.take() {
                state.settings_error = None;
                return window::close(id);
            }
            Task::none()
        }
        Message::OpenActivity => {
            if state.activity_window.is_some() {
                return Task::none();
            }
            let (id, task) = window::open(activity_window_settings());
            state.activity_window = Some(id);
            task.discard()
        }
        Message::CloseActivity => {
            if let Some(id) = state.activity_window.take() {
                return window::close(id);
            }
            Task::none()
        }
        Message::OpenClips => {
            if state.clips_window.is_some() {
                return Task::perform(async {}, |_| Message::RefreshClips);
            }
            let (id, task) = window::open(clips_window_settings());
            state.clips_window = Some(id);
            state.clips_status = "Loading clips...".into();
            Task::batch(vec![task.discard(), Task::perform(async {}, |_| Message::RefreshClips)])
        }
        Message::CloseClips => {
            if let Some(id) = state.clips_window.take() {
                return window::close(id);
            }
            Task::none()
        }
        Message::RefreshClips => {
            state.clips_status = "Loading clips...".into();
            let config = state.config.s3.clone();
            Task::perform(
                async move { list_clips(&config).await.map_err(|err| err.to_string()) },
                Message::ClipsLoaded,
            )
        }
        Message::OpenClip(key) => {
            if state
                .active_clip
                .as_ref()
                .map(|clip| clip.key == key && clip.source != ClipSource::Uploaded)
                .unwrap_or(false)
            {
                state.clips_status = "This clip is still local/in memory and cannot be opened yet".into();
                return Task::none();
            }

            state.clips_status = format!("Opening {key}...");
            let config = state.config.s3.clone();
            Task::perform(
                async move { download_clip(&config, &key).await.map_err(|err| err.to_string()) },
                Message::ClipOpened,
            )
        }
        Message::ClipOpened(result) => {
            match result {
                Ok(downloaded) => {
                    state.clips_status = format!("Opened {}", downloaded.key);
                    push_activity(
                        &mut state.activity_log,
                        format!("Downloaded clip to {}", downloaded.file_path.display()),
                    );
                    match open_with_system_player(&downloaded.file_path) {
                        Ok(()) => {
                            push_activity(
                                &mut state.activity_log,
                                format!("Opened clip in system player: {}", downloaded.key),
                            );
                        }
                        Err(err) => {
                            state.clips_status = format!("Downloaded clip, but failed to open: {err}");
                            push_activity(
                                &mut state.activity_log,
                                format!("Clip open failed: {err}"),
                            );
                        }
                    }
                }
                Err(err) => {
                    state.clips_status = format!("Failed to open clip: {err}");
                    push_activity(&mut state.activity_log, format!("Clip open failed: {err}"));
                }
            }
            Task::none()
        }
        Message::ClipsLoaded(result) => {
            match result {
                Ok(clips) => {
                    state.clips_status = format!("Loaded {} uploaded clips", clips.len());
                    state.clips = clips;
                    push_activity(&mut state.activity_log, "Clip history refreshed");
                }
                Err(err) => {
                    state.clips_status = format!("Failed to load clips: {err}");
                    push_activity(&mut state.activity_log, format!("Clip history load failed: {err}"));
                }
            }
            Task::none()
        }
        Message::WindowCloseRequested(id) => {
            if id == state.main_window {
                return iced::exit();
            }

            if Some(id) == state.settings_window {
                state.settings_window = None;
                state.settings_error = None;
                return window::close(id);
            }
            if Some(id) == state.activity_window {
                state.activity_window = None;
                return window::close(id);
            }
            if Some(id) == state.clips_window {
                state.clips_window = None;
                return window::close(id);
            }

            Task::none()
        }
        Message::WindowClosed(id) => {
            if Some(id) == state.settings_window {
                state.settings_window = None;
                state.settings_error = None;
            }
            if Some(id) == state.activity_window {
                state.activity_window = None;
            }
            if Some(id) == state.clips_window {
                state.clips_window = None;
            }
            Task::none()
        }
        Message::InputDeviceChanged(v) => {
            state.config.input_device = v.raw_name;
            Task::none()
        }
        Message::OutputDeviceChanged(v) => {
            state.config.output_device = v.raw_name;
            Task::none()
        }
        Message::InputThresholdChanged(v) => {
            state.config.input_threshold = v;
            Task::none()
        }
        Message::OutputThresholdChanged(v) => {
            state.config.output_threshold = v;
            Task::none()
        }
        Message::RingSecondsChanged(v) => {
            state.config.ring_buffer_secs = v;
            Task::none()
        }
        Message::KeepaliveSecondsChanged(v) => {
            state.config.keepalive_secs = v;
            Task::none()
        }
        Message::S3UrlChanged(v) => {
            state.config.s3.endpoint_url = v;
            Task::none()
        }
        Message::S3BucketChanged(v) => {
            state.config.s3.bucket = v;
            Task::none()
        }
        Message::S3AccessKeyChanged(v) => {
            state.config.s3.access_key = v;
            Task::none()
        }
        Message::S3SecretKeyChanged(v) => {
            state.config.s3.secret_key = v;
            Task::none()
        }
        Message::SaveSettings => {
            if let Some(error) = validate_settings(&state.config) {
                state.settings_error = Some(error);
                return Task::none();
            }

            state.settings_error = None;
            if let Err(err) = state.config.save() {
                state.status = format!("Config save failed: {err}");
                state.settings_error = Some(format!("Save failed: {err}"));
                Task::none()
            } else {
                state.status = "Settings saved".into();
                push_activity(&mut state.activity_log, "Settings saved");
                if let Ok(mut audio) = state.audio.lock() {
                    audio.apply_config(state.config.clone());
                }

                if let Some(id) = state.settings_window.take() {
                    state.settings_error = None;
                    window::close(id)
                } else {
                    Task::none()
                }
            }
        }
    }
}

fn subscription(_state: &AudioFriend) -> Subscription<Message> {
    Subscription::batch(vec![
        iced::time::every(Duration::from_millis(100)).map(|_| Message::Tick),
        window::close_requests().map(Message::WindowCloseRequested),
        window::close_events().map(Message::WindowClosed),
    ])
}

fn validate_settings(config: &AppConfig) -> Option<String> {
    if config.ring_buffer_secs.parse::<usize>().ok().filter(|v| *v > 0).is_none() {
        return Some("Ring buffer seconds must be a whole number greater than 0".into());
    }

    if config.keepalive_secs.parse::<u64>().ok().filter(|v| *v > 0).is_none() {
        return Some("Keepalive seconds must be a whole number greater than 0".into());
    }

    let s3 = &config.s3;
    let configured_fields = [
        !s3.endpoint_url.trim().is_empty(),
        !s3.bucket.trim().is_empty(),
        !s3.access_key.trim().is_empty(),
        !s3.secret_key.trim().is_empty(),
    ]
    .into_iter()
    .filter(|configured| *configured)
    .count();

    if configured_fields > 0 && configured_fields < 4 {
        return Some("Complete all S3 fields or leave all of them blank".into());
    }

    None
}

fn open_with_system_player(path: &std::path::Path) -> Result<(), String> {
    Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|err| err.to_string())
}

fn view(state: &AudioFriend, id: window::Id) -> Element<'_, Message> {
    if Some(id) == state.settings_window {
        settings_view(state)
    } else if Some(id) == state.activity_window {
        activity_view(state)
    } else if Some(id) == state.clips_window {
        clips_view(state)
    } else {
        main_view(state)
    }
}

fn main_window_settings() -> window::Settings {
    window::Settings {
        size: iced::Size::new(360.0, 140.0),
        resizable: false,
        decorations: true,
        ..Default::default()
    }
}

fn settings_window_settings() -> window::Settings {
    window::Settings {
        size: iced::Size::new(700.0, 560.0),
        resizable: true,
        decorations: true,
        ..Default::default()
    }
}

fn activity_window_settings() -> window::Settings {
    window::Settings {
        size: iced::Size::new(640.0, 420.0),
        resizable: true,
        decorations: true,
        ..Default::default()
    }
}

fn clips_window_settings() -> window::Settings {
    window::Settings {
        size: iced::Size::new(760.0, 460.0),
        resizable: true,
        decorations: true,
        ..Default::default()
    }
}
