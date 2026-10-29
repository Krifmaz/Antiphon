//! The order files wait to be sent in.
//!
//! One file goes at a time -- the call and the transfer share one link,
//! and two transfers would only make both slower while the voice suffers
//! -- so everything else waits here, in an order the user can change.
//! Generic over what is queued so the ordering rules are testable without
//! a phone, a peer or a file.

/// A queued item and the id the interface names it by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry<T> {
    /// Unique within its queue and never reused, so a tap on a row that
    /// has since moved still finds the right file.
    pub id: u64,
    /// Skipped over when choosing what to send next, without losing its
    /// place: the user wants it sent, just not yet.
    pub held: bool,
    /// What is waiting, typically an offered file and who it is for.
    pub item: T,
}

/// The files waiting to be sent, in the order they will go.
///
/// Ids come from a counter that only moves forward, even across
/// [`push_front`](Self::push_front), so an id stays unique for the
/// queue's lifetime.
#[derive(Clone, Debug)]
pub struct TransferQueue<T> {
    /// Front first: the next to be sent is the first entry not held.
    entries: Vec<Entry<T>>,
    /// The id the next [`push`](Self::push) hands out; always greater than
    /// every id in the queue.
    next_id: u64,
}

impl<T> Default for TransferQueue<T> {
    fn default() -> Self {
        Self { entries: Vec::new(), next_id: 1 }
    }
}

impl<T> TransferQueue<T> {
    /// A queue rebuilt from what was saved, front first, each with whether
    /// it was held. Ids are handed out afresh: they name rows on screen and
    /// are never saved, so nothing outside this run can hold an old one.
    pub fn restore(saved: impl IntoIterator<Item = (T, bool)>) -> Self {
        let mut queue = Self::default();
        for (item, held) in saved {
            let id = queue.push(item);
            queue.set_held(id, held);
        }
        queue
    }

    /// Adds to the back and returns the new entry's id.
    pub fn push(&mut self, item: T) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.entries.push(Entry { id, held: false, item });
        id
    }

    /// Puts an entry back at the front, keeping its id: a transfer that
    /// was interrupted goes first again rather than behind everything
    /// queued after it.
    pub fn push_front(&mut self, entry: Entry<T>) {
        self.next_id = self.next_id.max(entry.id + 1);
        self.entries.insert(0, entry);
    }

    /// How many entries are waiting, held ones included.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is waiting, held or not.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entries in sending order, front first.
    pub fn iter(&self) -> impl Iterator<Item = &Entry<T>> {
        self.entries.iter()
    }

    /// Moves an entry one place earlier. Returns whether it moved.
    pub fn move_up(&mut self, id: u64) -> bool {
        match self.position(id) {
            Some(at) if at > 0 => {
                self.entries.swap(at, at - 1);
                true
            }
            _ => false,
        }
    }

    /// Moves an entry one place later. Returns whether it moved.
    pub fn move_down(&mut self, id: u64) -> bool {
        match self.position(id) {
            Some(at) if at + 1 < self.entries.len() => {
                self.entries.swap(at, at + 1);
                true
            }
            _ => false,
        }
    }

    /// Holds an entry in place or releases it. Returns whether the id was
    /// found.
    pub fn set_held(&mut self, id: u64, held: bool) -> bool {
        match self.entries.iter_mut().find(|e| e.id == id) {
            Some(entry) => {
                entry.held = held;
                true
            }
            None => false,
        }
    }

    /// Takes an entry out of the queue.
    pub fn remove(&mut self, id: u64) -> Option<Entry<T>> {
        let at = self.position(id)?;
        Some(self.entries.remove(at))
    }

    /// Takes the first entry that is not held and that `fits` accepts
    /// (for example, one for the person currently on the call). Entries
    /// before it keep their places.
    pub fn take_next(&mut self, fits: impl Fn(&T) -> bool) -> Option<Entry<T>> {
        let at = self.entries.iter().position(|e| !e.held && fits(&e.item))?;
        Some(self.entries.remove(at))
    }

    /// Where the entry with this id is, if it is still queued.
    fn position(&self, id: u64) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(queue: &TransferQueue<&'static str>) -> Vec<&'static str> {
        queue.iter().map(|e| e.item).collect()
    }

    #[test]
    fn files_go_in_the_order_they_were_added() {
        let mut queue = TransferQueue::default();
        queue.push("a");
        queue.push("b");
        queue.push("c");
        assert_eq!(queue.take_next(|_| true).map(|e| e.item), Some("a"));
        assert_eq!(queue.take_next(|_| true).map(|e| e.item), Some("b"));
        assert_eq!(names(&queue), ["c"]);
    }

    #[test]
    fn reordering_moves_one_place_and_stops_at_the_ends() {
        let mut queue = TransferQueue::default();
        let a = queue.push("a");
        queue.push("b");
        let c = queue.push("c");
        assert!(queue.move_up(c));
        assert_eq!(names(&queue), ["a", "c", "b"]);
        assert!(!queue.move_up(a), "already first");
        assert!(queue.move_down(a));
        assert_eq!(names(&queue), ["c", "a", "b"]);
        assert!(queue.move_down(a));
        assert!(!queue.move_down(a), "already last");
        assert!(!queue.move_up(999), "no such entry");
    }

    #[test]
    fn held_entries_keep_their_place_but_are_skipped() {
        let mut queue = TransferQueue::default();
        let a = queue.push("a");
        queue.push("b");
        queue.set_held(a, true);
        assert_eq!(queue.take_next(|_| true).map(|e| e.item), Some("b"));
        assert_eq!(queue.take_next(|_| true), None);
        queue.set_held(a, false);
        assert_eq!(queue.take_next(|_| true).map(|e| e.item), Some("a"));
    }

    #[test]
    fn only_entries_that_fit_are_taken() {
        let mut queue = TransferQueue::default();
        queue.push("for-alice");
        queue.push("for-bob");
        let next = queue.take_next(|item| item.ends_with("bob"));
        assert_eq!(next.map(|e| e.item), Some("for-bob"));
        assert_eq!(names(&queue), ["for-alice"]);
    }

    #[test]
    fn a_restored_queue_keeps_order_and_holds() {
        let mut queue = TransferQueue::restore([("a", false), ("b", true), ("c", false)]);
        assert_eq!(names(&queue), ["a", "b", "c"]);
        assert_eq!(queue.iter().map(|e| e.held).collect::<Vec<_>>(), [false, true, false]);
        assert_eq!(queue.take_next(|_| true).map(|e| e.item), Some("a"));
        assert_eq!(queue.take_next(|_| true).map(|e| e.item), Some("c"), "the held one is skipped");
        let fresh = queue.push("d");
        assert!(queue.iter().filter(|e| e.id == fresh).count() == 1, "new ids do not collide");
    }

    #[test]
    fn cancelled_entries_leave_and_interrupted_ones_return_first() {
        let mut queue = TransferQueue::default();
        let a = queue.push("a");
        let b = queue.push("b");
        queue.push("c");
        assert_eq!(queue.remove(b).map(|e| e.item), Some("b"));
        assert!(queue.remove(b).is_none());
        let taken = queue.take_next(|_| true).unwrap();
        assert_eq!(taken.id, a);
        queue.push_front(taken);
        assert_eq!(names(&queue), ["a", "c"]);
        // Ids stay unique after a put-back.
        let d = queue.push("d");
        assert!(queue.iter().filter(|e| e.id == d).count() == 1);
        assert_eq!(queue.len(), 3);
    }
}
