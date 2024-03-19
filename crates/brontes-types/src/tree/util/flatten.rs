use crate::normalized_actions::NormalizedAction;

#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct FlattenSpecified<V: NormalizedAction, I: Iterator<Item = V>, W, T> {
    iter: I,
    wanted: W,
    transform: T,
    extra: Vec<V>,
}

impl<V: NormalizedAction, I: Iterator<Item = V>, W, T> FlattenSpecified<V, I, W, T> {
    pub(crate) fn new(iter: I, wanted: W, transform: T) -> Self {
        Self {
            iter,
            wanted,
            transform,
            extra: vec![],
        }
    }
}

impl<
        V: NormalizedAction,
        R: Clone,
        I: Iterator<Item = V>,
        W: Fn(&V) -> Option<&R>,
        T: Fn(R) -> Vec<V>,
    > Iterator for FlattenSpecified<V, I, W, T>
{
    type Item = V;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(extra) = self.extra.pop() {
            return Some(extra);
        }

        self.iter.next().and_then(|item| {
            if let Some(wanted) = (self.wanted)(&item) {
                let mut ret = (self.transform)(wanted.clone());
                let now = ret.pop();
                self.extra.extend(ret);
                now
            } else {
                Some(item)
            }
        })
    }
}
