//! Host-side parallelism for per-crop preprocessing (resize, patchify, tables): scoped
//! threads over contiguous groups, so results land exactly where the serial loop put
//! them.

/// Threads at most for a batch's preprocessing (batches are 12-16 crops).
const MAX_THREADS: usize = 8;

/// Runs `f` on every item, spread over up to [`MAX_THREADS`] scoped threads (the calling
/// thread takes one group). Order of side effects between items is unspecified; each
/// item must only write its own data.
pub fn for_each<T: Send>(items: Vec<T>, f: impl Fn(T) + Sync) {
    let n = items.len();
    let threads = std::thread::available_parallelism()
        .map_or(1, |p| p.get())
        .min(MAX_THREADS)
        .min(n);
    if threads <= 1 {
        items.into_iter().for_each(f);
        return;
    }
    let per = n.div_ceil(threads);
    let mut groups: Vec<Vec<T>> = Vec::with_capacity(threads);
    let mut it = items.into_iter();
    loop {
        let g: Vec<T> = it.by_ref().take(per).collect();
        if g.is_empty() {
            break;
        }
        groups.push(g);
    }
    let f = &f;
    std::thread::scope(|scope| {
        let mut rest = groups.into_iter();
        let first = rest.next();
        for g in rest {
            scope.spawn(move || g.into_iter().for_each(f));
        }
        if let Some(g) = first {
            g.into_iter().for_each(f);
        }
    });
}

/// Maps `f` over `items` on up to `max_threads` helper threads (and [`MAX_THREADS`])
/// and hands the results to
/// `consume` in item order as soon as each is ready, so the consumer (device work)
/// overlaps with the preparation of later items. Stops at the first error of `consume`.
pub fn map_ordered<T: Sync, R: Send, E>(
    items: &[T],
    max_threads: usize,
    f: impl Fn(&T) -> R + Sync,
    mut consume: impl FnMut(usize, R) -> Result<(), E>,
) -> Result<(), E> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let n = items.len();
    let workers = std::thread::available_parallelism()
        .map_or(1, |p| p.get())
        .min(MAX_THREADS)
        .min(max_threads)
        .min(n);
    if workers == 0 || n <= 1 {
        for (i, it) in items.iter().enumerate() {
            consume(i, f(it))?;
        }
        return Ok(());
    }
    let next = AtomicUsize::new(0);
    let (tx, rx) = std::sync::mpsc::channel::<(usize, R)>();
    let (f, next) = (&f, &next);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            scope.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n || tx.send((i, f(&items[i]))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        let mut held = std::collections::BTreeMap::new();
        for want in 0..n {
            while !held.contains_key(&want) {
                match rx.recv() {
                    Ok((i, r)) => {
                        held.insert(i, r);
                    }
                    Err(_) => break,
                }
            }
            let Some(r) = held.remove(&want) else { break };
            if let Err(e) = consume(want, r) {
                // Let the workers stop at their current item.
                next.store(n, Ordering::Relaxed);
                return Err(e);
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn ordered_results_and_early_stop() {
        let items: Vec<usize> = (0..37).collect();
        let mut seen = Vec::new();
        super::map_ordered(
            &items,
            8,
            |i| i * i,
            |k, r| -> Result<(), ()> {
                seen.push((k, r));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen, (0..37).map(|i| (i, i * i)).collect::<Vec<_>>());
        let mut count = 0;
        let r = super::map_ordered(
            &items,
            3,
            |i| *i,
            |k, _| {
                count += 1;
                if k == 5 { Err("stop") } else { Ok(()) }
            },
        );
        assert_eq!((r, count), (Err("stop"), 6));
    }

    #[test]
    fn every_item_once_in_place() {
        for n in [0usize, 1, 3, 16, 17] {
            let mut out = vec![0usize; n];
            let items: Vec<(usize, &mut usize)> = out.iter_mut().enumerate().collect();
            super::for_each(items, |(i, o)| *o = i * 2 + 1);
            assert_eq!(out, (0..n).map(|i| i * 2 + 1).collect::<Vec<_>>());
        }
    }
}
