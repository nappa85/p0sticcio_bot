
pub trait DedupFlatten: PartialEq {
    fn dedup_flatten(self) -> Self;
}

pub fn windows_dedup_flatten<T, X>(items: T, size: usize) -> T
where
    T: IntoIterator<Item=X> + FromIterator<X>,
    X: DedupFlatten,
{
     items
        .into_iter()
        .scan((None, 0), |prev, elm| {
            match prev.0.as_ref() {
                None => {
                    prev.0 = Some(elm);
                    prev.1 = 1;
                    Some(None)
                },
                Some(x) if x == &elm => {
                    prev.1 += 1;
                    if prev.1 >= size {
                        Some(Some(elm.dedup_flatten()))
                    } else {
                        Some(None)
                    }
                },
                _ => {
                    let prev_val = prev.0.replace(elm);
                    Some(prev_val)
                },
            }
        })
        .flatten()
        .collect::<T>()
}
