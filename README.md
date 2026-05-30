# audio-friend

A small Rust desktop app for monitoring audio activity, capturing clips, and optionally uploading recordings to an S3-compatible bucket.

## Features

- Monitor audio input activity
- Adjustable input/output detection thresholds
- Configurable ring buffer and keepalive timing
- Device selection for input and output
- Optional S3-compatible upload support
- Activity log window for recent events
- Clips window for viewing uploaded clip history
- Desktop UI built with `iced`

## Workspace layout

- `Cargo.toml` — workspace manifest
- `app/` — main application crate

## Requirements

- Rust toolchain
- System audio support compatible with `cpal`
- Linux desktop environment for the current UI behavior

## Run

```bash
cargo run -p audio-friend
```

## Build check

```bash
cargo check
```

## Configuration

The app includes a settings window for:

- input/output device selection
- detection sensitivity
- ring buffer duration
- keepalive duration
- S3 endpoint, bucket, access key, and secret key

Leave the S3 fields blank to disable uploads.

## S3 support

Uploads use the `rust-s3` crate and should work with S3-compatible providers, not just AWS.

Expected settings:

- endpoint URL
- bucket name
- access key
- secret key

## Current notes

- A synthetic `Default` device option is available in the UI
- Clips history is listed from S3 under the `recordings/` prefix
- Output device selection is exposed in settings; runtime behavior may still primarily rely on input capture paths

## Development

```bash
cargo check
cargo fmt
```
