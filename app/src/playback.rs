use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink};
use tracing::{error, warn};

/// Commands sent to the dedicated playback thread.
enum Command {
    Play { key: String, path: PathBuf },
    Stop,
}

/// In-app audio player.
///
/// rodio's `OutputStream` is `!Send`, so it lives entirely inside a dedicated
/// thread. The handle exposed to the UI only holds a command channel plus a
/// shared "now playing" key, which keeps `Player` cheap to clone-free share and
/// safe to store in the iced application state.
pub struct Player {
    tx: Sender<Command>,
    current: Arc<Mutex<Option<String>>>,
}

impl Player {
    pub fn new() -> Self {
        let (tx, rx) = channel::<Command>();
        let current = Arc::new(Mutex::new(None));
        let thread_current = current.clone();
        std::thread::Builder::new()
            .name("audio-playback".into())
            .spawn(move || run_player(rx, thread_current))
            .expect("spawn playback thread");
        Self { tx, current }
    }

    /// Start playing the clip at `path`, replacing anything already playing.
    pub fn play(&self, key: String, path: PathBuf) {
        if let Ok(mut current) = self.current.lock() {
            *current = Some(key.clone());
        }
        let _ = self.tx.send(Command::Play { key, path });
    }

    /// Stop playback immediately.
    pub fn stop(&self) {
        if let Ok(mut current) = self.current.lock() {
            *current = None;
        }
        let _ = self.tx.send(Command::Stop);
    }

    /// Key of the clip currently playing, if any. Cleared automatically when a
    /// clip finishes on its own.
    pub fn now_playing(&self) -> Option<String> {
        self.current.lock().ok().and_then(|current| current.clone())
    }
}

impl Default for Player {
    fn default() -> Self {
        Self::new()
    }
}

fn run_player(rx: Receiver<Command>, current: Arc<Mutex<Option<String>>>) {
    let (_stream, handle) = match OutputStream::try_default() {
        Ok(pair) => pair,
        Err(err) => {
            error!("failed to open audio output for playback: {err}");
            return;
        }
    };

    let mut sink: Option<Sink> = None;

    loop {
        // Poll on a timeout so we can notice when a clip finishes on its own
        // and clear the "now playing" key.
        match rx.recv_timeout(Duration::from_millis(150)) {
            Ok(Command::Play { key, path }) => {
                if let Some(existing) = sink.take() {
                    existing.stop();
                }
                match build_sink(&handle, &path) {
                    Ok(new_sink) => sink = Some(new_sink),
                    Err(err) => {
                        warn!("could not play clip {key}: {err}");
                        clear(&current);
                    }
                }
            }
            Ok(Command::Stop) => {
                if let Some(existing) = sink.take() {
                    existing.stop();
                }
                clear(&current);
            }
            Err(RecvTimeoutError::Timeout) => {
                if sink.as_ref().map(|s| s.empty()).unwrap_or(false) {
                    sink = None;
                    clear(&current);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn clear(current: &Arc<Mutex<Option<String>>>) {
    if let Ok(mut current) = current.lock() {
        *current = None;
    }
}

fn build_sink(handle: &OutputStreamHandle, path: &PathBuf) -> Result<Sink> {
    let file = BufReader::new(File::open(path)?);
    let source = Decoder::new(file)?;
    let sink = Sink::try_new(handle)?;
    sink.append(source);
    Ok(sink)
}
