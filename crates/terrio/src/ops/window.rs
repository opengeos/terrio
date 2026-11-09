//! Moving-window operations computed a row at a time, fast, with the windows that need care
//! (nodata, NaN) recomputed one pixel at a time.
//!
//! A window operation runs a fast path over whole rows: the same arithmetic, in the same order for
//! each pixel, vectorized across pixels. The windows holding a value the fast path cannot handle
//! are found from the runs of such values in the rows each window spans ([`dirty_runs`]) and
//! recomputed with the operation's per-pixel code ([`dirty_windows`]).

/// The runs `start..end` of the values of `row` that `dirty` flags; the whole row, `[(0, len)]`
/// ([`is_dense`]), once they number more than one per [`DENSE`] values: a row that speckled is
/// computed one pixel at a time, which the fast path and its fix-ups would not beat, and its runs
/// are not kept.
pub(crate) fn dirty_runs<T: Copy>(row: &[T], dirty: impl Fn(T) -> bool) -> Vec<(usize, usize)> {
    const CHUNK: usize = 64;
    let most = row.len() / DENSE;
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (c, chunk) in row.chunks(CHUNK).enumerate() {
        // Most chunks have no flagged value: a test without branches, which vectorizes, skips them.
        if !chunk.iter().fold(false, |a, v| a | dirty(*v)) {
            continue;
        }
        for (j, v) in chunk.iter().enumerate() {
            if dirty(*v) {
                let j = c * CHUNK + j;
                match runs.last_mut() {
                    Some(run) if run.1 == j => run.1 = j + 1,
                    _ => {
                        if runs.len() >= most {
                            return vec![(0, row.len())];
                        }
                        runs.push((j, j + 1));
                    }
                }
            }
        }
    }
    runs
}

/// Values per run past which [`dirty_runs`] gives a row as a whole.
const DENSE: usize = 16;

/// Whether `runs` cover a whole row of `width` values: compute it one pixel at a time.
pub(crate) fn is_dense(runs: &[(usize, usize)], width: usize) -> bool {
    runs == [(0, width)]
}

/// The columns `start..end` (sorted, disjoint, within `0..width`) whose window, `rx` columns
/// on each side, meets one of `runs`: the runs of the window's rows.
pub(crate) fn dirty_windows<'a>(runs: impl IntoIterator<Item = &'a [(usize, usize)]>, rx: usize, width: usize) -> Vec<(usize, usize)> {
    let mut spans: Vec<(usize, usize)> = runs.into_iter().flatten().map(|&(s, e)| (s.saturating_sub(rx), (e + rx).min(width))).collect();
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (s, e) in spans {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
}

/// `avx2_rows! { fn name[generics](args) { body } }` defines `fn name<generics>(args)`, which runs
/// `body` compiled for AVX2 when `is_x86_feature_detected!` finds it at run time (x86-64), and as
/// it is elsewhere. Both copies are the same source: the same IEEE operations (Rust contracts no
/// FMA), so the same values; only the vectors the compiler chooses differ. The body cannot name
/// `Self`, and every generic parameter must appear in an argument's type.
macro_rules! avx2_rows {
    ($(#[$attr:meta])* fn $name:ident[$($generics:tt)*]($($arg:ident: $ty:ty),* $(,)?) $body:block) => {
        $(#[$attr])*
        fn $name<$($generics)*>($($arg: $ty),*) {
            #[inline(always)]
            fn body<$($generics)*>($($arg: $ty),*) $body

            #[cfg(target_arch = "x86_64")]
            #[target_feature(enable = "avx2")]
            fn avx2<$($generics)*>($($arg: $ty),*) {
                body($($arg),*)
            }

            #[cfg(target_arch = "x86_64")]
            if std::arch::is_x86_feature_detected!("avx2") {
                // SAFETY: the CPU has AVX2, the one feature `avx2` is compiled for.
                return unsafe { avx2($($arg),*) };
            }
            body($($arg),*)
        }
    };
}
pub(crate) use avx2_rows;

#[cfg(test)]
mod tests {
    use super::*;

    /// A row with more than one run per `DENSE` values is given whole, so it is computed one pixel
    /// at a time and its runs are not kept.
    #[test]
    fn speckled_rows_are_given_whole() {
        let speckled: Vec<f32> = (0..160).map(|i| if i % 2 == 0 { f32::NAN } else { 1.0 }).collect();
        assert_eq!(dirty_runs(&speckled, f32::is_nan), vec![(0, 160)]);
        assert!(is_dense(&dirty_runs(&speckled, f32::is_nan), 160));
        let ten: Vec<f32> = (0..160).map(|i| if i % 16 == 0 { f32::NAN } else { 1.0 }).collect();
        assert_eq!(dirty_runs(&ten, f32::is_nan).len(), 10);
    }

    #[test]
    fn runs_and_the_windows_they_reach() {
        let row: Vec<f32> = (0..200).map(|i| if i == 3 || i == 4 || i == 70 || i == 199 { f32::NAN } else { i as f32 }).collect();
        let runs = dirty_runs(&row, f32::is_nan);
        assert_eq!(runs, vec![(3, 5), (70, 71), (199, 200)]);
        let other = [(5, 6), (100, 101)];
        assert_eq!(dirty_windows([&runs[..], &other[..]], 1, 200), vec![(2, 7), (69, 72), (99, 102), (198, 200)]);
        assert_eq!(dirty_windows([&[(0, 1)][..]], 2, 200), vec![(0, 3)]);
        assert!(dirty_windows([&[][..]], 1, 10).is_empty());
    }
}
