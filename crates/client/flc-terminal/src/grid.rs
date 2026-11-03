//! Cell geometry: font metrics → grid size, pixel → cell hit testing and the
//! resize debounce. Logical pixels throughout; `scale` converts to device
//! pixels where the protocol reports them.
use std::time::{Duration, Instant};

use fl_model::{MAX_COLUMNS, MAX_SCREEN_ROWS};
use fl_term::Size;

/// One terminal cell, in logical pixels, snapped to whole device pixels so
/// adjacent backgrounds never leave seams.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellMetrics {
    pub cell_width: f32,
    pub line_height: f32,
}

impl CellMetrics {
    /// `advance` is the font's monospace advance, `ascent + descent` its
    /// natural line box; `line_height_scale` stretches the line box.
    pub fn from_font(
        advance: f32,
        ascent: f32,
        descent: f32,
        line_height_scale: f32,
        scale: f32,
    ) -> Self {
        let snap = |value: f32| (value * scale).round().max(1.0) / scale;
        Self {
            cell_width: snap(advance),
            line_height: snap((ascent + descent.abs()) * line_height_scale.max(0.5)),
        }
    }
}

/// The grid that fits `width × height` logical pixels, clamped to the
/// protocol's supported range, with the cell size in device pixels.
pub fn grid_size(width: f32, height: f32, metrics: CellMetrics, scale: f32) -> Size {
    let fit = |extent: f32, cell: f32| (extent.max(0.0) / cell).floor().min(u16::MAX as f32) as u16;
    let columns = fit(width, metrics.cell_width).clamp(2, MAX_COLUMNS);
    let rows = fit(height, metrics.line_height).clamp(1, MAX_SCREEN_ROWS);
    let device = |cell: f32, count: u16| {
        let px = (cell * scale).round() as u32;
        if px.saturating_mul(count as u32) <= u16::MAX as u32 {
            px as u16
        } else {
            0
        }
    };
    Size {
        columns,
        rows,
        cell_width: device(metrics.cell_width, columns),
        cell_height: device(metrics.line_height, rows),
    }
}

/// Which half of a cell a pointer is over; selections include a cell only
/// when the pointer crossed its midpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Side {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellHit {
    /// Viewport row, clamped to the grid.
    pub row: u16,
    pub column: u16,
    pub side: Side,
    /// Rows beyond the top (negative) or bottom (positive) edge, for
    /// drag auto-scroll.
    pub overflow: i32,
}

/// Map a point relative to the grid origin to a cell. Points outside the
/// grid clamp to the nearest edge cell (drags may leave the pane).
pub fn cell_at(x: f32, y: f32, metrics: CellMetrics, size: Size) -> CellHit {
    let raw_row = (y / metrics.line_height).floor();
    let last_row = size.rows.saturating_sub(1) as f32;
    let overflow = if raw_row < 0.0 {
        raw_row as i32
    } else if raw_row > last_row {
        (raw_row - last_row) as i32
    } else {
        0
    };
    let last_column = size.columns.saturating_sub(1);
    let raw_column = x / metrics.cell_width;
    let (column, side) = if raw_column < 0.0 {
        (0, Side::Left)
    } else if raw_column >= size.columns as f32 {
        (last_column, Side::Right)
    } else {
        let column = raw_column.floor();
        let side = if raw_column - column < 0.5 {
            Side::Left
        } else {
            Side::Right
        };
        (column as u16, side)
    };
    CellHit {
        row: raw_row.clamp(0.0, last_row) as u16,
        column,
        side,
        overflow,
    }
}

/// What to do after observing a laid-out grid size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeStep {
    /// Nothing changed, or a check is already scheduled.
    Idle,
    /// First layout: request this size immediately.
    Send(Size),
    /// Call [`ResizeDebounce::due`] at this instant.
    CheckAt(Instant),
}

/// Requests a resize only when the laid-out grid actually changes, and only
/// once it has been stable for `delay` (a window drag produces one request,
/// not one per frame).
#[derive(Debug)]
pub struct ResizeDebounce {
    delay: Duration,
    requested: Option<Size>,
    pending: Option<(Size, Instant)>,
}

impl ResizeDebounce {
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            requested: None,
            pending: None,
        }
    }

    pub fn requested(&self) -> Option<Size> {
        self.requested
    }

    pub fn observe(&mut self, size: Size, now: Instant) -> ResizeStep {
        if self.requested.is_none() {
            self.requested = Some(size);
            self.pending = None;
            return ResizeStep::Send(size);
        }
        if self.requested == Some(size) {
            self.pending = None;
            return ResizeStep::Idle;
        }
        let scheduled = self.pending.is_some();
        // Every change restarts the quiet period.
        let deadline = now + self.delay;
        self.pending = Some((size, deadline));
        if scheduled {
            ResizeStep::Idle
        } else {
            ResizeStep::CheckAt(deadline)
        }
    }

    /// `Ok(Some(size))`: send it now. `Ok(None)`: nothing pending.
    /// `Err(deadline)`: still settling; check again then.
    pub fn due(&mut self, now: Instant) -> Result<Option<Size>, Instant> {
        match self.pending {
            None => Ok(None),
            Some((_, deadline)) if now < deadline => Err(deadline),
            Some((size, _)) => {
                self.pending = None;
                self.requested = Some(size);
                Ok(Some(size))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_term::{EffectMode, Engine};

    const METRICS: CellMetrics = CellMetrics {
        cell_width: 8.0,
        line_height: 16.0,
    };

    #[test]
    fn metrics_snap_to_device_pixels_at_fractional_dpi() {
        // Cascadia Mono-like metrics at 12.5 px and 150 % scale.
        let m = CellMetrics::from_font(7.53, 11.6, -3.05, 1.0, 1.5);
        assert_eq!(m.cell_width * 1.5, 11.0);
        assert_eq!(m.line_height * 1.5, 22.0);
        let size = grid_size(800.0, 600.0, m, 1.5);
        assert_eq!(size.columns, (800.0 / m.cell_width).floor() as u16);
        assert_eq!(size.cell_width, 11);
        assert_eq!(size.cell_height, 22);
    }

    #[test]
    fn computed_grid_is_accepted_by_the_engine() {
        let size = grid_size(643.0, 401.0, METRICS, 1.0);
        assert_eq!((size.columns, size.rows), (80, 25));
        let mut engine = Engine::new(Size::new(10, 5), EffectMode::Mirror).unwrap();
        engine.resize(size).unwrap();
        assert_eq!(engine.size(), size);
    }

    #[test]
    fn tiny_and_huge_panes_clamp_to_protocol_limits() {
        let tiny = grid_size(3.0, 2.0, METRICS, 1.0);
        assert_eq!((tiny.columns, tiny.rows), (2, 1));
        let huge = grid_size(1e7, 1e7, METRICS, 1.0);
        assert_eq!((huge.columns, huge.rows), (MAX_COLUMNS, MAX_SCREEN_ROWS));
        assert!(huge.validate().is_ok());
        // Pixel totals that overflow the protocol's u16 report as unknown.
        let dense = grid_size(1e7, 1e7, METRICS, 100.0);
        assert_eq!(dense.cell_width, 0);
        assert!(dense.validate().is_ok());
    }

    #[test]
    fn hit_testing_reports_sides_and_clamps_with_overflow() {
        let size = Size::new(10, 4);
        let hit = cell_at(8.0 * 3.0 + 1.0, 16.0 * 2.0 + 3.0, METRICS, size);
        assert_eq!(
            (hit.row, hit.column, hit.side, hit.overflow),
            (2, 3, Side::Left, 0)
        );
        let hit = cell_at(8.0 * 3.0 + 5.0, 0.0, METRICS, size);
        assert_eq!((hit.column, hit.side), (3, Side::Right));
        let above = cell_at(-20.0, -40.0, METRICS, size);
        assert_eq!(
            (above.row, above.column, above.side, above.overflow),
            (0, 0, Side::Left, -3)
        );
        let below = cell_at(500.0, 16.0 * 6.5, METRICS, size);
        assert_eq!(
            (below.row, below.column, below.side, below.overflow),
            (3, 9, Side::Right, 3)
        );
    }

    #[test]
    fn resize_is_sent_once_after_the_layout_settles() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut debounce = ResizeDebounce::new(ms(80));
        let a = Size::new(80, 24);
        assert_eq!(debounce.observe(a, t0), ResizeStep::Send(a));
        // Unchanged layouts on later frames do nothing.
        assert_eq!(debounce.observe(a, t0 + ms(16)), ResizeStep::Idle);
        // A drag: three sizes in quick succession produce one check...
        let b = Size::new(81, 24);
        let c = Size::new(83, 25);
        assert_eq!(
            debounce.observe(b, t0 + ms(20)),
            ResizeStep::CheckAt(t0 + ms(100))
        );
        assert_eq!(debounce.observe(c, t0 + ms(40)), ResizeStep::Idle);
        // ...which finds the quiet period restarted by the last change...
        assert_eq!(debounce.due(t0 + ms(100)), Err(t0 + ms(120)));
        // ...and then requests only the final size.
        assert_eq!(debounce.due(t0 + ms(120)), Ok(Some(c)));
        assert_eq!(debounce.requested(), Some(c));
        assert_eq!(debounce.due(t0 + ms(200)), Ok(None));
    }

    #[test]
    fn returning_to_the_requested_size_cancels_the_pending_resize() {
        let t0 = Instant::now();
        let mut debounce = ResizeDebounce::new(Duration::from_millis(50));
        let a = Size::new(80, 24);
        debounce.observe(a, t0);
        assert!(matches!(
            debounce.observe(Size::new(70, 24), t0),
            ResizeStep::CheckAt(_)
        ));
        assert_eq!(debounce.observe(a, t0), ResizeStep::Idle);
        assert_eq!(debounce.due(t0 + Duration::from_secs(1)), Ok(None));
    }
}
