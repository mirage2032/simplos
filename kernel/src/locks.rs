//! Advisory locks on files, as Linux keeps them. Neither kind stops a read
//! or a write: they only make whoever else asks for a lock wait, or fail.
//!
//! - `flock`'s lock a whole file, each its open file's: shared by the
//!   descriptors `dup` and `fork` make of it, and gone as the last of them
//!   closes. cargo locks its build directory with them, and rustc its
//!   incremental session.
//! - `fcntl`'s lock ranges of bytes. F_SETLK's are each its process's, and
//!   go as it closes any descriptor of the file, or ends; F_OFD_SETLK's are
//!   each its open file's, as `flock`'s are. SQLite, which cargo keeps its
//!   cache's records in, locks with them. The two kinds stand in each
//!   other's way, and not in `flock`'s, as on Linux.
//!
//! shortcut: deadlocks aren't found: two processes each waiting for a range
//! the other holds wait until a signal, where Linux tells the second
//! EDEADLK. And only files on the disk are locked: `flock` on anything else
//! succeeds, as there's no one it could be in the way of here.

use crate::thread::{Interrupted, WaitQueue};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

/// A file, as locks name it: its mount's, and its inode there
/// (`ext4::File::identity`).
pub type FileId = (usize, u32);

/// Whose a lock is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Owner {
    /// `flock`'s: an open file, by where its handle is.
    Whole(usize),
    /// F_SETLK's: a process, by its id.
    Process(u64),
    /// F_OFD_SETLK's: an open file, by where its handle is.
    Open(usize),
}

impl Owner {
    /// Whether it's a range's, of the kind `fcntl` locks.
    fn ranges(self) -> bool {
        !matches!(self, Owner::Whole(_))
    }
}

/// The end of a range that goes on to wherever the file ends, ever.
pub const TO_THE_END: u64 = u64::MAX;

#[derive(Clone, Copy, Debug)]
pub struct Lock {
    pub owner: Owner,
    pub file: FileId,
    /// The bytes it covers: from `start`, to before `end`.
    pub start: u64,
    pub end: u64,
    /// Exclusive, a write lock; or shared, a read lock.
    pub exclusive: bool,
}

impl Lock {
    /// Whether `self` can't be had while `held` is: another's, of the same
    /// kind, on bytes of the same file, and one or the other exclusive.
    fn conflicts(&self, held: &Lock) -> bool {
        self.file == held.file
            && self.owner != held.owner
            && self.owner.ranges() == held.owner.ranges()
            && self.start < held.end
            && held.start < self.end
            && (self.exclusive || held.exclusive)
    }
}

/// Every lock held. Taken with interrupts on.
static LOCKS: Mutex<Vec<Lock>> = Mutex::new(Vec::new());
/// Counted up whenever a lock goes or gets weaker, with `CHANGED` woken: for
/// whoever waits for one to.
static CHANGES: AtomicU64 = AtomicU64::new(0);
static CHANGED: WaitQueue = WaitQueue::new();

/// Why a lock wasn't had.
#[derive(Debug)]
pub enum Refused {
    /// Another's stands in the way, and it wasn't to be waited for.
    Held,
    /// A signal came while it was waited for.
    Interrupted,
}

/// The lock that stands in the way of `wanted`, if one does.
pub fn conflict(wanted: &Lock) -> Option<Lock> {
    LOCKS.lock().iter().find(|held| wanted.conflicts(held)).copied()
}

/// Have `wanted`, in place of what its owner held of those bytes before: at
/// once, if no other's stands in the way; or once none does, if `wait`. A
/// whole file's lock that changes lets go of the old one first, as Linux's
/// `flock` does, so two that each hold it shared and want it exclusive
/// don't wait for each other.
pub fn lock(wanted: Lock, wait: bool) -> Result<(), Refused> {
    if !wanted.owner.ranges() {
        let same = |held: &Lock| held.owner == wanted.owner && held.exclusive == wanted.exclusive;
        if LOCKS.lock().iter().any(|held| held.file == wanted.file && same(held)) {
            return Ok(());
        }
        unlock(wanted.owner, wanted.file, 0, TO_THE_END);
    }
    loop {
        let seen = CHANGES.load(Ordering::Acquire);
        {
            let mut locks = LOCKS.lock();
            if !locks.iter().any(|held| wanted.conflicts(held)) {
                cut(&mut locks, wanted.owner, wanted.file, wanted.start, wanted.end);
                locks.push(wanted);
                drop(locks);
                // It may be weaker than what it replaced.
                changed();
                return Ok(());
            }
        }
        if !wait {
            return Err(Refused::Held);
        }
        let waited = CHANGED.wait_until_interruptible(|| CHANGES.load(Ordering::Acquire) != seen);
        waited.map_err(|Interrupted| Refused::Interrupted)?;
    }
}

/// Let go of what `owner` holds of `file`'s bytes from `start` to before
/// `end`.
pub fn unlock(owner: Owner, file: FileId, start: u64, end: u64) {
    cut(&mut LOCKS.lock(), owner, file, start, end);
    changed();
}

/// The last descriptor of the open file whose handle is at `handle` is
/// closed: its `flock` and F_OFD_SETLK locks go.
pub fn release_open(handle: usize) {
    release(|owner| owner == Owner::Whole(handle) || owner == Owner::Open(handle));
}

/// Process `process` closed a descriptor of `file`, or `None` for each of
/// them, as it ended: its F_SETLK locks on it go, whatever other
/// descriptors of it it has, as POSIX says.
pub fn release_process(process: u64, file: Option<FileId>) {
    let mut locks = LOCKS.lock();
    let before = locks.len();
    locks.retain(|held| {
        held.owner != Owner::Process(process) || file.is_some_and(|file| file != held.file)
    });
    let gone = locks.len() != before;
    drop(locks);
    if gone {
        changed();
    }
}

fn release(of: impl Fn(Owner) -> bool) {
    let mut locks = LOCKS.lock();
    let before = locks.len();
    locks.retain(|held| !of(held.owner));
    let gone = locks.len() != before;
    drop(locks);
    if gone {
        changed();
    }
}

/// Take what `owner` holds of `file` from `start` to before `end` out of
/// `locks`, leaving what it holds on either side.
fn cut(locks: &mut Vec<Lock>, owner: Owner, file: FileId, start: u64, end: u64) {
    let mut kept = Vec::new();
    locks.retain(|held| {
        let covers =
            held.owner == owner && held.file == file && held.start < end && start < held.end;
        if covers {
            if held.start < start {
                kept.push(Lock { end: start, ..*held });
            }
            if end < held.end {
                kept.push(Lock { start: end, ..*held });
            }
        }
        !covers
    });
    locks.extend(kept);
}

fn changed() {
    CHANGES.fetch_add(1, Ordering::AcqRel);
    CHANGED.wake_all();
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: FileId = (1, 2);

    fn range(owner: Owner, start: u64, end: u64, exclusive: bool) -> Lock {
        Lock { owner, file: FILE, start, end, exclusive }
    }

    fn held_by(owner: Owner) -> Vec<(u64, u64, bool)> {
        let mut held: Vec<_> = LOCKS
            .lock()
            .iter()
            .filter(|lock| lock.owner == owner)
            .map(|lock| (lock.start, lock.end, lock.exclusive))
            .collect();
        held.sort();
        held
    }

    #[test_case]
    fn ranges_are_split_shared_and_refused_as_posix_says() {
        let (a, b) = (Owner::Process(1001), Owner::Process(1002));
        // A write lock on 0..100, then a read lock in its middle: three
        // pieces, the middle one shared.
        lock(range(a, 0, 100, true), false).unwrap();
        lock(range(a, 40, 60, false), false).unwrap();
        assert_eq!(held_by(a), [(0, 40, true), (40, 60, false), (60, 100, true)]);
        // Another may read the shared middle, but not write it, nor read
        // what's written.
        assert!(lock(range(b, 45, 55, false), false).is_ok());
        assert!(matches!(lock(range(b, 45, 55, true), false), Err(Refused::Held)));
        assert!(matches!(lock(range(b, 10, 20, false), false), Err(Refused::Held)));
        assert_eq!(conflict(&range(b, 90, TO_THE_END, false)).map(|lock| lock.start), Some(60));
        // Unlocking a hole in the middle; then all of it, as its process ends.
        unlock(a, FILE, 0, 50);
        assert_eq!(held_by(a), [(50, 60, false), (60, 100, true)]);
        release_process(1001, None);
        assert!(held_by(a).is_empty());
        assert!(lock(range(b, 0, TO_THE_END, true), false).is_ok());
        release_process(1002, Some(FILE));
        assert!(held_by(b).is_empty());
    }

    #[test_case]
    fn whole_files_are_locked_apart_from_ranges() {
        let (open, other) = (Owner::Whole(0x1000), Owner::Whole(0x2000));
        lock(range(open, 0, TO_THE_END, false), false).unwrap();
        lock(range(other, 0, TO_THE_END, false), false).unwrap();
        // Both shared, then one wants it exclusive: it lets go of its own,
        // and the other's is still in the way.
        assert!(matches!(lock(range(open, 0, TO_THE_END, true), false), Err(Refused::Held)));
        assert!(held_by(open).is_empty());
        // A range's lock isn't in the way of a whole file's.
        lock(range(Owner::Process(1003), 0, 10, true), false).unwrap();
        release_open(0x2000);
        assert!(lock(range(open, 0, TO_THE_END, true), false).is_ok());
        release_open(0x1000);
        release_process(1003, None);
        assert!(LOCKS.lock().is_empty());
    }
}
