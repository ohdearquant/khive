//! Terminal progress bar for `kkernel reindex`.

use super::*;

// ─── progress bar ─────────────────────────────────────────────────────────────

pub(super) struct ProgressBar {
    label: &'static str,
    start: Instant,
    current: AtomicU64,
    total: AtomicU64,
    window_current: AtomicU64,
    window_nanos: AtomicU64,
    rate: std::sync::Mutex<f64>,
}

const RATE_WINDOW_SECS: f64 = 10.0;

impl ProgressBar {
    pub(super) fn new(label: &'static str) -> Self {
        Self {
            label,
            start: Instant::now(),
            current: AtomicU64::new(0),
            total: AtomicU64::new(0),
            window_current: AtomicU64::new(0),
            window_nanos: AtomicU64::new(0),
            rate: std::sync::Mutex::new(0.0),
        }
    }

    pub(super) fn update(&self, current: u64, total: u64) {
        self.current.store(current, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);

        let now_ns = self.start.elapsed().as_nanos() as u64;
        let prev_ns = self.window_nanos.load(Ordering::Relaxed);
        let delta_secs = (now_ns - prev_ns) as f64 / 1e9;

        if delta_secs >= RATE_WINDOW_SECS {
            let prev_current = self.window_current.load(Ordering::Relaxed);
            let delta_items = current.saturating_sub(prev_current);
            if delta_secs > 0.1 {
                let window_rate = delta_items as f64 / delta_secs;
                if let Ok(mut r) = self.rate.lock() {
                    if *r < 0.1 {
                        *r = window_rate;
                    } else {
                        *r = 0.3 * *r + 0.7 * window_rate;
                    }
                }
            }
            self.window_current.store(current, Ordering::Relaxed);
            self.window_nanos.store(now_ns, Ordering::Relaxed);
        }

        self.render();
    }

    fn render(&self) {
        use std::io::Write;
        let current = self.current.load(Ordering::Relaxed);
        let total = self.total.load(Ordering::Relaxed);
        let pct = if total > 0 {
            (current as f64 / total as f64 * 100.0).min(100.0)
        } else {
            0.0
        };

        const BAR_WIDTH: usize = 30;
        let filled = (pct / 100.0 * BAR_WIDTH as f64) as usize;
        let empty = BAR_WIDTH.saturating_sub(filled);
        let bar: String = format!("{}{}", "\u{2588}".repeat(filled), "\u{2591}".repeat(empty),);

        let rate = self.rate.lock().map(|r| *r).unwrap_or(0.0);
        let eta = if rate > 0.1 && current < total {
            let remaining = (total - current) as f64 / rate;
            if remaining >= 60.0 {
                format!(
                    "ETA {}m {:02}s",
                    remaining as u64 / 60,
                    remaining as u64 % 60
                )
            } else {
                format!("ETA {:.0}s", remaining)
            }
        } else if current >= total && total > 0 {
            "done".into()
        } else {
            "warming up…".into()
        };

        eprint!(
            "\r  {:<10} [{bar}] {pct:>5.1}% ({current}/{total}) {rate:>6.0}/s {eta}    ",
            self.label,
        );
        let _ = std::io::stderr().flush();
    }

    pub(super) fn finish(&self) {
        self.render();
        eprintln!();
    }
}
