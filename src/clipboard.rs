//! Serialized access to the system pasteboard.
//!
//! `arboard::Clipboard` is not `Sync`, and AppKit is happiest when one thread
//! owns the pasteboard handle for the life of the process, so every request is
//! funnelled through a single dedicated worker thread.

use std::fmt;
use std::thread;

use arboard::{Clipboard, ImageData};
use tokio::sync::{mpsc, oneshot};

pub type Result<T> = std::result::Result<T, ClipError>;

#[derive(Debug)]
pub struct ClipError(String);

impl ClipError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    fn new_from_display(err: impl fmt::Display) -> Self {
        Self(err.to_string())
    }
}

impl fmt::Display for ClipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ClipError {}

/// Raw RGBA8 pixels, the only image form `arboard` speaks.
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

pub enum Payload {
    Text(String),
    Image(Image),
}

enum Command {
    Set(Payload, oneshot::Sender<Result<()>>),
    GetText(oneshot::Sender<Result<String>>),
    GetImage(oneshot::Sender<Result<Image>>),
}

#[derive(Clone)]
pub struct ClipboardHandle {
    tx: mpsc::UnboundedSender<Command>,
}

impl ClipboardHandle {
    /// Opens the pasteboard and hands it to a worker thread. Failing here at
    /// startup is much easier to diagnose than failing on the first request.
    pub fn spawn() -> Result<Self> {
        let clipboard = Clipboard::new().map_err(from_arboard)?;
        let (tx, rx) = mpsc::unbounded_channel();
        thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || worker(clipboard, rx))
            .map_err(ClipError::new_from_display)?;
        Ok(Self { tx })
    }

    pub async fn set(&self, payload: Payload) -> Result<()> {
        self.dispatch(move |reply| Command::Set(payload, reply))
            .await
    }

    pub async fn get_text(&self) -> Result<String> {
        self.dispatch(Command::GetText).await
    }

    pub async fn get_image(&self) -> Result<Image> {
        self.dispatch(Command::GetImage).await
    }

    async fn dispatch<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T>>) -> Command,
    ) -> Result<T> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(build(reply_tx))
            .map_err(|_| ClipError::new("clipboard worker is gone"))?;
        reply_rx
            .await
            .map_err(|_| ClipError::new("clipboard worker dropped the request"))?
    }
}

fn worker(mut clipboard: Clipboard, mut rx: mpsc::UnboundedReceiver<Command>) {
    while let Some(command) = rx.blocking_recv() {
        // A closed reply channel just means the client hung up mid-request.
        match command {
            Command::Set(payload, reply) => {
                let _ = reply.send(set(&mut clipboard, payload));
            }
            Command::GetText(reply) => {
                let _ = reply.send(clipboard.get_text().map_err(from_arboard));
            }
            Command::GetImage(reply) => {
                let _ = reply.send(get_image(&mut clipboard));
            }
        }
    }
}

fn set(clipboard: &mut Clipboard, payload: Payload) -> Result<()> {
    match payload {
        Payload::Text(text) => clipboard.set_text(text).map_err(from_arboard),
        Payload::Image(image) => clipboard
            .set_image(ImageData {
                width: image.width,
                height: image.height,
                bytes: image.rgba.into(),
            })
            .map_err(from_arboard),
    }
}

fn get_image(clipboard: &mut Clipboard) -> Result<Image> {
    let data = clipboard.get_image().map_err(from_arboard)?;
    Ok(Image {
        width: data.width,
        height: data.height,
        rgba: data.bytes.into_owned(),
    })
}

fn from_arboard(err: arboard::Error) -> ClipError {
    ClipError(err.to_string())
}
