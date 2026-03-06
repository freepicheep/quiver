use console::style;
use indicatif::{ProgressBar, ProgressStyle};

pub fn info(message: impl AsRef<str>) {
    eprintln!("{} {}", style("info").cyan().bold(), message.as_ref());
}

pub fn success(message: impl AsRef<str>) {
    eprintln!("{} {}", style("done").green().bold(), message.as_ref());
}

pub fn warn(message: impl AsRef<str>) {
    eprintln!("{} {}", style("warn").yellow().bold(), message.as_ref());
}

pub fn error(message: impl AsRef<str>) {
    eprintln!("{} {}", style("error").red().bold(), message.as_ref());
}

pub fn keyword(word: impl AsRef<str>) -> String {
    style(word.as_ref()).bold().cyan().to_string()
}

pub fn bytes_progress(message: impl Into<String>) -> ProgressBar {
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
    let pb = ProgressBar::new(0);
    pb.set_style(
        ProgressStyle::with_template("{msg} {bytes} ({bytes_per_sec})")
            .expect("valid bytes template"),
    );
    pb.set_message(message.into());
    pb
}
