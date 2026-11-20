//! `[auth.lockout]`: failed logins counted by account and the device they come from, and a lock
//! on a key that fails too often. The counts are in this node's memory.
//!
//! A login takes a place before it is checked and gives it back after, so that a burst of
//! logins that all start before any has failed cannot guess more often than the limit says.

use crate::config::LockoutSettings;
use crate::metrics::{write_family, Counter};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// What a failure is counted under: an account from one origin, or the account from any.
type Key = (String, Option<String>);

#[derive(Debug, Clone, Copy)]
struct Entry {
    count: u32,
    /// Logins of this key that have taken a place and are not decided yet.
    pending: u32,
    /// The first failure of the current run.
    first: Instant,
    locked_until: Option<Instant>,
    /// The last time it was used, for dropping the oldest when the table is full.
    touched: Instant,
}

impl Entry {
    fn new(now: Instant) -> Self {
        Self { count: 0, pending: 0, first: now, locked_until: None, touched: now }
    }

    fn locked(&self, now: Instant) -> bool {
        self.locked_until.is_some_and(|until| until > now)
    }

    /// By time alone, what it counted no longer counts.
    fn stale(&self, now: Instant, window: Duration) -> bool {
        match self.locked_until {
            Some(until) => until <= now,
            None => now.saturating_duration_since(self.first) > window,
        }
    }

    /// Nothing about it matters any more, and no login waits on it.
    fn expired(&self, now: Instant, window: Duration) -> bool {
        self.pending == 0 && self.stale(now, window)
    }

    /// Whether one more login can be let through: not locked, and the failures it has and the
    /// logins still being decided do not already add up to `limit`.
    fn full(&self, now: Instant, limit: u32) -> bool {
        self.locked(now) || self.count.saturating_add(self.pending) >= limit
    }
}

/// A lock that began with a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locked {
    /// The account from this origin.
    Origin,
    /// The account, from every origin.
    Account,
}

#[derive(Debug, Default)]
struct Table {
    settings: Option<LockoutSettings>,
    entries: HashMap<Key, Entry>,
    /// A full table with nothing to drop is not looked at again before this.
    full_until: Option<Instant>,
    /// When the full table was last said to be.
    full_logged: Option<Instant>,
}

/// The failure counts of the logins of this node.
#[derive(Debug, Default)]
pub struct Lockout {
    table: Mutex<Table>,
    /// Logins rejected because their key or account was locked, or had its failures coming.
    pub rejected: Counter,
    /// Locks that began.
    pub lockouts: Counter,
    /// Failures not counted because the table was full of locks.
    pub untracked: Counter,
}

/// The place of one login, until it is decided. Dropped without a decision, it only gives the
/// place back: a login that failed for a reason of the backend counts for nothing.
#[derive(Debug)]
pub struct Reservation<'a> {
    lockout: &'a Lockout,
    origin: Option<Key>,
    account: Option<Key>,
}

/// The key of an account: ASCII lowercase, so `Alice` and `alice` are one.
fn account_key(account: &str) -> String {
    account.to_ascii_lowercase()
}

impl Lockout {
    /// Applies the settings of a configuration, keeping the counts; `None` turns the limit off and
    /// forgets them.
    pub fn configure(&self, settings: Option<LockoutSettings>) {
        let mut table = self.table.lock();
        if settings.is_none() {
            table.entries.clear();
            table.full_until = None;
        }
        table.settings = settings;
    }

    pub fn enabled(&self) -> bool {
        self.table.lock().settings.is_some()
    }

    /// Takes a place for a login of `account` from `origin`, or `None`, counting it as rejected,
    /// when its key or its account is locked or already has as many logins coming as it may fail.
    pub fn admit(&self, account: &str, origin: &str, now: Instant) -> Option<Reservation<'_>> {
        let mut table = self.table.lock();
        let Some(settings) = table.settings else {
            return Some(Reservation { lockout: self, origin: None, account: None });
        };
        let account = account_key(account);
        let origin_key = (account.clone(), Some(origin.to_string()));
        let account_key = (account, None);
        let limits = [(&origin_key, settings.max_failures), (&account_key, settings.account_max_failures.unwrap_or(u32::MAX))];
        for (key, limit) in limits {
            if table.entries.get(key).is_some_and(|entry| entry.expired(now, settings.window)) {
                table.entries.remove(key);
            }
            if table.entries.get(key).is_some_and(|entry| entry.full(now, limit)) {
                self.rejected.inc();
                return None;
            }
        }
        let origin = self.reserve(&mut table, origin_key, &settings, now);
        let account = settings.account_max_failures.and_then(|_| self.reserve(&mut table, account_key, &settings, now));
        Some(Reservation { lockout: self, origin, account })
    }

    /// One more login waiting on `key`; `None` when the table has no room for it, which is counted.
    fn reserve(&self, table: &mut Table, key: Key, settings: &LockoutSettings, now: Instant) -> Option<Key> {
        if !table.entries.contains_key(&key) && !self.make_room(table, settings, now) {
            self.untracked.inc();
            if table.full_logged.is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(60)) {
                table.full_logged = Some(now);
                tracing::warn!("auth lockout: the table of {} keys is full of locks; failures of new keys are not counted", settings.max_entries);
            }
            return None;
        }
        let entry = table.entries.entry(key.clone()).or_insert_with(|| Entry::new(now));
        entry.pending += 1;
        entry.touched = now;
        Some(key)
    }

    /// Gives back a place; an entry with nothing left in it goes.
    fn release(table: &mut Table, key: &Key) {
        if let Some(entry) = table.entries.get_mut(key) {
            entry.pending = entry.pending.saturating_sub(1);
            if entry.pending == 0 && entry.count == 0 {
                table.entries.remove(key);
            }
        }
    }

    /// One failure under `key`; says when it locks it. A key the table had no room for is left out.
    fn count(table: &mut Table, key: &Key, limit: u32, settings: &LockoutSettings, now: Instant) -> bool {
        let Some(entry) = table.entries.get_mut(key) else { return false };
        entry.touched = now;
        if entry.locked(now) {
            return false;
        }
        if entry.count > 0 && entry.stale(now, settings.window) {
            entry.count = 0;
            entry.locked_until = None;
        }
        if entry.count == 0 {
            entry.first = now;
        }
        entry.count += 1;
        if entry.count >= limit {
            entry.locked_until = Some(now + settings.lock);
            return true;
        }
        false
    }

    /// Room for one more entry: drops the expired, then the oldest that are not locked and have
    /// no login waiting, a tenth of the table at once. False when nothing can go.
    fn make_room(&self, table: &mut Table, settings: &LockoutSettings, now: Instant) -> bool {
        if table.entries.len() < settings.max_entries {
            return true;
        }
        if table.full_until.is_some_and(|until| until > now) {
            return false;
        }
        table.entries.retain(|_, entry| !entry.expired(now, settings.window));
        if table.entries.len() >= settings.max_entries {
            let mut unlocked: Vec<(Instant, Key)> = table.entries.iter().filter(|(_, entry)| !entry.locked(now) && entry.pending == 0).map(|(key, entry)| (entry.touched, key.clone())).collect();
            unlocked.sort_unstable_by_key(|(touched, _)| *touched);
            for (_, key) in unlocked.into_iter().take(settings.max_entries / 10 + 1) {
                table.entries.remove(&key);
            }
        }
        if table.entries.len() >= settings.max_entries {
            table.full_until = Some(now + Duration::from_secs(1));
            return false;
        }
        true
    }

    /// The counters in the Prometheus text format; nothing without `[auth.lockout]`.
    pub fn render(&self, out: &mut String) {
        if !self.enabled() {
            return;
        }
        write_family(out, "rradius_auth_lockout_rejected_total", "counter", "Logins rejected because their account and device, or their account, was locked or had too many logins coming.", &[("", self.rejected.get())]);
        write_family(out, "rradius_auth_lockouts_total", "counter", "Locks that began.", &[("", self.lockouts.get())]);
        write_family(out, "rradius_auth_lockout_untracked_total", "counter", "Failures not counted because the table was full of locks.", &[("", self.untracked.get())]);
    }
}

impl Reservation<'_> {
    /// The login was refused for a wrong password or an unknown account: counts a failure and
    /// says which lock it began, if any.
    pub fn fail(mut self, now: Instant) -> Option<Locked> {
        let (origin, account) = (self.origin.take(), self.account.take());
        let mut table = self.lockout.table.lock();
        let settings = table.settings?;
        let mut began = None;
        if let Some(key) = &origin {
            Lockout::release(&mut table, key);
            // `release` may have dropped an entry that counted nothing yet; it is counted again here.
            if !table.entries.contains_key(key) && self.lockout.make_room(&mut table, &settings, now) {
                table.entries.insert(key.clone(), Entry::new(now));
            }
            if Lockout::count(&mut table, key, settings.max_failures, &settings, now) {
                began = Some(Locked::Origin);
            }
        }
        if let (Some(key), Some(limit)) = (&account, settings.account_max_failures) {
            Lockout::release(&mut table, key);
            if !table.entries.contains_key(key) && self.lockout.make_room(&mut table, &settings, now) {
                table.entries.insert(key.clone(), Entry::new(now));
            }
            if Lockout::count(&mut table, key, limit, &settings, now) {
                began = began.or(Some(Locked::Account));
            }
        }
        if began.is_some() {
            self.lockout.lockouts.inc();
        }
        began
    }

    /// The login was accepted: the failures of its key are forgotten.
    pub fn succeed(mut self) {
        let (origin, account) = (self.origin.take(), self.account.take());
        let mut table = self.lockout.table.lock();
        if let Some(key) = &origin {
            if let Some(entry) = table.entries.get_mut(key) {
                entry.count = 0;
                entry.locked_until = None;
            }
            Lockout::release(&mut table, key);
        }
        if let Some(key) = &account {
            Lockout::release(&mut table, key);
        }
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.origin.is_none() && self.account.is_none() {
            return;
        }
        let mut table = self.lockout.table.lock();
        for key in [self.origin.take(), self.account.take()].into_iter().flatten() {
            Lockout::release(&mut table, &key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> LockoutSettings {
        LockoutSettings { max_failures: 10, window: Duration::from_secs(300), lock: Duration::from_secs(300), max_entries: 100_000, account_max_failures: None }
    }

    fn lockout(settings: LockoutSettings) -> Lockout {
        let lockout = Lockout::default();
        lockout.configure(Some(settings));
        lockout
    }

    const DEVICE: &str = "aa-bb-cc-dd-ee-01";

    impl Lockout {
        /// A login that fails at once.
        fn failed(&self, account: &str, origin: &str, now: Instant) -> Option<Locked> {
            self.admit(account, origin, now)?.fail(now)
        }

        /// Whether a login would be turned away; it takes no place.
        fn locked(&self, account: &str, origin: &str, now: Instant) -> bool {
            self.admit(account, origin, now).is_none()
        }

        fn succeeded(&self, account: &str, origin: &str) {
            if let Some(reservation) = self.admit(account, origin, Instant::now()) {
                reservation.succeed();
            }
        }
    }

    /// The tenth failure in the window locks the key; the lock ends after `lock`, and a right
    /// password then clears the count.
    #[test]
    fn test_locked_after_the_tenth_failure_and_unlocked_later() {
        let lockout = lockout(settings());
        let start = Instant::now();
        for _ in 0..9 {
            assert_eq!(lockout.failed("alice", DEVICE, start), None);
        }
        assert!(!lockout.locked("alice", DEVICE, start));
        assert_eq!(lockout.failed("alice", DEVICE, start), Some(Locked::Origin));
        assert!(lockout.locked("alice", DEVICE, start + Duration::from_secs(299)));
        assert_eq!(lockout.rejected.get(), 1);
        assert!(!lockout.locked("alice", DEVICE, start + Duration::from_secs(300)));
        lockout.succeeded("alice", DEVICE);
        for _ in 0..9 {
            assert_eq!(lockout.failed("alice", DEVICE, start + Duration::from_secs(301)), None);
        }
    }

    #[test]
    fn test_failures_after_the_window_start_again() {
        let lockout = lockout(settings());
        let start = Instant::now();
        for _ in 0..9 {
            lockout.failed("alice", DEVICE, start);
        }
        assert_eq!(lockout.failed("alice", DEVICE, start + Duration::from_secs(301)), None);
        assert!(!lockout.locked("alice", DEVICE, start + Duration::from_secs(301)));
        // Nine more after the new first one are needed to lock it.
        for _ in 0..8 {
            assert_eq!(lockout.failed("alice", DEVICE, start + Duration::from_secs(302)), None);
        }
        assert_eq!(lockout.failed("alice", DEVICE, start + Duration::from_secs(302)), Some(Locked::Origin));
    }

    #[test]
    fn test_success_clears_and_another_device_is_free() {
        let lockout = lockout(settings());
        let start = Instant::now();
        for _ in 0..9 {
            lockout.failed("alice", DEVICE, start);
        }
        lockout.succeeded("alice", DEVICE);
        for _ in 0..9 {
            assert_eq!(lockout.failed("alice", DEVICE, start), None);
        }
        for _ in 0..10 {
            lockout.failed("bob", DEVICE, start);
        }
        assert!(lockout.locked("bob", DEVICE, start));
        assert!(!lockout.locked("bob", "aa-bb-cc-dd-ee-02", start));
        // Names differ only in case: one account.
        assert!(lockout.locked("BOB", DEVICE, start));
    }

    #[test]
    fn test_account_ceiling_locks_every_origin() {
        let lockout = lockout(LockoutSettings { account_max_failures: Some(30), ..settings() });
        let start = Instant::now();
        let mut began = None;
        for n in 0..30 {
            began = lockout.failed("alice", &format!("device-{n}"), start);
        }
        assert_eq!(began, Some(Locked::Account));
        assert!(lockout.locked("alice", "a device never seen", start));
        assert!(!lockout.locked("bob", "a device never seen", start));
    }

    #[test]
    fn test_off_counts_and_locks_nothing() {
        let lockout = Lockout::default();
        let start = Instant::now();
        for _ in 0..1000 {
            assert_eq!(lockout.failed("alice", DEVICE, start), None);
        }
        assert!(!lockout.locked("alice", DEVICE, start));
        let mut text = String::new();
        lockout.render(&mut text);
        assert_eq!(text, "");
        // Turned off, the counts are forgotten.
        lockout.configure(Some(settings()));
        for _ in 0..9 {
            lockout.failed("alice", DEVICE, start);
        }
        lockout.configure(None);
        lockout.configure(Some(settings()));
        assert_eq!(lockout.failed("alice", DEVICE, start), None);
        assert!(!lockout.locked("alice", DEVICE, start));
    }

    /// A full table drops expired entries, then the oldest ones that are not locked, and keeps every
    /// lock; when all are locked a new key is not tracked.
    #[test]
    fn test_full_table() {
        let lockout = lockout(LockoutSettings { max_entries: 20, max_failures: 2, ..settings() });
        let start = Instant::now();
        // Ten locked keys, then ten that are only counted.
        for n in 0..10 {
            lockout.failed(&format!("locked-{n}"), DEVICE, start);
            lockout.failed(&format!("locked-{n}"), DEVICE, start);
        }
        for n in 0..10 {
            lockout.failed(&format!("one-{n}"), DEVICE, start + Duration::from_secs(1 + n));
        }
        // A new key finds the table full: the oldest unlocked go, no lock does.
        lockout.failed("new", DEVICE, start + Duration::from_secs(20));
        for n in 0..10 {
            assert!(lockout.locked(&format!("locked-{n}"), DEVICE, start + Duration::from_secs(20)), "locked-{n}");
        }
        assert_eq!(lockout.untracked.get(), 0);
        // Everything locked: the next new key is not tracked.
        let full = lockout_with_all_locked();
        assert_eq!(full.untracked.get(), 5, "{} untracked", full.untracked.get());
        assert!(full.locked("locked-0", DEVICE, Instant::now()));
    }

    fn lockout_with_all_locked() -> Lockout {
        let lockout = lockout(LockoutSettings { max_entries: 10, max_failures: 1, ..settings() });
        let start = Instant::now();
        for n in 0..10 {
            lockout.failed(&format!("locked-{n}"), DEVICE, start);
        }
        for n in 0..5 {
            assert_eq!(lockout.failed(&format!("new-{n}"), DEVICE, start), None);
        }
        lockout
    }

    #[test]
    fn test_expired_entries_make_room() {
        let lockout = lockout(LockoutSettings { max_entries: 10, max_failures: 1, ..settings() });
        let start = Instant::now();
        for n in 0..10 {
            lockout.failed(&format!("locked-{n}"), DEVICE, start);
        }
        // The locks have ended: a new key is counted, and the old ones dropped.
        assert_eq!(lockout.failed("fresh", DEVICE, start + Duration::from_secs(400)), Some(Locked::Origin));
        assert_eq!(lockout.untracked.get(), 0);
    }

    /// The full table is said once in a minute, not for every failure.
    #[tokio::test]
    async fn test_full_table_is_logged_once_a_minute() {
        let log = crate::test_support::radius::logged_during(async {
            let lockout = lockout(LockoutSettings { max_entries: 2, max_failures: 1, ..settings() });
            let start = Instant::now();
            for n in 0..2 {
                lockout.failed(&format!("locked-{n}"), DEVICE, start);
            }
            for n in 0..5 {
                lockout.failed(&format!("new-{n}"), DEVICE, start + Duration::from_secs(n));
            }
            assert_eq!(lockout.untracked.get(), 5);
            lockout.failed("later", DEVICE, start + Duration::from_secs(61));
        })
        .await;
        let lines: Vec<&str> = log.lines().filter(|line| line.contains("is full of locks")).collect();
        assert_eq!(lines.len(), 2, "{log}");
    }

    #[test]
    fn test_metrics_text() {
        let lockout = lockout(LockoutSettings { max_failures: 1, ..settings() });
        let start = Instant::now();
        lockout.failed("alice", DEVICE, start);
        lockout.locked("alice", DEVICE, start);
        lockout.locked("alice", DEVICE, start);
        let mut text = String::new();
        lockout.render(&mut text);
        for line in ["rradius_auth_lockouts_total 1", "rradius_auth_lockout_rejected_total 2", "rradius_auth_lockout_untracked_total 0"] {
            assert!(text.contains(line), "{line}: {text}");
        }
    }

    /// Logins that start before any is decided cannot together guess more often than the limit.
    #[test]
    fn test_logins_in_flight_count_against_the_limit() {
        let lockout = lockout(LockoutSettings { max_failures: 3, ..settings() });
        let now = Instant::now();
        let started: Vec<_> = (0..50).filter_map(|_| lockout.admit("alice", DEVICE, now)).collect();
        assert_eq!(started.len(), 3);
        assert_eq!(lockout.rejected.get(), 47);
        // One that ends without a decision gives its place back; a failure counts.
        let mut started = started.into_iter();
        drop(started.next());
        let replacement = lockout.admit("alice", DEVICE, now);
        assert!(replacement.is_some());
        for reservation in started {
            reservation.fail(now);
        }
        // Two failures and the one place just taken: the next is turned away until that is decided.
        assert!(lockout.admit("alice", DEVICE, now).is_none());
    }

    /// An entry with a login waiting is not dropped to make room.
    #[test]
    fn test_a_waiting_login_keeps_its_entry() {
        let lockout = lockout(LockoutSettings { max_entries: 2, ..settings() });
        let now = Instant::now();
        let waiting = lockout.admit("a", DEVICE, now);
        let other = lockout.admit("b", DEVICE, now);
        assert!(waiting.is_some() && other.is_some());
        let later = now + Duration::from_secs(1000);
        // The table is full of entries that wait; a new key is not counted rather than evicting one.
        assert!(lockout.admit("c", DEVICE, later).is_some());
        assert_eq!(lockout.untracked.get(), 1);
        waiting.map(|reservation| reservation.fail(later));
        assert!(lockout.table.lock().entries.contains_key(&("a".to_string(), Some(DEVICE.to_string()))));
    }
}
