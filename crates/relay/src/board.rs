//! Community boards on a mailbox node (spec §11 "Communities at full
//! scale"; the protocol is [`antiphon_net::board`]).
//!
//! A board is an append-only log of sealed entries for one community. A
//! member uploads a message for everyone once, and every member reads it
//! from here, so sending to a community of thousands costs one upload
//! rather than one per member.
//!
//! What the node holds per board is: a random id, the hashes of the
//! access tokens of the community's last few MLS epochs, and numbered
//! sealed entries with the time each arrived. It can't read the entries,
//! can't tell who posted or fetched them beyond an address, and doesn't
//! know who the members are: whoever holds a current token is one. A
//! member removed from the community never learns the next epoch's token,
//! so they lose access as soon as the old epochs age out of the window.
//!
//! Limits, all set by the operator through [`BoardLimits`]: how many
//! boards, bytes per board (the oldest entries go first when a board is
//! full), how long entries are kept, posts per board per minute, and how
//! long an untouched board lives.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use antiphon_net::board::{token_hash, BoardId, Message, Token, MAX_PAGE};
use antiphon_net::mailbox::Refusal;

/// How many epochs' tokens a board accepts at once. Members who haven't
/// yet seen the newest commit keep working for this many epochs.
pub const TOKEN_WINDOW: usize = 16;

#[derive(Clone, Debug)]
pub struct BoardLimits {
    /// Most boards held at once.
    pub max_boards: usize,
    /// Bytes one board may hold; past it the oldest entries are dropped.
    pub board_cap: u64,
    /// How long an entry is kept.
    pub entry_expiry: Duration,
    /// How long a board nobody posts to or reads from is kept.
    pub board_expiry: Duration,
    /// Posts per board per minute.
    pub posts_per_minute: usize,
}

impl Default for BoardLimits {
    fn default() -> Self {
        Self {
            max_boards: 10_000,
            board_cap: 8 * 1024 * 1024,
            entry_expiry: Duration::from_secs(30 * 86_400),
            board_expiry: Duration::from_secs(60 * 86_400),
            posts_per_minute: 600,
        }
    }
}

struct Entry {
    seq: u64,
    sealed: Vec<u8>,
    arrived: Instant,
}

struct Board {
    /// Hashes of the tokens let in, oldest first.
    allowed: VecDeque<[u8; 32]>,
    entries: VecDeque<Entry>,
    /// The newest entry's number (entries are numbered from 1).
    last: u64,
    bytes: u64,
    recent: VecDeque<Instant>,
    touched: Instant,
}

impl Board {
    fn lets_in(&self, token: &Token) -> bool {
        let hash = token_hash(token);
        // Every hash is compared, so timing doesn't say which one matched.
        self.allowed.iter().fold(false, |found, h| found | constant_time_eq(h, &hash))
    }

    fn allow(&mut self, hash: [u8; 32]) {
        if !self.allowed.contains(&hash) {
            self.allowed.push_back(hash);
            while self.allowed.len() > TOKEN_WINDOW {
                self.allowed.pop_front();
            }
        }
    }

    fn drop_oldest(&mut self) -> u64 {
        match self.entries.pop_front() {
            Some(e) => {
                self.bytes -= e.sealed.len() as u64;
                e.sealed.len() as u64
            }
            None => 0,
        }
    }
}

/// Every board on this node.
pub struct Boards {
    limits: BoardLimits,
    boards: HashMap<BoardId, Board>,
    bytes: u64,
}

impl Boards {
    pub fn new(limits: BoardLimits) -> Self {
        Self { limits, boards: HashMap::new(), bytes: 0 }
    }

    /// Bytes held across all boards, which count toward the node's
    /// storage cap together with inbox mail.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn count(&self) -> usize {
        self.boards.len()
    }

    /// Answers one board request. `room` is how many more bytes the node
    /// as a whole may store.
    pub fn answer(&mut self, request: Message, room: u64, now: Instant) -> Vec<Message> {
        let one = |m| vec![m];
        match request {
            Message::Open { board, token_hash } => {
                if let Some(b) = self.boards.get_mut(&board) {
                    // Opening again is only for someone already let in, so
                    // a stranger who learns an id can't add themselves.
                    return if b.allowed.contains(&token_hash) {
                        b.touched = now;
                        one(Message::Opened { board })
                    } else {
                        one(Message::Refused(Refusal::WrongSecret))
                    };
                }
                if self.boards.len() >= self.limits.max_boards {
                    return one(Message::Refused(Refusal::Full));
                }
                self.boards.insert(
                    board,
                    Board {
                        allowed: VecDeque::from([token_hash]),
                        entries: VecDeque::new(),
                        last: 0,
                        bytes: 0,
                        recent: VecDeque::new(),
                        touched: now,
                    },
                );
                one(Message::Opened { board })
            }
            Message::Rekey { board, token, new_hash } => match self.member(&board, &token, now) {
                Err(r) => one(Message::Refused(r)),
                Ok(b) => {
                    b.allow(new_hash);
                    one(Message::Rekeyed { board })
                }
            },
            Message::Post { board, token, entry } => {
                let posts_per_minute = self.limits.posts_per_minute;
                let cap = self.limits.board_cap;
                let size = entry.len() as u64;
                let b = match self.member(&board, &token, now) {
                    Err(r) => return one(Message::Refused(r)),
                    Ok(b) => b,
                };
                while b.recent.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60)) {
                    b.recent.pop_front();
                }
                if b.recent.len() >= posts_per_minute {
                    return one(Message::Refused(Refusal::RateLimited));
                }
                // A full board makes room by forgetting its oldest entries;
                // a full node refuses instead, so one board can't push out
                // everyone else's mail.
                let mut freed = 0;
                while b.bytes + size > cap && !b.entries.is_empty() {
                    freed += b.drop_oldest();
                }
                if size > room.saturating_add(freed) {
                    self.bytes -= freed;
                    return one(Message::Refused(Refusal::Full));
                }
                b.recent.push_back(now);
                b.last += 1;
                b.bytes += size;
                b.entries.push_back(Entry { seq: b.last, sealed: entry, arrived: now });
                let seq = b.last;
                self.bytes = self.bytes - freed + size;
                one(Message::Posted { board, seq })
            }
            Message::Fetch { board, token, since, max } => match self.member(&board, &token, now) {
                Err(r) => one(Message::Refused(r)),
                Ok(b) => {
                    let mut out: Vec<Message> = b
                        .entries
                        .iter()
                        .filter(|e| e.seq > since)
                        .take(max.clamp(1, MAX_PAGE) as usize)
                        .map(|e| Message::Entry { seq: e.seq, entry: e.sealed.clone() })
                        .collect();
                    let seqs = || out.iter().filter_map(|m| if let Message::Entry { seq, .. } = m { Some(*seq) } else { None });
                    let first = seqs().next().unwrap_or(0);
                    let next = seqs().next_back().unwrap_or(b.last);
                    out.push(Message::Done { first, next, last: b.last });
                    out
                }
            },
            // Replies arriving at the node are noise.
            _ => Vec::new(),
        }
    }

    fn member(&mut self, board: &BoardId, token: &Token, now: Instant) -> Result<&mut Board, Refusal> {
        let b = self.boards.get_mut(board).ok_or(Refusal::UnknownLabel)?;
        if !b.lets_in(token) {
            return Err(Refusal::WrongSecret);
        }
        b.touched = now;
        Ok(b)
    }

    /// Drops expired entries, and boards untouched for too long.
    pub fn sweep(&mut self, now: Instant) {
        let (entry_expiry, board_expiry) = (self.limits.entry_expiry, self.limits.board_expiry);
        let mut freed = 0;
        self.boards.retain(|_, b| {
            while b.entries.front().is_some_and(|e| now.duration_since(e.arrived) > entry_expiry) {
                freed += b.drop_oldest();
            }
            if now.duration_since(b.touched) > board_expiry {
                freed += b.bytes;
                return false;
            }
            true
        });
        self.bytes -= freed;
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOARD: BoardId = [7; 32];
    const OLD: Token = [1; 32];

    fn boards(limits: BoardLimits) -> Boards {
        let mut b = Boards::new(limits);
        let now = Instant::now();
        assert_eq!(b.answer(Message::Open { board: BOARD, token_hash: token_hash(&OLD) }, u64::MAX, now), vec![Message::Opened { board: BOARD }]);
        b
    }

    fn post(b: &mut Boards, token: Token, entry: &[u8], now: Instant) -> Message {
        b.answer(Message::Post { board: BOARD, token, entry: entry.to_vec() }, u64::MAX, now).remove(0)
    }

    fn fetch(b: &mut Boards, token: Token, since: u64, max: u16) -> Vec<Message> {
        b.answer(Message::Fetch { board: BOARD, token, since, max }, u64::MAX, Instant::now())
    }

    #[test]
    fn members_post_once_and_read_in_pages() {
        let mut b = boards(BoardLimits::default());
        let now = Instant::now();
        for i in 1..=5u8 {
            assert_eq!(post(&mut b, OLD, &[i; 10], now), Message::Posted { board: BOARD, seq: i as u64 });
        }
        let page = fetch(&mut b, OLD, 1, 2);
        assert_eq!(
            page,
            vec![
                Message::Entry { seq: 2, entry: vec![2; 10] },
                Message::Entry { seq: 3, entry: vec![3; 10] },
                Message::Done { first: 2, next: 3, last: 5 },
            ]
        );
        assert_eq!(fetch(&mut b, OLD, 5, 64), vec![Message::Done { first: 0, next: 5, last: 5 }]);
        assert_eq!(b.bytes(), 50);
    }

    #[test]
    fn only_current_epochs_get_in() {
        let mut b = boards(BoardLimits::default());
        let now = Instant::now();
        // A stranger with a guessed token, or who learned the id, is out.
        assert_eq!(post(&mut b, [9; 32], b"x", now), Message::Refused(Refusal::WrongSecret));
        assert_eq!(
            b.answer(Message::Open { board: BOARD, token_hash: token_hash(&[9; 32]) }, u64::MAX, now),
            vec![Message::Refused(Refusal::WrongSecret)]
        );
        // A member moves the board on, epoch by epoch; the oldest epoch
        // falls out of the window, and with it anyone stuck there.
        let mut current = OLD;
        for epoch in 2..=(TOKEN_WINDOW as u8 + 1) {
            let new = [epoch; 32];
            let reply = b.answer(Message::Rekey { board: BOARD, token: current, new_hash: token_hash(&new) }, u64::MAX, now);
            assert_eq!(reply, vec![Message::Rekeyed { board: BOARD }]);
            current = new;
        }
        assert_eq!(post(&mut b, current, b"x", now), Message::Posted { board: BOARD, seq: 1 });
        assert_eq!(post(&mut b, OLD, b"x", now), Message::Refused(Refusal::WrongSecret));
        assert_eq!(post(&mut b, [2; 32], b"x", now), Message::Posted { board: BOARD, seq: 2 });
    }

    #[test]
    fn a_full_board_forgets_its_oldest_and_a_full_node_refuses() {
        let mut b = boards(BoardLimits { board_cap: 25, ..Default::default() });
        let now = Instant::now();
        for _ in 0..3 {
            post(&mut b, OLD, &[0; 10], now);
        }
        assert_eq!(b.bytes(), 20);
        assert!(matches!(fetch(&mut b, OLD, 0, 64)[0], Message::Entry { seq: 2, .. }));
        let reply = b.answer(Message::Post { board: BOARD, token: OLD, entry: vec![0; 30] }, 5, now);
        assert_eq!(reply, vec![Message::Refused(Refusal::Full)]);
        assert_eq!(b.bytes(), 0, "the board still made what room it could");
    }

    #[test]
    fn posts_are_rate_limited_and_old_things_expire() {
        let mut b = boards(BoardLimits { posts_per_minute: 2, ..Default::default() });
        let now = Instant::now();
        post(&mut b, OLD, b"a", now);
        post(&mut b, OLD, b"b", now);
        assert_eq!(post(&mut b, OLD, b"c", now), Message::Refused(Refusal::RateLimited));
        assert!(matches!(post(&mut b, OLD, b"c", now + Duration::from_secs(61)), Message::Posted { seq: 3, .. }));

        b.sweep(now + Duration::from_secs(31 * 86_400));
        assert_eq!((b.count(), b.bytes()), (1, 0));
        assert_eq!(fetch(&mut b, OLD, 0, 64), vec![Message::Done { first: 0, next: 3, last: 3 }]);
        b.sweep(Instant::now() + Duration::from_secs(61 * 86_400));
        assert_eq!(b.count(), 0);
    }
}
