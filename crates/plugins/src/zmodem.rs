//! ZMODEM (`sz`/`rz`) bridge for the terminal.
//!
//! The remote shell starts `sz` or `rz`; this module detects the ZMODEM
//! handshake in the SSH output, hides the protocol bytes from the terminal,
//! and drives `zmodem2`'s caller-owned state machine. Downloads land in the
//! user's Downloads folder, uploads come from a file picker in the UI.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use xcord_plugin_api::Transport;

use zmodem2::{Action, Event, FileInfo, Position, Receiver, Sender};

const MARK: &[u8] = b"**\x18B";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Start {
    /// Remote `sz` is about to send files.
    Receive,
    /// Remote `rz` is waiting for files from us.
    Upload,
}

/// Find the first ZMODEM header in a terminal output chunk.
pub fn detect(data: &[u8]) -> Option<(usize, Start)> {
    let mut index = 0;
    while index + MARK.len() + 2 <= data.len() {
        if &data[index..index + MARK.len()] == MARK {
            let type_start = index + MARK.len();
            let kind = std::str::from_utf8(&data[type_start..type_start + 2])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok());
            match kind {
                Some(0) => return Some((index, Start::Receive)),
                Some(1) => return Some((index, Start::Upload)),
                _ => {}
            }
        }
        index += 1;
    }
    None
}

pub enum Session {
    Receiving(ReceiverState),
    UploadPending(Vec<u8>),
    Sending(SenderState),
}

pub struct ReceiverState {
    state: Receiver,
    dir: PathBuf,
    file: Option<File>,
    path: Option<PathBuf>,
    input: Vec<u8>,
    consumed: usize,
    finished: bool,
}

pub struct SenderState {
    state: Sender,
    files: Vec<(String, Vec<u8>)>,
    index: usize,
    input: Vec<u8>,
    consumed: usize,
    finished: bool,
}

impl Session {
    pub fn receiving() -> Self {
        Self::Receiving(ReceiverState::new())
    }

    pub fn upload_pending(bytes: Vec<u8>) -> Self {
        Self::UploadPending(bytes)
    }

    /// Start sending the chosen files to a remote `rz`.
    pub fn start_upload(&mut self, paths: &[String]) -> Result<(), String> {
        let pending = match self {
            Self::UploadPending(bytes) => std::mem::take(bytes),
            _ => return Err("No remote rz is waiting for an upload.".to_string()),
        };
        let mut files = Vec::new();
        for path in paths {
            let path = Path::new(path);
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .ok_or_else(|| format!("Invalid upload path: {}", path.display()))?
                .to_string();
            let data = std::fs::read(path)
                .map_err(|error| format!("Could not read {}: {error}", path.display()))?;
            if data.len() > u32::MAX as usize {
                return Err(format!("{name} is larger than 4 GiB."));
            }
            files.push((name, data));
        }
        if files.is_empty() {
            return Err("No files selected.".to_string());
        }
        let mut state = Sender::new().map_err(|error| format!("ZMODEM sender: {error:?}"))?;
        state.set_streaming_window(usize::MAX);
        begin_file(&mut state, &files, 0)?;
        *self = Self::Sending(SenderState {
            state,
            files,
            index: 0,
            input: pending,
            consumed: 0,
            finished: false,
        });
        Ok(())
    }

    pub fn cancel(&mut self, transport: Option<&dyn Transport>) {
        match self {
            Self::Receiving(state) => {
                let _ = state.state.abort();
                for _ in 0..8 {
                    match state.state.poll() {
                        Action::WriteWire(bytes) => {
                            let len = bytes.len();
                            if let Some(transport) = transport {
                                let _ = transport.write(bytes);
                            }
                            state.state.wire_written(len);
                        }
                        _ => break,
                    }
                }
            }
            Self::Sending(state) => {
                state.state.abort();
                for _ in 0..8 {
                    match state.state.poll() {
                        Action::WriteWire(bytes) => {
                            let len = bytes.len();
                            if let Some(transport) = transport {
                                let _ = transport.write(bytes);
                            }
                            state.state.wire_written(len);
                        }
                        _ => break,
                    }
                }
            }
            Self::UploadPending(_) => {
                if let Some(transport) = transport {
                    let _ = transport.write(&[0x18; 10]);
                }
            }
        }
    }

    /// Feed incoming terminal bytes and pump the protocol. Returns `true` when
    /// the session has completed and the caller can drop it.
    pub fn feed(
        &mut self,
        data: &[u8],
        transport: Option<&dyn Transport>,
        notices: &mut Vec<String>,
    ) -> bool {
        match self {
            Self::UploadPending(bytes) => {
                bytes.extend_from_slice(data);
                false
            }
            Self::Receiving(state) => {
                state.input.extend_from_slice(data);
                state.pump(transport, notices)
            }
            Self::Sending(state) => {
                state.input.extend_from_slice(data);
                state.pump(transport, notices)
            }
        }
    }
}

pub fn visible_name(name: &[u8]) -> String {
    let raw = String::from_utf8_lossy(name);
    let base = Path::new(raw.as_ref())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download");
    let cleaned: String = base
        .chars()
        .filter(|ch| !ch.is_control() && *ch != '/' && *ch != '\\')
        .collect();
    if cleaned.trim().is_empty() {
        "download".to_string()
    } else {
        cleaned
    }
}

fn download_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        let home = PathBuf::from(home);
        let downloads = home.join("Downloads");
        if downloads.is_dir() {
            return downloads;
        }
        return home;
    }
    std::env::temp_dir()
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("download");
    let extension = path.extension().and_then(|ext| ext.to_str());
    for index in 1..10_000 {
        let name = match extension {
            Some(extension) => format!("{stem}-{index}.{extension}"),
            None => format!("{stem}-{index}"),
        };
        let candidate = dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem}-{}", std::process::id()))
}

impl ReceiverState {
    fn new() -> Self {
        let state = Receiver::with_flow_control(0, true)
            .unwrap_or_else(|_| Receiver::new().expect("ZMODEM receiver"));
        Self {
            state,
            dir: download_dir(),
            file: None,
            path: None,
            input: Vec::new(),
            consumed: 0,
            finished: false,
        }
    }

    fn pump(&mut self, transport: Option<&dyn Transport>, notices: &mut Vec<String>) -> bool {
        for _ in 0..100_000 {
            match self.state.poll() {
                Action::WriteWire(bytes) => {
                    let len = bytes.len();
                    if let Some(transport) = transport {
                        if transport.write(bytes).is_err() {
                            return true;
                        }
                    }
                    self.state.wire_written(len);
                }
                Action::WriteFile(bytes) => {
                    let len = bytes.len();
                    if let Some(file) = self.file.as_mut() {
                        let _ = file.write_all(bytes);
                    }
                    let _ = self.state.file_written(len);
                }
                Action::Event(event) => match event {
                    Event::FileStarted(info) => {
                        let name = visible_name(info.name);
                        let path = unique_path(&self.dir, &name);
                        match OpenOptions::new()
                            .write(true)
                            .create(true)
                            .truncate(true)
                            .open(&path)
                        {
                            Ok(file) => {
                                notices
                                    .push(format!("ZMODEM receiving {name} -> {}", path.display()));
                                self.file = Some(file);
                                self.path = Some(path);
                            }
                            Err(error) => {
                                notices.push(format!("ZMODEM could not create {name}: {error}"));
                                self.file = None;
                                self.path = None;
                            }
                        }
                    }
                    Event::FileCompleted => {
                        if let Some(mut file) = self.file.take() {
                            let _ = file.flush();
                        }
                        if let Some(path) = self.path.take() {
                            notices.push(format!("ZMODEM saved {}", path.display()));
                        }
                    }
                    Event::SessionCompleted => {
                        self.finished = true;
                        notices.push("ZMODEM download complete.".to_string());
                        continue;
                    }
                    Event::Aborted => {
                        self.finished = true;
                        notices.push("ZMODEM transfer aborted.".to_string());
                        continue;
                    }
                    _ => {}
                },
                Action::ReadFile { .. } => {}
                Action::Idle => {
                    if self.consumed < self.input.len() {
                        match self.state.submit_wire(&self.input[self.consumed..]) {
                            Ok(consumed) if consumed > 0 => {
                                self.consumed += consumed;
                                if self.consumed >= self.input.len() {
                                    self.input.clear();
                                    self.consumed = 0;
                                } else if self.consumed > 8192 {
                                    self.input.drain(..self.consumed);
                                    self.consumed = 0;
                                }
                                continue;
                            }
                            Ok(_) => break,
                            Err(error) => {
                                notices.push(format!("ZMODEM receiver error: {error:?}"));
                                self.finished = true;
                                return true;
                            }
                        }
                    }
                    break;
                }
                _ => break,
            }
        }
        self.finished
    }
}

fn begin_file(state: &mut Sender, files: &[(String, Vec<u8>)], index: usize) -> Result<(), String> {
    let (name, data) = files
        .get(index)
        .ok_or_else(|| "No file to send.".to_string())?;
    let size = u32::try_from(data.len()).map_err(|_| format!("{name} is larger than 4 GiB."))?;
    state
        .start_file(FileInfo::new(name.as_bytes(), Some(Position::new(size))))
        .map_err(|error| format!("ZMODEM start file: {error:?}"))
}

impl SenderState {
    fn pump(&mut self, transport: Option<&dyn Transport>, notices: &mut Vec<String>) -> bool {
        for _ in 0..100_000 {
            match self.state.poll() {
                Action::WriteWire(bytes) => {
                    let len = bytes.len();
                    if let Some(transport) = transport {
                        if transport.write(bytes).is_err() {
                            return true;
                        }
                    }
                    self.state.wire_written(len);
                }
                Action::ReadFile { offset, max_len } => {
                    let data = self
                        .files
                        .get(self.index)
                        .map(|(_, data)| data)
                        .map(|data| {
                            let start = offset.get() as usize;
                            let end = (start + max_len).min(data.len());
                            data.get(start..end).unwrap_or(&[]).to_vec()
                        })
                        .unwrap_or_default();
                    if let Err(error) = self.state.submit_file(&data) {
                        notices.push(format!("ZMODEM sender error: {error:?}"));
                        return true;
                    }
                }
                Action::Event(event) => match event {
                    Event::FileCompleted => {
                        self.index += 1;
                        if self.index < self.files.len() {
                            if let Err(error) = begin_file(&mut self.state, &self.files, self.index)
                            {
                                notices.push(error);
                                return true;
                            }
                        } else if let Err(error) = self.state.finish() {
                            notices.push(format!("ZMODEM finish: {error:?}"));
                            return true;
                        }
                    }
                    Event::SessionCompleted => {
                        self.finished = true;
                        notices.push("ZMODEM upload complete.".to_string());
                        continue;
                    }
                    Event::Aborted => {
                        self.finished = true;
                        notices.push("ZMODEM transfer aborted.".to_string());
                        continue;
                    }
                    _ => {}
                },
                Action::Idle => {
                    if self.consumed < self.input.len() {
                        match self.state.submit_wire(&self.input[self.consumed..]) {
                            Ok(consumed) if consumed > 0 => {
                                self.consumed += consumed;
                                if self.consumed >= self.input.len() {
                                    self.input.clear();
                                    self.consumed = 0;
                                } else if self.consumed > 8192 {
                                    self.input.drain(..self.consumed);
                                    self.consumed = 0;
                                }
                                continue;
                            }
                            Ok(_) => break,
                            Err(error) => {
                                notices.push(format!("ZMODEM sender error: {error:?}"));
                                self.finished = true;
                                return true;
                            }
                        }
                    }
                    break;
                }
                _ => break,
            }
        }
        self.finished
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_sz_and_rz_headers() {
        assert_eq!(detect(b"hello**\x18B00rest"), Some((5, Start::Receive)));
        assert_eq!(detect(b"**\x18B01rest"), Some((0, Start::Upload)));
        assert_eq!(detect(b"no protocol here"), None);
    }

    #[test]
    fn sanitizes_remote_names() {
        assert_eq!(visible_name(b"../../etc/passwd"), "passwd");
        assert_eq!(visible_name(b"..\\..\\windows\\win.ini"), "win.ini");
        assert_eq!(visible_name(b""), "download");
        assert_eq!(visible_name(b".\x01hidden"), ".hidden");
    }
}
