//! Accounting requests the cluster cannot take right now, kept in files on this node until
//! the database works again. The NAS gets its answer once a request is on disk, so this
//! closes the gap between a short outage and the few retransmissions a NAS makes.
//!
//! The directory holds segment files of records (length, CRC-32, content) that are read in
//! order, a `position` file saying how far replay has come, and `rejected.log` for records
//! the script refused when replayed. A crash between a replay and saving the position runs
//! that record again, so replay is at least once.

use crate::metrics::SpoolCounters;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::IpAddr;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

/// How many records replay may finish before the position goes to disk. A power cut replays
/// at most these again, as long as the file survives; flushing after each would add a flush to every record.
const POSITION_SYNC_RECORDS: u32 = 64;

/// A record's length and CRC-32.
const HEADER_LEN: u64 = 8;
/// Where one segment file ends and the next begins. Space comes back a whole file at a time.
const SEGMENT_LEN: u64 = 16 * 1024 * 1024;
/// Far more than any record: a length above this is damage, not a record.
const MAX_RECORD_LEN: u32 = 64 * 1024;
/// Requests waiting to be written at once; further ones wait to get in.
const WRITE_QUEUE: usize = 4096;

/// One accounting request as received.
#[derive(Debug, Clone, PartialEq)]
pub struct SpooledRequest {
    pub client: String,
    pub source: IpAddr,
    /// Unix time it arrived.
    pub received: u64,
    pub packet: Vec<u8>,
    /// It came over RadSec, whose secret is not the client's own.
    pub radsec: bool,
}

impl SpooledRequest {
    fn encode(&self) -> Vec<u8> {
        let source = self.source.to_string();
        let mut content = Vec::with_capacity(self.client.len() + source.len() + self.packet.len() + 11);
        content.extend_from_slice(&(self.client.len() as u16).to_be_bytes());
        content.extend_from_slice(self.client.as_bytes());
        content.push(source.len() as u8);
        content.extend_from_slice(source.as_bytes());
        content.extend_from_slice(&self.received.to_be_bytes());
        content.push(u8::from(self.radsec));
        content.extend_from_slice(&self.packet);
        let mut record = Vec::with_capacity(content.len() + HEADER_LEN as usize);
        record.extend_from_slice(&(content.len() as u32).to_be_bytes());
        record.extend_from_slice(&crc32(&content).to_be_bytes());
        record.extend_from_slice(&content);
        record
    }

    fn decode(content: &[u8]) -> Option<Self> {
        let (length, rest) = content.split_first_chunk::<2>()?;
        let (client, rest) = rest.split_at_checked(usize::from(u16::from_be_bytes(*length)))?;
        let (length, rest) = rest.split_first()?;
        let (source, rest) = rest.split_at_checked(usize::from(*length))?;
        let (received, rest) = rest.split_first_chunk::<8>()?;
        let (radsec, packet) = rest.split_first()?;
        Some(Self {
            client: std::str::from_utf8(client).ok()?.to_string(),
            source: std::str::from_utf8(source).ok()?.parse().ok()?,
            received: u64::from_be_bytes(*received),
            packet: packet.to_vec(),
            radsec: *radsec != 0,
        })
    }
}

/// CRC-32 (IEEE), enough to tell a damaged record from a sound one.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

#[derive(Debug, PartialEq)]
pub enum SpoolError {
    Full,
    Io(String),
}

impl std::fmt::Display for SpoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpoolError::Full => f.write_str("it is full"),
            SpoolError::Io(e) => f.write_str(e),
        }
    }
}

/// Whether `dir` could be used, for a check of the configuration: `Spool::open` makes it and
/// repairs what is in it. Goes by what writing a file here does, leaving nothing behind.
pub fn check_dir(dir: &Path) -> io::Result<()> {
    let mut nearest = dir;
    loop {
        match fs::metadata(nearest) {
            Ok(_) => break,
            // A link to nothing is not nothing: the server could not make the directory there.
            Err(e) if e.kind() == io::ErrorKind::NotFound && fs::symlink_metadata(nearest).is_ok() => return Err(e),
            Err(e) if e.kind() == io::ErrorKind::NotFound => match nearest.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => nearest = parent,
                Some(_) | None => {
                    nearest = Path::new(".");
                }
            },
            Err(e) => return Err(e),
        }
    }
    // The server lists what is waiting there.
    if nearest == dir {
        fs::read_dir(dir)?;
    }
    // Under a file this fails too.
    let probe = nearest.join(format!(".rradius-check-{}", std::process::id()));
    OpenOptions::new().write(true).create_new(true).mode(0o600).open(&probe)?;
    fs::remove_file(&probe)
}

/// What reading a record found at the read position.
enum Found {
    Record(Vec<u8>),
    EndOfFile,
    Damaged,
}

fn read_record(file: &mut File, offset: u64, file_len: u64) -> io::Result<Found> {
    if offset == file_len {
        return Ok(Found::EndOfFile);
    }
    if offset + HEADER_LEN > file_len {
        return Ok(Found::Damaged);
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut header = [0u8; HEADER_LEN as usize];
    file.read_exact(&mut header)?;
    let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let crc = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    // No record is empty: the CRC-32 of nothing is 0, so a power cut's zeroes would pass for one.
    if length == 0 || length > MAX_RECORD_LEN || offset + HEADER_LEN + u64::from(length) > file_len {
        return Ok(Found::Damaged);
    }
    let mut content = vec![0u8; length as usize];
    file.read_exact(&mut content)?;
    if crc32(&content) != crc {
        return Ok(Found::Damaged);
    }
    Ok(Found::Record(content))
}

struct Files {
    dir: PathBuf,
    max_bytes: u64,
    /// Segment numbers still holding records, oldest first; the last one takes new records.
    segments: VecDeque<u64>,
    writing: File,
    writing_len: u64,
    /// How much of the newest file is known to be on disk.
    synced_len: u64,
    /// The newest file has bytes past `writing_len` that could not be removed yet.
    tail_damaged: bool,
    /// A flush failed outside `flush`, and what it covers has not been told.
    flush_failed: Option<String>,
    read_offset: u64,
    /// Records finished since the position last went to disk.
    unsynced_records: u32,
    #[cfg(test)]
    position_flushes: u32,
    /// Bytes of records written and not replayed.
    pending: u64,
    /// Where a file ends and the next begins.
    segment_len: u64,
    #[cfg(test)]
    faults: Faults,
}

/// Disk trouble for tests to ask for, and what reached the disk.
#[cfg(test)]
#[derive(Default)]
struct Faults {
    /// The next write stops halfway.
    partial_write: bool,
    /// The next flush fails.
    fail_sync: bool,
    /// The next this many cuts fail.
    fail_set_len: u32,
    /// The segments flushed, in order.
    synced: Vec<u64>,
}

/// The segment and offset replay had reached. With no file, nothing has been replayed. A file
/// that cannot be understood means starting again from the oldest segment, so it is said out loud.
fn read_position(dir: &Path) -> (u64, u64) {
    let path = dir.join("position");
    let unreadable = match fs::read_to_string(&path) {
        Ok(text) => match parse_position(&text) {
            Some(position) => return position,
            None => "it holds no position".to_string(),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (0, 0),
        Err(e) => e.to_string(),
    };
    tracing::error!("accounting spool: cannot use '{}' ({unreadable}); replaying the oldest segment from its beginning, which runs requests again", path.display());
    (0, 0)
}

fn parse_position(text: &str) -> Option<(u64, u64)> {
    let (segment, offset) = text.trim().split_once(' ')?;
    Some((segment.parse().ok()?, offset.parse().ok()?))
}

fn segment_path(dir: &Path, number: u64) -> PathBuf {
    dir.join(format!("segment-{number:020}.log"))
}

fn open_for_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).mode(0o600).open(path)
}

impl Files {
    fn open(dir: &Path, max_bytes: u64) -> io::Result<Self> {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let mut numbers: Vec<u64> = fs::read_dir(dir)?
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.strip_prefix("segment-")?.strip_suffix(".log")?.parse().ok())
            .collect();
        numbers.sort_unstable();
        let (read_segment, mut read_offset) = read_position(dir);
        // Replay had finished with these, and a crash came before they were removed.
        for number in numbers.iter().filter(|number| **number < read_segment) {
            fs::remove_file(segment_path(dir, *number))?;
        }
        numbers.retain(|number| *number >= read_segment);
        if numbers.first() != Some(&read_segment) {
            read_offset = 0;
        }
        if numbers.is_empty() {
            numbers.push(read_segment.max(1));
        }
        let last = *numbers.last().unwrap_or(&1);
        let damaged = Self::drop_torn_tail(&segment_path(dir, last), if numbers.len() == 1 { read_offset } else { 0 })?;
        let writing = open_for_append(&segment_path(dir, last))?;
        let writing_len = writing.metadata()?.len();
        // The file itself, and the directory if this just made it, have to survive a crash too.
        File::open(dir)?.sync_all()?;
        if let Some(parent) = dir.parent() {
            let _ = File::open(parent).and_then(|parent| parent.sync_all());
        }
        let mut pending = 0;
        for number in &numbers {
            pending += fs::metadata(segment_path(dir, *number))?.len();
        }
        let mut files = Self { dir: dir.to_path_buf(), max_bytes, segments: numbers.into(), writing, writing_len, synced_len: writing_len, tail_damaged: false, flush_failed: None, read_offset, unsynced_records: 0, pending: pending.saturating_sub(read_offset), segment_len: SEGMENT_LEN, #[cfg(test)] position_flushes: 0, #[cfg(test)] faults: Faults::default() };
        // Replay will set that file aside, and what is answered from now on must not go with it.
        if damaged {
            files.start_segment()?;
        }
        Ok(files)
    }

    /// Removes what a crash in the middle of a write left at the end of the newest file.
    /// That record was never answered, so the NAS still has it. Damage anywhere else is
    /// left for replay to find, and `true` says there is some.
    fn drop_torn_tail(path: &Path, from: u64) -> io::Result<bool> {
        let Ok(mut file) = OpenOptions::new().read(true).write(true).open(path) else { return Ok(false) };
        let file_len = file.metadata()?.len();
        let mut offset = from.min(file_len);
        loop {
            match read_record(&mut file, offset, file_len)? {
                Found::Record(content) => offset += HEADER_LEN + content.len() as u64,
                Found::EndOfFile => return Ok(false),
                Found::Damaged => {
                    if !Self::reaches_the_end(&mut file, offset, file_len)? {
                        return Ok(true);
                    }
                    let kept = match Self::keep_what_is_cut(&mut file, path, offset).and_then(|kept| file.set_len(offset).map(|()| kept)) {
                        Ok(kept) => kept,
                        Err(e) => {
                            // A full disk must not stop the server. This takes no room: the file is set aside whole.
                            crate::lua_engine::log_line(tracing::Level::ERROR, format!("accounting spool: could not remove an unfinished write from {}, the file will be set aside: {e}", path.display()));
                            return Ok(true);
                        }
                    };
                    let kept = if kept { ", kept in truncated.log" } else { "" };
                    crate::lua_engine::log_line(tracing::Level::WARN, format!("accounting spool: removed {} octets of an unfinished write from {}{kept}", file_len - offset, path.display()));
                    return Ok(false);
                }
            }
        }
    }

    /// Whether the damaged record at `offset` is the last thing in the file. A length no
    /// record can have is damage to a record that was whole, and more may follow it.
    fn reaches_the_end(file: &mut File, offset: u64, file_len: u64) -> io::Result<bool> {
        if offset + HEADER_LEN > file_len {
            return Ok(true);
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut length = [0u8; 4];
        file.read_exact(&mut length)?;
        let length = u32::from_be_bytes(length);
        if length == 0 {
            // A power cut's zeroes reach the end; zeroes with something after them are damage.
            let mut rest = Vec::new();
            file.read_to_end(&mut rest)?;
            return Ok(rest.iter().all(|byte| *byte == 0));
        }
        Ok(length <= MAX_RECORD_LEN && offset + HEADER_LEN + u64::from(length) >= file_len)
    }

    /// A write cut short and a damaged length that still fits a record look the same, and the
    /// second would hold answered requests, so what is about to go is kept for someone to read.
    /// Says whether there was anything to keep.
    fn keep_what_is_cut(file: &mut File, path: &Path, offset: u64) -> io::Result<bool> {
        file.seek(SeekFrom::Start(offset))?;
        let mut removed = Vec::new();
        file.read_to_end(&mut removed)?;
        if removed.iter().all(|byte| *byte == 0) {
            return Ok(false);
        }
        let mut kept = open_for_append(&path.with_file_name("truncated.log"))?;
        kept.write_all(&removed)?;
        kept.sync_data()?;
        // The file may be new, and what it saves is about to go.
        if let Some(dir) = path.parent() {
            File::open(dir)?.sync_all()?;
        }
        Ok(true)
    }

    fn append(&mut self, record: &[u8]) -> Result<(), SpoolError> {
        let io_error = |e: io::Error| SpoolError::Io(e.to_string());
        if self.pending + record.len() as u64 > self.max_bytes {
            return Err(SpoolError::Full);
        }
        if self.tail_damaged {
            self.repair_tail().map_err(io_error)?;
        }
        if self.writing_len >= self.segment_len {
            self.start_segment().map_err(io_error)?;
        }
        if let Err(e) = self.write_out(record) {
            // What is answered later must not land after half a record.
            self.cut_to(self.writing_len);
            return Err(io_error(e));
        }
        self.writing_len += record.len() as u64;
        self.pending += record.len() as u64;
        Ok(())
    }

    /// Makes `len` the end of the newest file, now or as soon as the disk lets it.
    fn cut_to(&mut self, len: u64) {
        self.pending = self.pending.saturating_sub(self.writing_len.saturating_sub(len));
        self.writing_len = len;
        self.tail_damaged = self.cut_writing(len).is_err();
    }

    fn cut_writing(&mut self, len: u64) -> io::Result<()> {
        #[cfg(test)]
        if self.faults.fail_set_len > 0 {
            self.faults.fail_set_len -= 1;
            return Err(io::Error::other("the disk failed"));
        }
        self.writing.set_len(len)
    }

    /// A new file does as well as a clean end: replay stops at the damage in the old one, and
    /// nothing after the damage was answered.
    fn repair_tail(&mut self) -> io::Result<()> {
        if self.cut_writing(self.writing_len).is_err() {
            // The flush that comes before the answer only reaches the newest file.
            self.sync().inspect_err(|e| self.flush_failed = Some(e.to_string()))?;
            self.start_segment()?;
        }
        self.tail_damaged = false;
        Ok(())
    }

    fn write_out(&mut self, record: &[u8]) -> io::Result<()> {
        #[cfg(test)]
        if std::mem::take(&mut self.faults.partial_write) {
            self.writing.write_all(&record[..record.len() / 2])?;
            return Err(io::Error::other("no space left on device"));
        }
        self.writing.write_all(record)
    }

    fn sync(&mut self) -> io::Result<()> {
        #[cfg(test)]
        {
            if std::mem::take(&mut self.faults.fail_sync) {
                return Err(io::Error::other("the disk failed"));
            }
            self.faults.synced.extend(self.segments.back());
        }
        self.writing.sync_data()
    }

    /// Writes the records of one batch and flushes them, saying how each one went.
    fn append_batch<'a>(&mut self, records: impl Iterator<Item = &'a [u8]>) -> Vec<Result<(), SpoolError>> {
        let mut results = Vec::new();
        let mut not_flushed = 0;
        for record in records {
            // Before leaving a file: the flush below only reaches the newest one.
            if self.writing_len >= self.segment_len {
                self.flush(&mut results[not_flushed..]);
                not_flushed = results.len();
            }
            results.push(self.append(record));
        }
        self.flush(&mut results[not_flushed..]);
        results
    }

    /// Answered only after this: an answer means the NAS will not send it again. A failure
    /// takes back all that was not flushed, since the NAS will send it again.
    fn flush(&mut self, results: &mut [Result<(), SpoolError>]) {
        // A disk that failed a flush can report the next one as fine with the data gone.
        let failed = self.flush_failed.take();
        if failed.is_none() && self.synced_len == self.writing_len {
            return;
        }
        match failed.map_or_else(|| self.sync(), |e| Err(io::Error::other(e))) {
            Ok(()) => self.synced_len = self.writing_len,
            Err(e) => {
                for result in results.iter_mut().filter(|result| result.is_ok()) {
                    *result = Err(SpoolError::Io(e.to_string()));
                }
                self.cut_to(self.synced_len);
            }
        }
    }

    fn start_segment(&mut self) -> io::Result<()> {
        let next = self.segments.back().map_or(1, |last| last + 1);
        self.writing = open_for_append(&segment_path(&self.dir, next))?;
        self.writing_len = 0;
        self.synced_len = 0;
        self.tail_damaged = false;
        self.segments.push_back(next);
        File::open(&self.dir)?.sync_all()
    }

    /// The next record to replay, moving past finished files and damaged ones. `corrupt`
    /// counts the damaged ones.
    fn next(&mut self, corrupt: &mut u64) -> io::Result<Option<(SpooledRequest, u64)>> {
        loop {
            let Some(&segment) = self.segments.front() else { return Ok(None) };
            let path = segment_path(&self.dir, segment);
            let mut file = File::open(&path)?;
            let file_len = file.metadata()?.len();
            let found = read_record(&mut file, self.read_offset, file_len)?;
            let is_newest = self.segments.len() == 1;
            match found {
                Found::Record(content) => match SpooledRequest::decode(&content) {
                    Some(request) => return Ok(Some((request, HEADER_LEN + content.len() as u64))),
                    None => self.set_aside(segment, corrupt)?,
                },
                Found::EndOfFile if is_newest => return Ok(None),
                Found::EndOfFile => {
                    self.finish_segment(segment)?;
                }
                Found::Damaged => self.set_aside(segment, corrupt)?,
            }
        }
    }

    /// Up to `max` records from the start, without moving past them: the first as `next`
    /// finds it, then those after it in the same file. A record that cannot be read whole
    /// ends the batch, and is left for `next` to deal with.
    fn next_batch(&mut self, max: usize, corrupt: &mut u64) -> io::Result<Vec<(SpooledRequest, u64)>> {
        let Some(first) = self.next(corrupt)? else { return Ok(Vec::new()) };
        let mut offset = self.read_offset + first.1;
        let mut batch = vec![first];
        let Some(&segment) = self.segments.front().filter(|_| max > 1) else { return Ok(batch) };
        let mut file = File::open(segment_path(&self.dir, segment))?;
        let file_len = file.metadata()?.len();
        while batch.len() < max {
            let Found::Record(content) = read_record(&mut file, offset, file_len)? else { break };
            let Some(request) = SpooledRequest::decode(&content) else { break };
            let len = HEADER_LEN + content.len() as u64;
            batch.push((request, len));
            offset += len;
        }
        Ok(batch)
    }

    /// Keeps a damaged file under another name, and goes on with the next one.
    fn set_aside(&mut self, segment: u64, corrupt: &mut u64) -> io::Result<()> {
        *corrupt += 1;
        if self.segments.len() == 1 {
            self.start_segment()?;
        }
        let path = segment_path(&self.dir, segment);
        fs::rename(&path, path.with_extension("corrupt"))?;
        self.segments.pop_front();
        self.read_offset = 0;
        // Counted from the files: this one can hold bytes that never counted.
        let mut pending = 0;
        for number in &self.segments {
            pending += fs::metadata(segment_path(&self.dir, *number))?.len();
        }
        self.pending = pending;
        self.save_position(true)
    }

    fn finish_segment(&mut self, segment: u64) -> io::Result<()> {
        self.segments.pop_front();
        self.read_offset = 0;
        self.save_position(true)?;
        fs::remove_file(segment_path(&self.dir, segment))
    }

    /// Records that `records` records of `len` bytes in all, from the read position, are done
    /// with, saving the position once.
    fn advance_many(&mut self, len: u64, records: u64) -> io::Result<()> {
        self.read_offset += len;
        self.pending = self.pending.saturating_sub(len);
        self.unsynced_records = self.unsynced_records.saturating_add(u32::try_from(records).unwrap_or(u32::MAX));
        self.save_position(self.unsynced_records >= POSITION_SYNC_RECORDS)
    }

    /// Replaces the position, which a crash of this process cannot undo. Only `to_disk` also
    /// survives a power cut.
    fn save_position(&mut self, to_disk: bool) -> io::Result<()> {
        let segment = self.segments.front().copied().unwrap_or(0);
        let temporary = self.dir.join("position.new");
        let mut file = OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&temporary)?;
        file.write_all(format!("{segment} {}\n", self.read_offset).as_bytes())?;
        if to_disk {
            file.sync_data()?;
        }
        fs::rename(temporary, self.dir.join("position"))?;
        if to_disk {
            File::open(&self.dir)?.sync_all()?;
            self.unsynced_records = 0;
            #[cfg(test)]
            {
                self.position_flushes += 1;
            }
        }
        Ok(())
    }

    fn reject(&mut self, request: &SpooledRequest) -> io::Result<()> {
        let mut file = open_for_append(&self.dir.join("rejected.log"))?;
        file.write_all(&request.encode())?;
        file.sync_data()
    }
}

struct Waiting {
    record: Vec<u8>,
    done: oneshot::Sender<Result<(), SpoolError>>,
}

/// The files of one node, written by one task so that requests arriving together share one
/// flush to disk.
pub struct Spool {
    files: Arc<Mutex<Files>>,
    queue: mpsc::Sender<Waiting>,
    counters: Arc<SpoolCounters>,
}

impl Spool {
    /// Opens or creates the directory, and starts the task writing to it.
    pub fn open(dir: &Path, max_bytes: u64, counters: Arc<SpoolCounters>) -> io::Result<Self> {
        let files = Arc::new(Mutex::new(Files::open(dir, max_bytes)?));
        counters.bytes.set(files.lock().pending);
        let (queue, waiting) = mpsc::channel(WRITE_QUEUE);
        tokio::spawn(write_batches(files.clone(), waiting, counters.clone()));
        Ok(Self { files, queue, counters })
    }

    /// Returns once `request` is on disk, or why it is not.
    pub async fn write(&self, request: &SpooledRequest) -> Result<(), SpoolError> {
        let (done, finished) = oneshot::channel();
        let stopped = || SpoolError::Io("the spool writer stopped".to_string());
        let result = match self.queue.send(Waiting { record: request.encode(), done }).await {
            Ok(()) => finished.await.unwrap_or_else(|_| Err(stopped())),
            Err(_) => Err(stopped()),
        };
        match &result {
            Ok(()) => self.counters.written.inc(),
            Err(_) => self.counters.full.inc(),
        }
        result
    }

    /// The next request to replay, and how to say it is done with.
    pub async fn next(&self) -> io::Result<Option<(SpooledRequest, u64)>> {
        Ok(self.next_batch(1).await?.into_iter().next())
    }

    /// Up to `max` requests from the start of the spool, in the order written, none past the
    /// end of the current file. They stay in the spool until `advance` or `reject` is called
    /// for each, in that order.
    pub async fn next_batch(&self, max: usize) -> io::Result<Vec<(SpooledRequest, u64)>> {
        let files = self.files.clone();
        let (found, corrupt, pending, dir) = tokio::task::spawn_blocking(move || {
            let mut corrupt = 0;
            let mut files = files.lock();
            let found = files.next_batch(max, &mut corrupt);
            (found, corrupt, files.pending, files.dir.clone())
        })
        .await
        .map_err(io::Error::other)?;
        self.counters.bytes.set(pending);
        if corrupt > 0 {
            self.counters.corrupt.add(corrupt);
            crate::lua_engine::log_line(tracing::Level::ERROR, format!("accounting spool: {corrupt} damaged files in {} set aside with the extension .corrupt", dir.display()));
        }
        found
    }

    /// Marks the request `next` returned as done with.
    pub async fn advance(&self, len: u64) -> io::Result<()> {
        self.advance_many(&[len]).await
    }

    /// Marks the requests `next_batch` returned as done with, the first `lens.len()` of them,
    /// whose lengths these are, saving the position once.
    pub async fn advance_many(&self, lens: &[u64]) -> io::Result<()> {
        if lens.is_empty() {
            return Ok(());
        }
        let (len, records) = (lens.iter().sum::<u64>(), lens.len() as u64);
        let files = self.files.clone();
        let bytes = tokio::task::spawn_blocking(move || {
            let mut files = files.lock();
            files.advance_many(len, records).map(|()| files.pending)
        })
        .await
        .map_err(io::Error::other)??;
        self.counters.bytes.set(bytes);
        Ok(())
    }

    /// Moves the request `next` returned to `rejected.log`.
    pub async fn reject(&self, request: SpooledRequest, len: u64) -> io::Result<()> {
        let files = self.files.clone();
        tokio::task::spawn_blocking(move || files.lock().reject(&request)).await.map_err(io::Error::other)??;
        self.counters.rejected.inc();
        self.advance(len).await
    }
}

async fn write_batches(files: Arc<Mutex<Files>>, mut queue: mpsc::Receiver<Waiting>, counters: Arc<SpoolCounters>) {
    let mut batch = Vec::new();
    while queue.recv_many(&mut batch, WRITE_QUEUE).await > 0 {
        let waiting = std::mem::take(&mut batch);
        let files = files.clone();
        let written = tokio::task::spawn_blocking(move || {
            let mut files = files.lock();
            let results = files.append_batch(waiting.iter().map(|one| one.record.as_slice()));
            (waiting, results, files.pending)
        })
        .await;
        let Ok((waiting, results, pending)) = written else { continue };
        counters.bytes.set(pending);
        for (one, result) in waiting.into_iter().zip(results) {
            let _ = one.done.send(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn request(id: u8) -> SpooledRequest {
        SpooledRequest { client: "nas1".to_string(), source: "10.0.0.1".parse().unwrap(), received: 1000 + u64::from(id), packet: vec![id; 30], radsec: false }
    }

    fn spool(dir: &Path) -> Spool {
        Spool::open(dir, 1 << 30, Arc::new(SpoolCounters::default())).unwrap()
    }

    async fn drain(spool: &Spool) -> Vec<u8> {
        let mut seen = Vec::new();
        while let Some((request, len)) = spool.next().await.unwrap() {
            seen.push(request.packet[0]);
            spool.advance(len).await.unwrap();
        }
        seen
    }

    fn only_segment(dir: &Path) -> PathBuf {
        let mut segments: Vec<_> = fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().path()).filter(|path| path.extension().is_some_and(|e| e == "log") && path.file_name().unwrap().to_str().unwrap().starts_with("segment")).collect();
        segments.sort();
        assert_eq!(segments.len(), 1, "{segments:?}");
        segments.remove(0)
    }

    #[test]
    fn test_crc32_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn test_request_round_trip() {
        for source in ["10.0.0.1", "2001:db8::1"] {
            for radsec in [false, true] {
                let original = SpooledRequest { source: source.parse().unwrap(), radsec, ..request(1) };
                let record = original.encode();
                assert_eq!(SpooledRequest::decode(&record[HEADER_LEN as usize..]), Some(original));
            }
        }
        assert_eq!(SpooledRequest::decode(&[]), None);
        assert_eq!(SpooledRequest::decode(&[0, 200, 1]), None);
    }

    #[tokio::test]
    async fn test_written_in_order_and_survives_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let spool_dir = dir.path().join("spool");
        {
            let spool = spool(&spool_dir);
            for id in 1..=3 {
                spool.write(&request(id)).await.unwrap();
            }
            let (first, len) = spool.next().await.unwrap().unwrap();
            assert_eq!(first, request(1));
            spool.advance(len).await.unwrap();
        }
        // As after a restart: what was done with stays done with, the rest comes in order.
        let reopened = spool(&spool_dir);
        assert_eq!(drain(&reopened).await, [2, 3]);
        assert_eq!(drain(&reopened).await, Vec::<u8>::new());
    }

    #[tokio::test]
    async fn test_position_reaches_the_disk_every_so_often() {
        let dir = tempfile::tempdir().unwrap();
        let spool = spool(dir.path());
        for id in 0..=u8::try_from(POSITION_SYNC_RECORDS).unwrap() {
            spool.write(&request(id)).await.unwrap();
        }
        let before = spool.files.lock().position_flushes;
        for _ in 1..POSITION_SYNC_RECORDS {
            let (_, len) = spool.next().await.unwrap().unwrap();
            spool.advance(len).await.unwrap();
        }
        assert_eq!(spool.files.lock().position_flushes, before, "not for each record");
        let (_, len) = spool.next().await.unwrap().unwrap();
        spool.advance(len).await.unwrap();
        assert_eq!(spool.files.lock().position_flushes, before + 1);
        let (_, len) = spool.next().await.unwrap().unwrap();
        spool.advance(len).await.unwrap();
        assert_eq!(spool.files.lock().position_flushes, before + 1, "counted from the last one");
    }

    struct LogWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_parse_position() {
        assert_eq!(parse_position("3 128\n"), Some((3, 128)));
        for damaged in ["", "\0\0\0", "3", "3 x", "x 128", "-1 0"] {
            assert_eq!(parse_position(damaged), None, "{damaged:?}");
        }
    }

    #[tokio::test]
    async fn test_damaged_position_replays_from_the_beginning() {
        let dir = tempfile::tempdir().unwrap();
        {
            let spool = spool(dir.path());
            spool.write(&request(1)).await.unwrap();
            spool.write(&request(2)).await.unwrap();
            let (_, len) = spool.next().await.unwrap().unwrap();
            spool.advance(len).await.unwrap();
        }
        fs::write(dir.path().join("position"), "").unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let written = log.clone();
        let subscriber = tracing_subscriber::fmt().with_max_level(tracing::Level::ERROR).with_writer(move || LogWriter(written.clone())).finish();
        let reopened = tracing::subscriber::with_default(subscriber, || spool(dir.path()));
        assert_eq!(drain(&reopened).await, [1, 2], "again, which is better than losing them");
        let text = String::from_utf8(log.lock().clone()).unwrap();
        assert!(text.contains("cannot use") && text.contains("position"), "{text}");
    }

    #[tokio::test]
    async fn test_not_done_with_comes_again_after_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        {
            let spool = spool(dir.path());
            spool.write(&request(1)).await.unwrap();
            spool.next().await.unwrap().unwrap();
        }
        assert_eq!(drain(&spool(dir.path())).await, [1]);
    }

    #[tokio::test]
    async fn test_torn_write() {
        let dir = tempfile::tempdir().unwrap();
        {
            let spool = spool(dir.path());
            spool.write(&request(1)).await.unwrap();
            spool.write(&request(2)).await.unwrap();
        }
        let path = only_segment(dir.path());
        let len = fs::metadata(&path).unwrap().len();
        OpenOptions::new().write(true).open(&path).unwrap().set_len(len - 5).unwrap();
        let reopened = spool(dir.path());
        assert_eq!(drain(&reopened).await, [1], "the torn record was never answered");
        assert_eq!(reopened.counters.corrupt.get(), 0);
        reopened.write(&request(3)).await.unwrap();
        assert_eq!(drain(&reopened).await, [3], "the file is sound again");
    }

    #[tokio::test]
    async fn test_corrupt_record() {
        let dir = tempfile::tempdir().unwrap();
        {
            let spool = spool(dir.path());
            for id in 1..=3 {
                spool.write(&request(id)).await.unwrap();
            }
        }
        let path = only_segment(dir.path());
        let mut bytes = fs::read(&path).unwrap();
        let record_len = request(1).encode().len();
        // A flipped bit in the middle record's packet.
        bytes[record_len + record_len / 2] ^= 1;
        fs::write(&path, bytes).unwrap();
        let reopened = spool(dir.path());
        assert_eq!(drain(&reopened).await, [1], "the rest of the file cannot be trusted");
        assert_eq!(reopened.counters.corrupt.get(), 1);
        assert!(path.with_extension("corrupt").exists());
        reopened.write(&request(4)).await.unwrap();
        assert_eq!(drain(&reopened).await, [4], "writing goes on in a new file");
    }

    /// The bytes that were replayed from `files`, as an identifying first packet byte each.
    fn replay_all(files: &mut Files) -> (Vec<u8>, u64) {
        let mut corrupt = 0;
        let mut seen = Vec::new();
        while let Some((request, len)) = files.next(&mut corrupt).unwrap() {
            seen.push(request.packet[0]);
            let advanced = files.advance_many(len, 1);
            assert!(advanced.is_ok(), "{advanced:?}");
        }
        (seen, corrupt)
    }

    #[test]
    fn test_batch_across_two_files_flushes_both() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Files::open(dir.path(), u64::MAX).unwrap();
        let (one, two) = (request(1).encode(), request(2).encode());
        files.segment_len = one.len() as u64;
        let results = files.append_batch([one.as_slice(), two.as_slice()].into_iter());
        assert_eq!(results, [Ok(()), Ok(())]);
        assert_eq!(files.faults.synced, [1, 2], "the first file holds an answered record too");
    }

    #[test]
    fn test_failed_flush_before_a_new_file_fails_what_was_in_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Files::open(dir.path(), u64::MAX).unwrap();
        let (one, two) = (request(1).encode(), request(2).encode());
        files.segment_len = one.len() as u64;
        files.faults.fail_sync = true;
        let results = files.append_batch([one.as_slice(), two.as_slice()].into_iter());
        assert!(matches!(results[0], Err(SpoolError::Io(_))), "{results:?}");
        assert_eq!(results[1], Ok(()));
        assert_eq!(replay_all(&mut files), (vec![2], 0), "only what was answered");
    }

    #[test]
    fn test_failed_write_leaves_the_file_sound() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut files = Files::open(dir.path(), u64::MAX).unwrap();
            let batch = |files: &mut Files, id: u8| files.append_batch([request(id).encode().as_slice()].into_iter()).remove(0);
            assert_eq!(batch(&mut files, 1), Ok(()));
            files.faults.partial_write = true;
            assert!(matches!(batch(&mut files, 2), Err(SpoolError::Io(_))));
            assert_eq!(batch(&mut files, 3), Ok(()));
            assert_eq!(files.pending, 2 * request(1).encode().len() as u64, "the failed one is not waiting");
        }
        let mut reopened = Files::open(dir.path(), u64::MAX).unwrap();
        assert_eq!(replay_all(&mut reopened), (vec![1, 3], 0), "what was answered after the failure is not lost");
    }

    #[test]
    fn test_new_file_after_a_failed_repair_flushes_the_old_one_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Files::open(dir.path(), u64::MAX).unwrap();
        let (one, two, three) = (request(1).encode(), request(2).encode(), request(3).encode());
        assert_eq!(files.append(&one), Ok(()));
        files.faults.partial_write = true;
        files.faults.fail_set_len = 2;
        assert!(matches!(files.append(&two), Err(SpoolError::Io(_))));
        assert_eq!(files.append(&three), Ok(()));
        assert_eq!(files.faults.synced, [1], "the first record is answered with this batch, and the flush only reaches the new file");
        let mut results = [Ok(()), Ok(())];
        files.flush(&mut results);
        assert_eq!(files.faults.synced, [1, 2]);
    }

    #[test]
    fn test_flush_failing_during_a_repair_fails_what_came_before() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Files::open(dir.path(), u64::MAX).unwrap();
        let (one, two, three, four) = (request(1).encode(), request(2).encode(), request(3).encode(), request(4).encode());
        assert_eq!(files.append(&one), Ok(()));
        files.faults.partial_write = true;
        files.faults.fail_set_len = 2;
        assert!(matches!(files.append(&two), Err(SpoolError::Io(_))));
        files.faults.fail_sync = true;
        assert!(matches!(files.append(&three), Err(SpoolError::Io(_))));
        let mut results = [Ok(())];
        // This flush may well succeed, which says nothing of what the failed one lost.
        files.flush(&mut results);
        assert!(matches!(results[0], Err(SpoolError::Io(_))), "the first record is not answered");
        assert_eq!(files.append_batch([four.as_slice()].into_iter()), [Ok(())]);
        assert_eq!(replay_all(&mut files), (vec![4], 0), "later requests are written and replayed");
    }

    #[test]
    fn test_flush_failing_during_a_repair_counts_after_the_next_file_too() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Files::open(dir.path(), u64::MAX).unwrap();
        let (one, two, three, four) = (request(1).encode(), request(2).encode(), request(3).encode(), request(4).encode());
        assert_eq!(files.append(&one), Ok(()));
        files.faults.partial_write = true;
        files.faults.fail_set_len = 2;
        assert!(matches!(files.append(&two), Err(SpoolError::Io(_))));
        files.faults.fail_sync = true;
        assert!(matches!(files.append(&three), Err(SpoolError::Io(_))));
        // The next repair gets as far as a new file, whose record fails, so that one has nothing to flush.
        files.faults.fail_set_len = 1;
        files.faults.partial_write = true;
        assert!(matches!(files.append(&four), Err(SpoolError::Io(_))));
        assert_eq!(files.writing_len, files.synced_len);
        let mut results = [Ok(())];
        files.flush(&mut results);
        assert!(matches!(results[0], Err(SpoolError::Io(_))), "the first record is not answered");
    }

    #[test]
    fn test_failed_flush_takes_the_records_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Files::open(dir.path(), u64::MAX).unwrap();
        let batch = |files: &mut Files, id: u8| files.append_batch([request(id).encode().as_slice()].into_iter()).remove(0);
        assert_eq!(batch(&mut files, 1), Ok(()));
        files.faults.fail_sync = true;
        assert!(matches!(batch(&mut files, 2), Err(SpoolError::Io(_))));
        assert_eq!(batch(&mut files, 3), Ok(()));
        assert_eq!(replay_all(&mut files), (vec![1, 3], 0), "the NAS sends the failed one again");
    }

    #[tokio::test]
    async fn test_disk_failure_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        let spool = spool(dir.path());
        spool.files.lock().faults.fail_sync = true;
        assert!(matches!(spool.write(&request(1)).await, Err(SpoolError::Io(_))));
        assert_eq!(spool.counters.full.get(), 1);
        assert_eq!(spool.counters.written.get(), 0);
    }

    #[tokio::test]
    async fn test_zeroes_after_the_last_record_are_no_records() {
        for zeroes in [1, 8, 9, 64] {
            let dir = tempfile::tempdir().unwrap();
            {
                let spool = spool(dir.path());
                spool.write(&request(1)).await.unwrap();
                spool.write(&request(2)).await.unwrap();
            }
            let path = only_segment(dir.path());
            let mut bytes = fs::read(&path).unwrap();
            let whole = bytes.len() as u64;
            bytes.extend(vec![0; zeroes]);
            fs::write(&path, bytes).unwrap();
            let reopened = spool(dir.path());
            assert_eq!(fs::metadata(&path).unwrap().len(), whole, "{zeroes}");
            reopened.write(&request(3)).await.unwrap();
            assert_eq!(drain(&reopened).await, [1, 2, 3], "{zeroes}");
            assert_eq!(reopened.counters.corrupt.get(), 0, "{zeroes}");
            assert!(!dir.path().join("truncated.log").exists(), "zeroes are nothing to keep");
        }
    }

    #[tokio::test]
    async fn test_zeroes_before_a_record_are_damage() {
        let dir = tempfile::tempdir().unwrap();
        {
            let spool = spool(dir.path());
            spool.write(&request(1)).await.unwrap();
        }
        let path = only_segment(dir.path());
        let mut bytes = fs::read(&path).unwrap();
        bytes.extend(vec![0; 16]);
        bytes.extend(request(2).encode());
        fs::write(&path, &bytes).unwrap();
        let reopened = spool(dir.path());
        assert_eq!(drain(&reopened).await, [1]);
        assert_eq!(reopened.counters.corrupt.get(), 1);
        assert_eq!(fs::read(path.with_extension("corrupt")).unwrap(), bytes, "all of it is kept");
    }

    #[tokio::test]
    async fn test_what_a_cut_removes_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        {
            let spool = spool(dir.path());
            for id in 1..=3 {
                spool.write(&request(id)).await.unwrap();
            }
        }
        let path = only_segment(dir.path());
        let mut bytes = fs::read(&path).unwrap();
        let record_len = request(1).encode().len();
        let removed = bytes[record_len..].to_vec();
        // The middle record's length is still one a record can have, and reaches past the end.
        let length = u32::from_be_bytes(bytes[record_len..record_len + 4].try_into().unwrap()) + (2 * record_len) as u32;
        bytes[record_len..record_len + 4].copy_from_slice(&length.to_be_bytes());
        fs::write(&path, &bytes).unwrap();
        let reopened = spool(dir.path());
        assert_eq!(drain(&reopened).await, [1]);
        let kept = fs::read(dir.path().join("truncated.log")).unwrap();
        assert_eq!(kept[4..], removed[4..], "the other records can still be read from it");
        assert_eq!(fs::metadata(dir.path().join("truncated.log")).unwrap().permissions().mode() & 0o777, 0o600);
        let logged = crate::test_support::logged_errors().lock().unwrap().clone();
        assert!(logged.iter().any(|line| line.contains(path.to_str().unwrap()) && line.contains("kept in truncated.log")), "someone is told where it went");
    }

    #[tokio::test]
    async fn test_startup_survives_what_cannot_be_kept() {
        let dir = tempfile::tempdir().unwrap();
        {
            let spool = spool(dir.path());
            for id in 1..=3 {
                spool.write(&request(id)).await.unwrap();
            }
        }
        let path = only_segment(dir.path());
        let mut bytes = fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 3);
        fs::write(&path, &bytes).unwrap();
        // Writing here fails, as it would on a full disk.
        fs::create_dir(dir.path().join("truncated.log")).unwrap();
        let reopened = spool(dir.path());
        assert_eq!(fs::metadata(&path).unwrap().len(), bytes.len() as u64, "nothing is cut that was not kept");
        reopened.write(&request(4)).await.unwrap();
        assert_eq!(drain(&reopened).await, [1, 2, 4]);
        assert_eq!(reopened.counters.corrupt.get(), 1);
        assert_eq!(fs::read(path.with_extension("corrupt")).unwrap(), bytes, "all of it is kept");
        let logged = crate::test_support::logged_errors().lock().unwrap().clone();
        assert!(logged.iter().any(|line| line.contains(path.to_str().unwrap()) && line.contains("could not")), "{logged:?}");
    }

    #[tokio::test]
    async fn test_damaged_length_is_not_taken_for_a_torn_write() {
        let dir = tempfile::tempdir().unwrap();
        {
            let spool = spool(dir.path());
            for id in 1..=3 {
                spool.write(&request(id)).await.unwrap();
            }
        }
        let path = only_segment(dir.path());
        let mut bytes = fs::read(&path).unwrap();
        let original_len = bytes.len() as u64;
        // The middle record's length now reaches past the end of the file.
        bytes[request(1).encode().len()] = 1;
        fs::write(&path, bytes).unwrap();
        let reopened = spool(dir.path());
        assert_eq!(drain(&reopened).await, [1]);
        assert_eq!(reopened.counters.corrupt.get(), 1);
        assert_eq!(fs::metadata(path.with_extension("corrupt")).unwrap().len(), original_len, "all of it is kept");
        assert_eq!(reopened.counters.bytes.get(), 0, "nothing is waiting any more");
    }

    #[tokio::test]
    async fn test_written_after_damage_is_not_lost_with_it() {
        let dir = tempfile::tempdir().unwrap();
        {
            let spool = spool(dir.path());
            for id in 1..=3 {
                spool.write(&request(id)).await.unwrap();
            }
        }
        let path = only_segment(dir.path());
        let mut bytes = fs::read(&path).unwrap();
        let record_len = request(1).encode().len();
        bytes[record_len + record_len / 2] ^= 1;
        fs::write(&path, bytes).unwrap();
        let reopened = spool(dir.path());
        // Answered before replay gets to the damage.
        reopened.write(&request(4)).await.unwrap();
        assert_eq!(drain(&reopened).await, [1, 4]);
    }

    #[tokio::test]
    async fn test_spool_full() {
        let dir = tempfile::tempdir().unwrap();
        let record_len = request(1).encode().len() as u64;
        let spool = Spool::open(dir.path(), record_len * 2, Arc::new(SpoolCounters::default())).unwrap();
        spool.write(&request(1)).await.unwrap();
        spool.write(&request(2)).await.unwrap();
        assert_eq!(spool.write(&request(3)).await, Err(SpoolError::Full));
        assert_eq!(spool.counters.full.get(), 1);
        assert_eq!(spool.counters.bytes.get(), record_len * 2);
        // Replaying makes room again.
        let (_, len) = spool.next().await.unwrap().unwrap();
        spool.advance(len).await.unwrap();
        spool.write(&request(3)).await.unwrap();
    }

    #[tokio::test]
    async fn test_file_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let spool_dir = dir.path().join("spool");
        let spool = spool(&spool_dir);
        spool.write(&request(1)).await.unwrap();
        let (found, len) = spool.next().await.unwrap().unwrap();
        spool.reject(found, len).await.unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&spool_dir), 0o700);
        for entry in fs::read_dir(&spool_dir).unwrap() {
            let path = entry.unwrap().path();
            assert_eq!(mode(&path), 0o600, "{path:?}");
        }
    }

    #[tokio::test]
    async fn test_rejected_record_is_kept_and_passed() {
        let dir = tempfile::tempdir().unwrap();
        let spool = spool(dir.path());
        spool.write(&request(1)).await.unwrap();
        spool.write(&request(2)).await.unwrap();
        let (found, len) = spool.next().await.unwrap().unwrap();
        spool.reject(found, len).await.unwrap();
        assert_eq!(spool.counters.rejected.get(), 1);
        assert_eq!(drain(&spool).await, [2]);
        assert!(fs::metadata(dir.path().join("rejected.log")).unwrap().len() > 0);
    }

    #[tokio::test]
    async fn test_next_batch() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let spool = Spool::open(dir.path(), 1 << 30, Arc::new(SpoolCounters::default()))?;
        for id in 1..=5 {
            spool.write(&request(id)).await.map_err(|e| io::Error::other(e.to_string()))?;
        }
        let ids = |batch: &[(SpooledRequest, u64)]| batch.iter().map(|(request, _)| request.packet[0]).collect::<Vec<_>>();
        assert_eq!(ids(&spool.next_batch(3).await?), [1, 2, 3]);
        // Reading moves nothing.
        let batch = spool.next_batch(3).await?;
        assert_eq!(ids(&batch), [1, 2, 3]);
        for (_, len) in batch.iter().take(2) {
            spool.advance(*len).await?;
        }
        assert_eq!(ids(&spool.next_batch(10).await?), [3, 4, 5], "on from where it got to, up to the end");
        let rest = spool.next_batch(10).await?;
        spool.advance_many(&rest.iter().map(|(_, len)| *len).collect::<Vec<_>>()).await?;
        assert!(spool.next_batch(10).await?.is_empty(), "several at once");
        Ok(())
    }

    #[test]
    fn test_next_batch_stays_in_one_file() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut files = Files::open(dir.path(), u64::MAX)?;
        files.append(&request(1).encode()).map_err(|e| io::Error::other(e.to_string()))?;
        files.append(&request(2).encode()).map_err(|e| io::Error::other(e.to_string()))?;
        files.start_segment()?;
        files.append(&request(3).encode()).map_err(|e| io::Error::other(e.to_string()))?;
        let mut corrupt = 0;
        let ids = |batch: Vec<(SpooledRequest, u64)>| batch.into_iter().map(|(request, len)| (request.packet[0], len)).collect::<Vec<_>>();
        let first = ids(files.next_batch(10, &mut corrupt)?);
        assert_eq!(first.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [1, 2]);
        for (_, len) in first {
            files.advance_many(len, 1)?;
        }
        assert_eq!(ids(files.next_batch(10, &mut corrupt)?).iter().map(|(id, _)| *id).collect::<Vec<_>>(), [3]);
        Ok(())
    }

    #[test]
    fn test_next_batch_leaves_a_damaged_record() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut files = Files::open(dir.path(), u64::MAX)?;
        for id in 1..=3 {
            files.append(&request(id).encode()).map_err(|e| io::Error::other(e.to_string()))?;
        }
        files.sync()?;
        // Break the second record's length, which makes it unreadable.
        let path = segment_path(dir.path(), files.segments.front().copied().unwrap_or_default());
        let mut bytes = fs::read(&path)?;
        let second = request(1).encode().len() + HEADER_LEN as usize;
        if let Some(byte) = bytes.get_mut(second) {
            *byte ^= 0xff;
        }
        fs::write(&path, bytes)?;
        let mut corrupt = 0;
        let batch = files.next_batch(10, &mut corrupt)?;
        assert_eq!((batch.len(), corrupt), (1, 0), "only the record before it, and nothing set aside yet");
        // Once the first is done with, `next` deals with the damage as it always has.
        files.advance_many(batch.iter().map(|(_, len)| *len).sum(), 1)?;
        assert!(files.next_batch(10, &mut corrupt)?.is_empty());
        assert_eq!(corrupt, 1);
        Ok(())
    }

    #[test]
    fn test_finished_segments_go_away() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut files = Files::open(dir.path(), u64::MAX)?;
        let record = request(1).encode();
        files.append(&record).map_err(|e| io::Error::other(e.to_string()))?;
        files.start_segment()?;
        files.append(&record).map_err(|e| io::Error::other(e.to_string()))?;
        let mut corrupt = 0;
        for _ in 0..2 {
            let (_, len) = files.next(&mut corrupt)?.ok_or_else(|| io::Error::other("nothing to replay"))?;
            files.advance_many(len, 1)?;
        }
        assert!(files.next(&mut corrupt)?.is_none());
        assert_eq!(files.segments.len(), 1, "the older file is finished");
        assert!(!segment_path(dir.path(), 1).exists());
        assert_eq!(files.pending, 0);
        Ok(())
    }
}
