pub trait DedupFlatten: PartialEq {
    fn dedup_flatten(&mut self);
}

pub fn windows_dedup_flatten<T>(mut items: Vec<T>, size: usize) -> Vec<T>
where
    T: DedupFlatten + std::fmt::Debug,
{
    // the collect here is necessary to truncate items reference
    #[allow(clippy::needless_collect)]
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
        if items.len() >= window + size {
            items[window].dedup_flatten();
            items.drain((window + 1)..(window + size));
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::DedupFlatten;

    #[derive(Copy, Clone, Debug, PartialEq)]
    struct Foo(usize);

    impl DedupFlatten for Foo {
        fn dedup_flatten(&mut self) {
            self.0 += 1;
        }
    }

    #[test]
    fn test1() {
        let list = vec![Foo(0); 8];
        assert_eq!(super::windows_dedup_flatten(list, 8), &[Foo(1)]);
    }

    #[test]
    fn test2() {
        let list = vec![
            Foo(1),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
        ];
        assert_eq!(super::windows_dedup_flatten(list, 8), &[Foo(1), Foo(1)]);
    }

    #[test]
    fn test3() {
        let list = vec![
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(1),
        ];
        assert_eq!(super::windows_dedup_flatten(list, 8), &[Foo(1), Foo(1)]);
    }

    #[test]
    fn test4() {
        let list = vec![
            Foo(1),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
        ];
        assert_eq!(
            super::windows_dedup_flatten(list, 8),
            &[Foo(1), Foo(0), Foo(1)]
        );
    }

    #[test]
    fn test5() {
        let list = vec![
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(0),
            Foo(1),
        ];
        assert_eq!(
            super::windows_dedup_flatten(list, 8),
            &[Foo(0), Foo(1), Foo(1)]
        );
    }
}
