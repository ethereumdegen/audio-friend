use iced::alignment::Alignment;
use iced::border;
use iced::widget::{
    button, column, container, pick_list, progress_bar, row, scrollable, slider, stack, text,
    text_input,
};
use iced::{Background, Color, Element, Length, Theme};

use crate::app::{AudioFriend, Message};
use crate::audio::{selected_input_device, selected_output_device, ClipSource};

pub fn main_view(state: &AudioFriend) -> Element<'_, Message> {
    let toggle_label = if state.enabled { "On" } else { "Off" };
    let threshold_line = ((state.config.input_threshold.clamp(0.0, 1.0)) * 100.0) as u16;

    let content = column![
        row![
            button(text(toggle_label)).on_press(Message::ToggleEnabled),
            button(text("Settings")).on_press(Message::OpenSettings),
            button(text("Activity")).on_press(Message::OpenActivity),
            button(text("Clips")).on_press(Message::OpenClips),
        ]
        .spacing(10),
        stack![
            progress_bar(0.0..=1.0, state.amplitude),
            row![
                container("\u{00a0}").width(Length::FillPortion(threshold_line.max(1))),
                container("\u{00a0}")
                    .width(2)
                    .height(16)
                    .style(|_| iced::widget::container::Style {
                        background: Some(Background::Color(Color::WHITE)),
                        ..Default::default()
                    }),
                container("\u{00a0}").width(Length::FillPortion((100 - threshold_line).max(1))),
            ]
            .height(16),
        ],
        text(&state.status).size(14),
    ]
    .padding(10)
    .spacing(8)
    .align_x(Alignment::Start);

    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

pub fn settings_view(state: &AudioFriend) -> Element<'_, Message> {
    let devices = state.input_devices.clone();
    let output_devices = state.output_devices.clone();

    let selected_input = selected_input_device(&state.config.input_device, &state.input_devices);
    let selected_output = selected_output_device(&state.config.output_device, &state.output_devices);

    let error_label = state.settings_error.as_ref().map(|message| {
        text(message)
            .size(14)
            .style(|_| iced::widget::text::Style {
                color: Some(Color::from_rgb8(0xff, 0x6b, 0x6b)),
            })
    });

    let audio_devices_group = settings_card(column![
        text("Audio devices").size(20),
        section_help("Choose the input and output devices used by the app."),
        text("Input device").size(14),
        pick_list(devices, selected_input, Message::InputDeviceChanged)
            .placeholder("Select input device"),
        text("Output device").size(14),
        pick_list(output_devices, selected_output, Message::OutputDeviceChanged)
            .placeholder("Select output device"),
    ]);

    let detection_group = settings_card(column![
        text("Detection").size(20),
        section_help("Tune how the app detects audio activity and how long it keeps recording."),
        text(format!("Recording threshold ({:.0}%)", state.config.input_threshold * 100.0)).size(14),
        slider(0.0..=1.0, state.config.input_threshold, Message::InputThresholdChanged).step(0.01),
        section_help("Audio above this level starts and keeps a clip alive. Audio below it is treated as silence."),
        text("Output sensitivity").size(14),
        slider(0.0..=1.0, state.config.output_threshold, Message::OutputThresholdChanged).step(0.01),
        text("Ring buffer seconds").size(14),
        text_input("Seconds of audio to keep before trigger", &state.config.ring_buffer_secs)
            .on_input(Message::RingSecondsChanged),
        text("Keepalive seconds").size(14),
        text_input("Seconds to continue after audio drops", &state.config.keepalive_secs)
            .on_input(Message::KeepaliveSecondsChanged),
    ]);

    let s3_group = settings_card(column![
        text("S3 upload").size(20),
        section_help("Leave all S3 fields blank to disable uploads."),
        text("S3 endpoint URL").size(14),
        text_input("https://s3.example.com", &state.config.s3.endpoint_url)
            .on_input(Message::S3UrlChanged),
        text("S3 bucket").size(14),
        text_input("Bucket name", &state.config.s3.bucket).on_input(Message::S3BucketChanged),
        text("S3 access key").size(14),
        text_input("Access key", &state.config.s3.access_key).on_input(Message::S3AccessKeyChanged),
        text("S3 secret key").size(14),
        text_input("Secret key", &state.config.s3.secret_key).on_input(Message::S3SecretKeyChanged),
    ]);

    let content = column![
        text("Settings").size(24),
        audio_devices_group,
        detection_group,
        s3_group,
        error_label.unwrap_or_else(|| text("")),
        row![
            button(text("Save")).on_press(Message::SaveSettings),
            button(text("Close")).on_press(Message::CloseSettings),
        ]
        .spacing(10)
    ]
    .padding(20)
    .spacing(18);

    container(scrollable(content))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

pub fn activity_view(state: &AudioFriend) -> Element<'_, Message> {
    let entries = if state.activity_log.is_empty() {
        column![text("No activity yet.").size(14)]
    } else {
        state.activity_log.iter().fold(column!().spacing(8), |col, entry| {
            col.push(settings_card(column![
                text(format_activity_timestamp(entry.timestamp)).size(13),
                text(&entry.message).size(15),
            ]))
        })
    };

    let content = column![
        row![
            text("Activity").size(24),
            button(text("Close")).on_press(Message::CloseActivity),
        ]
        .spacing(10),
        scrollable(entries),
    ]
    .padding(20)
    .spacing(16);

    container(content).width(Length::Fill).height(Length::Fill).into()
}

pub fn clips_view(state: &AudioFriend) -> Element<'_, Message> {
    let mut entries = column!().spacing(8);

    if let Some(active) = &state.active_clip {
        entries = entries.push(settings_card(column![
            text(&active.key).size(15),
            text(format!("State: {}", clip_source_label(&active.source))).size(13),
            text(format!("Updated: {}", active.last_modified_display)).size(13),
            text(format!("Size: {} KB", active.size_bytes / 1024)).size(13),
        ]));
    }

    if state.clips.is_empty() && state.active_clip.is_none() {
        entries = entries.push(text(&state.clips_status).size(14));
    } else {
        let now_playing = state.player.now_playing();
        for clip in &state.clips {
            let is_playing = now_playing.as_deref() == Some(clip.key.as_str());
            let controls = if is_playing {
                row![
                    button(text("Stop")).on_press(Message::StopPlayback),
                    text("Playing...").size(13),
                ]
                .spacing(10)
                .align_y(Alignment::Center)
            } else {
                row![button(text("Play")).on_press(Message::OpenClip(clip.key.clone()))]
            };

            entries = entries.push(settings_card(column![
                text(&clip.key).size(15),
                text(format!("State: {}", clip_source_label(&clip.source))).size(13),
                text(format!("Modified: {}", clip.last_modified_display)).size(13),
                text(format!("Size: {} KB", clip.size_bytes / 1024)).size(13),
                controls,
            ]));
        }
    }

    let content = column![
        row![
            text("Clips").size(24),
            button(text("Refresh")).on_press(Message::RefreshClips),
            button(text("Close")).on_press(Message::CloseClips),
        ]
        .spacing(10),
        text(&state.clips_status).size(14),
        scrollable(entries),
    ]
    .padding(20)
    .spacing(16);

    container(content).width(Length::Fill).height(Length::Fill).into()
}

fn clip_source_label(source: &ClipSource) -> &'static str {
    match source {
        ClipSource::InMemoryActive => "Recording in memory",
        ClipSource::InMemoryPendingUpload => "Pending upload",
        ClipSource::Uploaded => "Uploaded to S3",
    }
}

fn format_activity_timestamp(timestamp: chrono::DateTime<chrono::Utc>) -> String {
    timestamp
        .with_timezone(&chrono::Local)
        .format("%b %-d, %Y at %-I:%M:%S %p")
        .to_string()
}

fn section_help(content: &str) -> iced::widget::Text<'_> {
    text(content).size(13).style(|_| iced::widget::text::Style {
        color: Some(Color::from_rgb8(0xaa, 0xaa, 0xaa)),
    })
}

fn settings_card<'a>(content: iced::widget::Column<'a, Message>) -> Element<'a, Message> {
    container(content.spacing(8))
        .width(Length::Fill)
        .padding(16)
        .style(card_style)
        .into()
}

fn card_style(_theme: &Theme) -> iced::widget::container::Style {
    iced::widget::container::Style {
        background: Some(Background::Color(Color::from_rgb8(0x22, 0x22, 0x28))),
        border: border::Border {
            color: Color::from_rgb8(0x45, 0x45, 0x52),
            width: 1.0,
            radius: 10.0.into(),
        },
        ..Default::default()
    }
}
