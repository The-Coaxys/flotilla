//! Opt-in diagnostics. Frame cadence is CPU-side view cadence, not GPU timing.
use std::time::{Duration, Instant};

use flc_conn::supervisor::ConnPhase;
use flc_ui::theme::ActiveTheme;
use gpui::prelude::*;
use gpui::{Context, Div, SharedString, Window, div, px};

use super::Workspace;

const SAMPLE_PERIOD: Duration = Duration::from_millis(500);

pub(super) struct Hud {
    last_frame: Option<Instant>,
    frame_intervals: u64,
    frame_elapsed: Duration,
    sampled_at: Instant,
    output_bytes: u64,
    fps: Option<f64>,
    frame_ms: Option<f64>,
    bytes_per_second: f64,
    resident_bytes: Option<u64>,
    rows: [(&'static str, SharedString); 6],
}

impl Hud {
    fn new(now: Instant, output_bytes: u64) -> Self {
        Self {
            last_frame: None,
            frame_intervals: 0,
            frame_elapsed: Duration::ZERO,
            sampled_at: now,
            output_bytes,
            fps: None,
            frame_ms: None,
            bytes_per_second: 0.,
            resident_bytes: None,
            rows: [
                ("FPS", "—".into()),
                ("Frame interval", "—".into()),
                ("Transport", "—".into()),
                ("RTT", "—".into()),
                ("Term RX (all hosts)", "—".into()),
                ("Resident memory", "—".into()),
            ],
        }
    }

    fn frame(&mut self, now: Instant) {
        if let Some(previous) = self.last_frame.replace(now) {
            self.frame_intervals += 1;
            self.frame_elapsed += now.saturating_duration_since(previous);
        }
    }

    fn sample(&mut self, now: Instant, output_bytes: u64, resident_bytes: Option<u64>) {
        let elapsed = now.saturating_duration_since(self.sampled_at).as_secs_f64();
        if elapsed == 0. {
            return;
        }
        let frame_seconds = self.frame_elapsed.as_secs_f64();
        self.fps = (frame_seconds > 0.).then(|| self.frame_intervals as f64 / frame_seconds);
        self.frame_ms =
            (self.frame_intervals > 0).then(|| frame_seconds * 1000. / self.frame_intervals as f64);
        // Closing/removing a host can lower the aggregate counter. It is not traffic.
        self.bytes_per_second = output_bytes.saturating_sub(self.output_bytes) as f64 / elapsed;
        self.resident_bytes = resident_bytes;
        self.frame_intervals = 0;
        self.frame_elapsed = Duration::ZERO;
        self.output_bytes = output_bytes;
        self.sampled_at = now;
        self.rows[0].1 = self
            .fps
            .map_or_else(|| "—".into(), |n| format!("{n:.1}").into());
        self.rows[1].1 = self
            .frame_ms
            .map_or_else(|| "—".into(), |n| format!("{n:.2} ms").into());
        self.rows[4].1 = format!("{:.1} KiB/s", self.bytes_per_second / 1024.).into();
        self.rows[5].1 = self.resident_bytes.map_or_else(
            || "Unavailable".into(),
            |n| format!("{:.1} MiB", n as f64 / 1048576.).into(),
        );
    }
}

impl Workspace {
    fn terminal_output_bytes(&self) -> u64 {
        self.hosts
            .iter()
            .filter_map(|host| host.sync.as_ref())
            .map(|sync| sync.terminal_output_bytes())
            .sum()
    }

    pub(super) fn toggle_hud(&mut self, cx: &mut Context<Self>) {
        if self.hud.take().is_some() {
            // Dropping the task cancels the timer and any in-flight sample.
            self.hud_sampler = None;
        } else {
            self.hud = Some(Hud::new(Instant::now(), self.terminal_output_bytes()));
            let executor = cx.background_executor().clone();
            self.hud_sampler = Some(cx.spawn(async move |this, cx| {
                loop {
                    executor.timer(SAMPLE_PERIOD).await;
                    // OS memory queries never run on the render thread.
                    let resident = executor.spawn(async { resident_bytes() }).await;
                    if this
                        .update(cx, |ws, cx| {
                            let bytes = ws.terminal_output_bytes();
                            let connection = match ws.selected_phase() {
                                Some(ConnPhase::Connected { kind, rtt_ms, .. }) => {
                                    (kind.to_string().into(), format!("{rtt_ms} ms").into())
                                }
                                _ => ("Offline".into(), "—".into()),
                            };
                            if let Some(hud) = &mut ws.hud {
                                hud.sample(Instant::now(), bytes, resident);
                                hud.rows[2].1 = connection.0;
                                hud.rows[3].1 = connection.1;
                                cx.notify();
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }));
        }
        cx.notify();
    }

    pub(super) fn render_hud(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let hud = self.hud.as_mut()?;
        hud.frame(Instant::now());
        // Repaint only while the overlay exists. This intentionally measures
        // continuously driven cadence rather than claiming idle renders are FPS.
        window.request_animation_frame();
        let theme = cx.theme();
        Some(
            div()
                .absolute()
                // Bottom-left of the center area: never covers the right panel
                // or the settings overlay, which open above the canvas.
                .left(px(300.))
                .bottom(px(56.))
                .w(px(268.))
                .p(px(12.))
                .rounded(px(6.))
                .bg(theme.surface_raised)
                .border_1()
                .border_color(theme.border)
                .text_color(theme.text_primary)
                .text_size(theme.text_size_sm)
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div().flex().justify_between().child("Performance").child(
                        div()
                            .id("hud-close")
                            .cursor_pointer()
                            .child("Close")
                            .on_click(cx.listener(|ws, _, _, cx| ws.toggle_hud(cx))),
                    ),
                )
                .children(hud.rows.iter().map(|(name, value)| {
                    div()
                        .flex()
                        .justify_between()
                        .child(div().text_color(theme.text_secondary).child(*name))
                        .child(value.clone())
                }))
                .child(
                    div()
                        .text_color(theme.text_secondary)
                        .child("CPU-side cadence · not GPU timing"),
                )
                .child(
                    div()
                        .text_color(theme.text_secondary)
                        .child("Terminal output · excludes snapshots"),
                ),
        )
    }
}

#[cfg(target_os = "linux")]
fn resident_bytes() -> Option<u64> {
    parse_resident_bytes(&std::fs::read_to_string("/proc/self/status").ok()?)
}

#[cfg(any(target_os = "linux", test))]
fn parse_resident_bytes(status: &str) -> Option<u64> {
    let mut fields = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .split_whitespace();
    let kib = fields.next()?.parse::<u64>().ok()?;
    (fields.next()? == "kB").then_some(kib)?.checked_mul(1024)
}

#[cfg(windows)]
fn resident_bytes() -> Option<u64> {
    // PROCESS_MEMORY_COUNTERS from psapi.h: two DWORDs, eight SIZE_Ts.
    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
        fn K32GetProcessMemoryInfo(
            process: *mut std::ffi::c_void,
            counters: *mut Counters,
            size: u32,
        ) -> i32;
    }
    let mut counters = Counters {
        cb: std::mem::size_of::<Counters>() as u32,
        ..Counters::default()
    };
    // SAFETY: the pseudo-handle is current-process-owned and never closed;
    // counters is writable and its exact size and C layout match the API.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    (ok != 0).then_some(counters.working_set_size as u64)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn resident_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadence_and_throughput_use_elapsed_time_and_reset_between_samples() {
        let start = Instant::now();
        let mut hud = Hud::new(start, 100);
        for tick in 0..=50 {
            hud.frame(start + Duration::from_millis(tick * 10));
        }
        hud.sample(start + Duration::from_millis(500), 1124, Some(8192));
        assert!((hud.fps.unwrap() - 100.).abs() < 0.001);
        assert!((hud.frame_ms.unwrap() - 10.).abs() < 0.001);
        assert!((hud.bytes_per_second - 2048.).abs() < 0.001);
        assert_eq!(hud.resident_bytes, Some(8192));
        hud.sample(start + Duration::from_secs(1), 50, None);
        assert_eq!(hud.bytes_per_second, 0.);
        assert_eq!(hud.fps, None);
        assert_eq!(hud.frame_ms, None);
        assert_eq!(hud.resident_bytes, None);
    }

    #[test]
    fn linux_memory_requires_rss_unit_and_checked_size() {
        assert_eq!(
            parse_resident_bytes("Name:\tflotilla\nVmRSS:\t12345 kB\n"),
            Some(12641280)
        );
        assert_eq!(parse_resident_bytes("VmSize: 12345 kB"), None);
        assert_eq!(parse_resident_bytes("VmRSS: 12 MB"), None);
        assert_eq!(parse_resident_bytes("VmRSS: 18446744073709551615 kB"), None);
    }
}
