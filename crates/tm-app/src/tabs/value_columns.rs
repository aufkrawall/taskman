//! The numeric column catalogue shared by the process-shaped tables.
//!
//! Processes and Users show the same six measurements of the same processes —
//! one page per process, one page per session — so the catalogue, the display
//! order the user drags them into, and the way one value becomes a cell live
//! here instead of being written twice and drifting apart.
//!
//! Two orders are in play and must never be confused. The LOGICAL order is the
//! one below: it indexes every row's `values` array and the header totals, and
//! nothing outside the UI ever sees anything else. The DISPLAY order is the
//! user's permutation of the numeric block, persisted by stable id; the two
//! helpers [`logical_col`] and [`display_col`] are the only translation.

use tm_core::format;
use tm_core::i18n::{self, K};

use crate::widgets::tablekit::TmColumn;

/// Numeric value columns: CPU, Memory, Disk I/O, Network, Disk active
/// time, GPU.
pub const VALUE_COLS: usize = 6;

/// Index of the Network column inside `values` — one of the three whose value
/// can be genuinely unknown per row.
pub const NET: usize = 3;
/// Index of the Disk active time column inside `values`; likewise optional.
pub const DISK_ACT: usize = 4;
/// Index of the GPU column inside `values`; optional for the same reason.
pub const GPU: usize = 5;

/// Columns that never move: a label column and Status. The blue heat band is
/// painted as ONE contiguous span starting after them, so a value column
/// dragged in front of either would tear it.
pub const FIXED_COLS: usize = 2;

/// One numeric column, in the fixed LOGICAL order.
pub struct ValueColumn {
    pub id: &'static str,
    pub label: fn() -> &'static str,
    pub width: f32,
}

pub const VALUE_COLUMNS: [ValueColumn; VALUE_COLS] = [
    ValueColumn {
        id: "cpu",
        label: || i18n::tr(K::ColCpu),
        width: 110.0,
    },
    ValueColumn {
        id: "mem",
        label: || i18n::tr(K::ColMemory),
        width: 110.0,
    },
    ValueColumn {
        id: "disk",
        label: || i18n::tr(K::ColDisk),
        width: 110.0,
    },
    // Windows supplies this lazily from the per-process ETW source. Missing
    // telemetry still renders as an honest "—", never fake zero.
    ValueColumn {
        id: "net",
        label: || i18n::tr(K::ColNetwork),
        width: 110.0,
    },
    // Which process is actually keeping the disks busy. The Disk I/O column
    // counts I/O BYTES the process asked for, cache hits included; this one
    // is its share of the time the disks really spent.
    ValueColumn {
        id: "diskact",
        label: || i18n::tr(K::ColDiskActivity),
        width: 150.0,
    },
    // Busiest-engine utilization, from the same on-demand PDH group the
    // Performance page uses.
    ValueColumn {
        id: "gpu",
        label: || i18n::tr(K::ColGpu),
        width: 110.0,
    },
];

/// Stable ids of a page's columns in LOGICAL order, for sort persistence.
/// `fixed` are the page's own two leading columns.
pub fn column_ids(fixed: [&'static str; FIXED_COLS]) -> Vec<&'static str> {
    let mut ids = fixed.to_vec();
    ids.extend(VALUE_COLUMNS.iter().map(|c| c.id));
    ids
}

/// The table's columns: the page's fixed ones, then the numeric block in the
/// user's display order.
pub fn columns(fixed: Vec<TmColumn>, order: &[usize]) -> Vec<TmColumn> {
    let mut cols = fixed;
    for &li in order {
        let c = &VALUE_COLUMNS[li];
        cols.push(TmColumn::num(c.id, (c.label)(), c.width));
    }
    cols
}

/// Untouched display order.
pub fn default_order() -> Vec<usize> {
    (0..VALUE_COLS).collect()
}

/// The built-in order as stable ids — what a persisted order is compared
/// against, so a default layout leaves nothing in the settings file.
pub fn default_ids() -> Vec<String> {
    VALUE_COLUMNS.iter().map(|c| c.id.to_string()).collect()
}

/// Display slot -> logical column.
pub fn logical_col(order: &[usize], display: usize) -> usize {
    match display.checked_sub(FIXED_COLS) {
        Some(slot) => FIXED_COLS + order.get(slot).copied().unwrap_or(slot),
        None => display,
    }
}

/// Logical column -> display slot.
pub fn display_col(order: &[usize], logical: usize) -> usize {
    match logical.checked_sub(FIXED_COLS) {
        Some(li) => {
            FIXED_COLS
                + order
                    .iter()
                    .position(|candidate| *candidate == li)
                    .unwrap_or(li)
        }
        None => logical,
    }
}

/// Move the column at display slot `from` so it lands at slot `to`.
/// Returns false when the gesture would change nothing.
pub fn move_column(order: &mut Vec<usize>, from: usize, to: usize) -> bool {
    let (Some(from), Some(to)) = (from.checked_sub(FIXED_COLS), to.checked_sub(FIXED_COLS)) else {
        return false;
    };
    if from == to || from >= order.len() || to >= order.len() {
        return false;
    }
    let moved = order.remove(from);
    order.insert(to, moved);
    true
}

/// Display order as stable ids, for persistence.
pub fn saved_ids(order: &[usize]) -> Vec<String> {
    order
        .iter()
        .map(|&li| VALUE_COLUMNS[li].id.to_string())
        .collect()
}

/// Adopt a persisted order, ignoring unknown ids and appending any column the
/// saved file predates. A settings file from an older build must never make a
/// column disappear.
pub fn apply_saved(order: &mut Vec<usize>, ids: &[String]) {
    let mut next: Vec<usize> = Vec::with_capacity(VALUE_COLS);
    for id in ids {
        if let Some(li) = VALUE_COLUMNS.iter().position(|c| c.id == id)
            && !next.contains(&li)
        {
            next.push(li);
        }
    }
    for li in 0..VALUE_COLS {
        if !next.contains(&li) {
            next.push(li);
        }
    }
    *order = next;
}

/// Reorder a logical-order array into the display order.
pub fn in_display_order<T: Clone>(order: &[usize], logical: &[T; VALUE_COLS]) -> Vec<T> {
    order.iter().map(|&li| logical[li].clone()).collect()
}

/// One numeric cell. `known` is false where the telemetry behind the column
/// was never measured for this row: the cell then reads as unknown, never as
/// a fabricated zero (core product invariant).
pub fn value_text(li: usize, value: f64, known: bool) -> String {
    match li {
        _ if !known => "\u{2014}".to_string(),
        0 => format::format_pct_cell(value.min(100.0) as f32),
        1 => format::format_mb(value as u64),
        2 => format::format_rate_mb(value),
        NET => format::format_process_net_rate(value),
        DISK_ACT | GPU => format::format_pct_cell(value.min(100.0) as f32),
        _ => "\u{2014}".to_string(),
    }
}

/// Why a column can read "—", for the hover explanation. `None` where the
/// column is always measurable and a dash would be a bug, not a limitation.
pub fn unavailable_tip(li: usize) -> Option<&'static str> {
    match li {
        NET => Some(i18n::tr(K::NetPerProcessUnavailable)),
        DISK_ACT => Some(i18n::tr(K::DiskPerProcessUnavailable)),
        GPU => Some(i18n::tr(K::GpuPerProcessUnavailable)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_order_survives_a_column_the_file_predates() {
        let mut order = default_order();
        // A file written before the GPU column existed.
        apply_saved(
            &mut order,
            &["gpu", "cpu", "mem", "disk", "net"].map(str::to_string),
        );
        assert_eq!(order[0], GPU);
        assert_eq!(order.len(), VALUE_COLS);
        let mut seen = order.clone();
        seen.sort_unstable();
        assert_eq!(seen, default_order(), "every column must still be present");
    }

    #[test]
    fn display_and_logical_are_inverse() {
        let mut order = default_order();
        assert!(move_column(&mut order, FIXED_COLS + 5, FIXED_COLS));
        for logical in 0..FIXED_COLS + VALUE_COLS {
            assert_eq!(logical_col(&order, display_col(&order, logical)), logical);
        }
    }

    #[test]
    fn an_unmeasured_cell_never_prints_a_zero() {
        assert_eq!(value_text(NET, 0.0, false), "\u{2014}");
        assert_eq!(value_text(GPU, 0.0, false), "\u{2014}");
        assert_ne!(value_text(GPU, 0.0, true), "\u{2014}");
    }
}
