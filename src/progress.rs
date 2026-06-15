//! `indicatif` progress UI: an overall files bar plus per-file transfer bars, and the final
//! summary. See specs/progress-ui.md.

use std::path::Path;

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

use crate::sync::SyncSummary;

pub struct Progress {
    multi: MultiProgress,
    overall: ProgressBar,
    hidden: bool,
}

impl Progress {
    pub fn new(total_files: u64, total_bytes: u64, quiet: bool) -> Progress {
        use std::io::IsTerminal;
        let hidden = quiet || !std::io::stderr().is_terminal();
        let multi = MultiProgress::new();
        if hidden {
            multi.set_draw_target(ProgressDrawTarget::hidden());
        }
        let overall = multi.add(ProgressBar::new(total_files));
        overall.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} [{elapsed_precise}] {bar:30.cyan/blue} {pos}/{len} files  {binary_bytes_per_sec}",
            )
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=>-"),
        );
        if !hidden {
            overall.enable_steady_tick(std::time::Duration::from_millis(120));
        }
        let _ = total_bytes;
        Progress {
            multi,
            overall,
            hidden,
        }
    }

    /// Begin a per-file transfer; returns a handle that updates the bar and returns it on drop.
    pub fn file_start(&self, rel: &Path, len: u64) -> FileBar {
        let bar = if self.hidden {
            ProgressBar::hidden()
        } else {
            let b = self.multi.add(ProgressBar::new(len.max(1)));
            b.set_style(
                ProgressStyle::with_template(
                    "  {prefix:.dim} {bar:25.green/black} {bytes}/{total_bytes}  {binary_bytes_per_sec}  ETA {eta}",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar()),
            );
            b.set_prefix(shorten(rel));
            b
        };
        FileBar {
            bar,
            overall: self.overall.clone(),
        }
    }

    pub fn finish_summary(&self, summary: &SyncSummary) {
        self.overall.finish_and_clear();
        print_summary(summary);
    }
}

pub struct FileBar {
    bar: ProgressBar,
    overall: ProgressBar,
}

impl FileBar {
    pub fn inc(&self, bytes: u64) {
        self.bar.inc(bytes);
    }
}

impl Drop for FileBar {
    fn drop(&mut self) {
        self.bar.finish_and_clear();
        self.overall.inc(1);
    }
}

/// Shorten a relative path, keeping the filename, truncating the middle.
fn shorten(rel: &Path) -> String {
    let s = rel.to_string_lossy();
    const MAX: usize = 40;
    if s.len() <= MAX {
        return s.to_string();
    }
    let name = rel
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let keep_head = MAX.saturating_sub(name.len() + 3);
    format!("{}...{}", &s[..keep_head.min(s.len())], name)
}

pub fn print_summary(summary: &SyncSummary) {
    if summary.dry_run {
        // The dry-run plan is printed by the engine; nothing more here.
        return;
    }
    println!("✓ Sync complete ({})", summary.direction_label);
    println!(
        "  Files:    {} transferred, {} deleted, {} unchanged",
        summary.files_transferred, summary.files_deleted, summary.files_unchanged
    );
    let saved_pct = if summary.total_bytes > 0 {
        100.0 * (1.0 - summary.bytes_transferred as f64 / summary.total_bytes as f64)
    } else {
        0.0
    };
    println!(
        "  Data:     {} changed → {} sent ({:.0}% saved by delta+zstd)",
        human_bytes(summary.total_bytes),
        human_bytes(summary.bytes_transferred),
        saved_pct.max(0.0)
    );
    println!(
        "  Time:     {}    Avg: {}/s",
        human_duration(summary.elapsed),
        human_bytes(summary.avg_speed_bps as u64)
    );
    if summary.symlinks_skipped > 0 {
        println!(
            "⚠ {} symlinks skipped (not supported in v1)",
            summary.symlinks_skipped
        );
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} {}", UNITS[0])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn human_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        format!("{}ms", d.as_millis())
    } else {
        format!("{secs:.2}s")
    }
}
