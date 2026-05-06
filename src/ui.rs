use std::cell::RefCell;

use console::style;
use indicatif::{ProgressBar, ProgressStyle};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    Info,
    Success,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedLog {
    pub kind: LogKind,
    pub message: String,
}

type LogCapture = Box<dyn FnMut(CapturedLog)>;

thread_local! {
    static LOG_CAPTURE: RefCell<Option<LogCapture>> = const { RefCell::new(None) };
}

pub fn capture_logs_stream<T>(emit: impl FnMut(CapturedLog) + 'static, f: impl FnOnce() -> T) -> T {
    LOG_CAPTURE.with(|capture| {
        let previous = capture.replace(Some(Box::new(emit)));
        let result = f();
        let _ = capture.replace(previous);
        result
    })
}

fn capture_line(kind: LogKind, message: String) -> bool {
    LOG_CAPTURE.with(|capture| {
        let mut capture = capture.borrow_mut();
        match capture.as_mut() {
            Some(emit) => {
                emit(CapturedLog { kind, message });
                true
            }
            None => false,
        }
    })
}

fn is_capturing() -> bool {
    LOG_CAPTURE.with(|capture| capture.borrow().is_some())
}

pub fn info(message: impl AsRef<str>) {
    let message = message.as_ref();
    if !capture_line(LogKind::Info, message.to_string()) {
        eprintln!("{} {}", style("info").cyan().bold(), message);
    }
}

pub fn success(message: impl AsRef<str>) {
    let message = message.as_ref();
    if !capture_line(LogKind::Success, message.to_string()) {
        eprintln!("{} {}", style("done").green().bold(), message);
    }
}

pub fn warn(message: impl AsRef<str>) {
    let message = message.as_ref();
    if !capture_line(LogKind::Warn, message.to_string()) {
        eprintln!("{} {}", style("warn").yellow().bold(), message);
    }
}

pub fn error(message: impl AsRef<str>) {
    let message = message.as_ref();
    if !capture_line(LogKind::Error, message.to_string()) {
        eprintln!("{} {}", style("error").red().bold(), message);
    }
}

pub fn keyword(word: impl AsRef<str>) -> String {
    if is_capturing() {
        return word.as_ref().to_string();
    }
    style(word.as_ref()).bold().cyan().to_string()
}

pub fn bytes_progress(message: impl Into<String>) -> ProgressBar {
    if is_capturing() {
        return ProgressBar::hidden();
    }
    let pb = ProgressBar::new(0);
    pb.set_style(
        ProgressStyle::with_template(
            "{msg} [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, eta {eta})",
        )
        .expect("valid bytes template")
        .progress_chars("=>-"),
    );
    pb.set_message(message.into());
    pb
}

pub fn bytes_progress_unknown(message: impl Into<String>) -> ProgressBar {
    if is_capturing() {
        return ProgressBar::hidden();
    }
    let pb = ProgressBar::new(0);
    pb.set_style(
        ProgressStyle::with_template("{msg} {bytes} ({bytes_per_sec})")
            .expect("valid bytes template"),
    );
    pb.set_message(message.into());
    pb
}
