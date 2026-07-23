//! # Layout
//!
//! Screen splitting: arrange N agent panes in a row-major grid, reserving the
//! last terminal line for a status/footer bar. Pure rect arithmetic over
//! [`ratatui::layout::Rect`] — no state, so it recomputes cheaply every frame
//! (the terminal-resize path and the per-pane viewport resize both key off
//! these rects).
//!
//! The grid is balanced: `cols = ceil(sqrt(n))`, `rows = ceil(n / cols)`, filled
//! row-major so the final (partial) row sits at the bottom. Each cell gets a
//! roughly-equal share via `Constraint::Min(1)`.

use ratatui::layout::{Constraint, Layout, Rect};

/// Split `area` into `n` non-overlapping rects arranged in a row-major grid.
///
/// Returns exactly `n` rects (or an empty vec when `n == 0`), each fully inside
/// `area`, with no two overlapping. The grid shape is `cols = ceil(sqrt(n))`,
/// `rows = ceil(n / cols)`; the last row may hold fewer cells but each cell in
/// it still spans the full width of its column block.
///
/// The caller is responsible for subtracting any borders (a `Pane` draws a
/// 1-cell border, so its inner area is `rect.width-2 × rect.height-2`).
#[must_use]
pub fn split_panes(area: Rect, n: usize) -> Vec<Rect> {
    if n == 0 {
        return Vec::new();
    }

    let cols = ((n as f64).sqrt().ceil() as usize).max(1);
    let rows = n.div_ceil(cols);

    // Even vertical split into `rows` rows.
    let v: Vec<Constraint> = (0..rows).map(|_| Constraint::Min(1)).collect();
    let row_rects = Layout::vertical(v).split(area);

    let mut out = Vec::with_capacity(n);
    let mut idx = 0usize;
    for r in 0..rows {
        if idx >= n {
            break;
        }
        let remaining = n - idx;
        let in_row = remaining.min(cols);
        let h: Vec<Constraint> = (0..in_row).map(|_| Constraint::Min(1)).collect();
        let cell_rects = Layout::horizontal(h).split(row_rects[r]);
        for c in 0..in_row {
            // `cell_rects` is an Rc<[Rect]>; index it directly.
            out.push(cell_rects[c]);
            idx += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 120, 60)
    }

    #[test]
    fn zero_panes_is_empty() {
        assert!(split_panes(area(), 0).is_empty());
    }

    #[test]
    fn split_count_and_bounds() {
        for n in [1usize, 2, 3, 4, 5, 6, 9, 12] {
            let a = area();
            let rects = split_panes(a, n);
            assert_eq!(rects.len(), n, "n={n}: expected {n} rects, got {}", rects.len());

            for (i, r) in rects.iter().enumerate() {
                assert!(
                    r.width > 0 && r.height > 0,
                    "n={n}: rect {i} is zero-sized ({r:?})"
                );
                assert!(r.x >= a.x, "n={n}: rect {i} left of area ({r:?})");
                assert!(r.y >= a.y, "n={n}: rect {i} above area ({r:?})");
                assert!(
                    r.right() <= a.right(),
                    "n={n}: rect {i} overflows right ({r:?} vs area {a:?})"
                );
                assert!(
                    r.bottom() <= a.bottom(),
                    "n={n}: rect {i} overflows bottom ({r:?} vs area {a:?})"
                );
            }

            // Pairwise non-overlap.
            for i in 0..rects.len() {
                for j in (i + 1)..rects.len() {
                    assert!(
                        !rects[i].intersects(rects[j]),
                        "n={n}: rects {i} ({:?}) and {j} ({:?}) overlap",
                        rects[i],
                        rects[j]
                    );
                }
            }
        }
    }

    #[test]
    fn single_pane_covers_the_whole_area() {
        let a = area();
        let rects = split_panes(a, 1);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0], a);
    }

    #[test]
    fn four_panes_form_a_two_by_two_grid() {
        let a = area();
        let rects = split_panes(a, 4);
        assert_eq!(rects.len(), 4);
        // Two rows: rows 0,1 share the same y-band; rows 2,3 share a lower band.
        assert_eq!(
            rects[0].y, rects[1].y,
            "row 0 pair should start at the same y"
        );
        assert_eq!(rects[2].y, rects[3].y, "row 1 pair should start at the same y");
        assert!(rects[2].y > rects[0].y, "second row should sit below the first");
        // Columns: rect 0 left of rect 1.
        assert!(rects[0].right() <= rects[1].x);
        assert!(rects[2].right() <= rects[3].x);
    }

    #[test]
    fn three_panes_two_in_first_row_one_in_second() {
        let a = area();
        let rects = split_panes(a, 3);
        assert_eq!(rects.len(), 3);
        // cols = ceil(sqrt(3)) = 2, rows = ceil(3/2) = 2.
        // Row 0: rects[0], rects[1] share the same y-band, side-by-side.
        assert_eq!(
            rects[0].y, rects[1].y,
            "first-row pair should share y origin"
        );
        assert!(rects[0].right() <= rects[1].x, "rect[0] left of rect[1]");
        // Row 1: rects[2] sits below, alone (the partial row).
        assert!(
            rects[2].y > rects[0].y,
            "third pane should start on the second row"
        );
        // The lone bottom pane should still fit within the area horizontally.
        assert!(rects[2].right() <= a.right());
    }
}
