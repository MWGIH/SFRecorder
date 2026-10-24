//! Talking to the dashboard tab: commands come in on stdin, one per line,
//! and status goes out on stdout as one JSON object per line. The recorded
//! view goes to view.json so SFrameOverlay can keep its HUD inside it.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

pub enum Command {
    Pause,
    Resume,
    Stop,
    Hud(bool),
}

/// Reads commands until stdin closes; a closed stdin (the tab went away)
/// stops the recording.
pub fn commands() -> Receiver<Command> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let cmd = match line.trim() {
                "pause" => Command::Pause,
                "resume" => Command::Resume,
                "stop" => Command::Stop,
                "hud show" => Command::Hud(true),
                "hud hide" => Command::Hud(false),
                _ => continue,
            };
            if tx.send(cmd).is_err() {
                return;
            }
        }
        let _ = tx.send(Command::Stop);
    });
    rx
}

pub fn status(state: &str, seconds: f64, file: &str, message: &str) {
    let line = format!(r#"{{"state":"{}","seconds":{:.1},"file":"{}","message":"{}"}}"#, escape(state), seconds, escape(file), escape(message));
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            c if (c as u32) < 0x20 => format!("\\u{:04x}", c as u32).chars().collect(),
            c => vec![c],
        })
        .collect()
}

/// `$XDG_RUNTIME_DIR/sfrecorder/view.json`, there only while recording.
pub struct ViewFile {
    path: PathBuf,
    view: [f64; 4],
}

impl ViewFile {
    pub fn new(view: [f64; 4], hud: bool) -> Self {
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("sfrecorder");
        let _ = std::fs::create_dir_all(&dir);
        let file = Self { path: dir.join("view.json"), view };
        file.write(hud);
        file
    }

    pub fn write(&self, hud: bool) {
        let [left, right, up, down] = self.view;
        let json = format!(
            r#"{{"pid":{},"left":{left:.5},"right":{right:.5},"up":{up:.5},"down":{down:.5},"hud":"{}"}}"#,
            std::process::id(),
            if hud { "show" } else { "hide" }
        );
        // Written whole then renamed, so a reader never sees half a file.
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

impl Drop for ViewFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
