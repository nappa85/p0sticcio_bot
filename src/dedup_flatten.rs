pub trait DedupFlatten: PartialEq {
    fn dedup_flatten(&mut self);
}

pub fn windows_dedup_flatten<T>(mut items: Vec<T>, size: usize) -> Vec<T>
where
    T: DedupFlatten,
{
    let windows = items
        .windows(size)
        .enumerate()
        .filter_map(|(index, e)| {
            if e.windows(2).any(|e| e[0] != e[1]) {
                None
            } else {
                Some(index)
            }
        })
        .collect::<Vec<_>>();
    for window in windows.into_iter().rev() {
        items[window].dedup_flatten();
        items.drain((window + 1)..(window + size));
    }
    items
}
