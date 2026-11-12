//! One load-generating thread. It owns its sockets, in-flight tables and histograms and
//! shares nothing with other threads while running; `run` merges the results afterwards.
use crate::cli::LoadSpec;
use crate::eap::{failure_kind, report_once, EapClient, EapSetup, CODE_SUCCESS};
use crate::radsec::{TlsLink, TlsTarget};
use crate::tacacs::{self, Connect, Expect, TacacsLink, TacacsTarget, Verdict};
use crate::packet::{copy_for_accounting, eap_parts, verify_response, Answer, Rng, Signer, Step, Template, MAX_PACKET};
use hdrhistogram::Histogram;
use mio::net::UdpSocket;
use mio::{Events, Interest, Poll, Token};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Longest latency a histogram tells apart; a longer one is recorded as this.
const MAX_RECORDED_NS: u64 = 3_600_000_000_000;
/// Waiting for a timer is coarse (whole milliseconds on Linux), so a fixed-rate worker
/// stops waiting this long before a send and polls until it is due.
const SPIN_BEFORE_SEND: Duration = Duration::from_millis(1);
/// After a full send buffer or a failed send, before trying that again.
const SEND_BACKOFF: Duration = Duration::from_micros(100);
const NO_SLOT: u16 = u16::MAX;
/// After a RadSec connection could not be opened again, before the next try: each try can
/// block the worker for up to `--timeout`.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(100);

#[cfg(target_os = "linux")]
const ENOBUFS: i32 = 105;
#[cfg(not(target_os = "linux"))]
const ENOBUFS: i32 = 55;

/// What a socket of a load is: a UDP socket, a RadSec connection, or a TACACS+ connection.
pub enum Link {
    Udp(UdpSocket),
    Tls(Box<TlsLink>),
    Tacacs(Box<TacacsLink>),
}

/// Where a load's requests go.
#[derive(Debug, Clone, Copy)]
pub struct Targets {
    pub auth: SocketAddr,
    pub acct: SocketAddr,
}

/// What one load sends: its steps, each with a template per user.
pub struct LoadPlan {
    pub spec: LoadSpec,
    pub steps: Vec<StepPlan>,
    /// For an EAP load.
    pub eap: Option<EapPlan>,
    /// For a TACACS+ load.
    pub tacacs: Option<TacacsPlan>,
}

/// Who a TACACS+ load logs in as.
pub struct TacacsPlan {
    pub users: Vec<String>,
    pub password: String,
}

/// A TACACS+ request across its packets: one, or three for an ASCII login.
struct Login {
    session: u32,
    /// Packets answered so far: which one goes next.
    stage: u8,
    started: Option<Instant>,
    measured: bool,
    /// Packets sent.
    rounds: u64,
    /// When the whole request times out, from its first packet.
    deadline: Option<Instant>,
}

/// What an EAP load's conversations start from.
pub struct EapPlan {
    pub setup: Arc<EapSetup>,
    /// The names of the templates, in their order.
    pub users: Vec<String>,
    pub password: String,
}

/// An EAP login across its Access-Requests.
struct Conversation {
    client: EapClient,
    /// The EAP-Message of the next Access-Request, and the State to go with it.
    message: Vec<u8>,
    state: Option<Vec<u8>>,
    /// When the first Access-Request went, or was due: latency counts from here.
    started: Option<Instant>,
    measured: bool,
    /// Access-Requests sent, retransmissions apart.
    rounds: u64,
}

pub struct StepPlan {
    pub step: Step,
    pub templates: Vec<Template>,
}

/// Opens a socket for a worker: connected, except for a load that uses both ports. An unconnected
/// socket gets no ICMP errors, so a closed port shows as timeouts on those.
pub fn open_socket(bind: Option<IpAddr>, connect_to: Option<SocketAddr>, family_of: SocketAddr) -> io::Result<UdpSocket> {
    let socket = socket2::Socket::new(socket2::Domain::for_address(family_of), socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    // Enough for a full window of responses whatever the system's default. An error only
    // leaves that default in place, so it is not worth stopping for.
    let _ = socket.set_recv_buffer_size(1 << 20);
    let local = bind.unwrap_or(match family_of {
        SocketAddr::V4(_) => IpAddr::from([0u8; 4]),
        SocketAddr::V6(_) => IpAddr::from([0u8; 16]),
    });
    socket.bind(&SocketAddr::new(local, 0).into())?;
    if let Some(target) = connect_to {
        socket.connect(&target.into())?;
    }
    socket.set_nonblocking(true)?;
    Ok(UdpSocket::from_std(socket.into()))
}

fn new_histogram() -> Result<Histogram<u64>, String> {
    Histogram::new_with_bounds(1, MAX_RECORDED_NS, 3).map_err(|e| format!("histogram: {e}"))
}

/// Counts and latencies for one step of a load.
pub struct StepStats {
    pub sent: u64,
    pub accept: u64,
    pub reject: u64,
    pub challenge: u64,
    pub accounting: u64,
    pub timeouts: u64,
    pub invalid: u64,
    pub late: u64,
    pub socket_errors: u64,
    pub retransmits: u64,
    /// Access-Requests of the conversations that ended in Accept or Reject.
    pub rounds: u64,
    pub latency: Histogram<u64>,
}

impl StepStats {
    pub fn new() -> Result<StepStats, String> {
        Ok(StepStats {
            sent: 0,
            accept: 0,
            reject: 0,
            challenge: 0,
            accounting: 0,
            timeouts: 0,
            invalid: 0,
            late: 0,
            socket_errors: 0,
            retransmits: 0,
            rounds: 0,
            latency: new_histogram()?,
        })
    }

    pub fn responses(&self) -> u64 {
        self.accept + self.reject + self.challenge + self.accounting
    }

    pub fn merge(&mut self, other: &StepStats) -> Result<(), String> {
        self.sent += other.sent;
        self.accept += other.accept;
        self.reject += other.reject;
        self.challenge += other.challenge;
        self.accounting += other.accounting;
        self.timeouts += other.timeouts;
        self.invalid += other.invalid;
        self.late += other.late;
        self.socket_errors += other.socket_errors;
        self.retransmits += other.retransmits;
        self.rounds += other.rounds;
        self.latency.add(&other.latency).map_err(|e| format!("histogram: {e}"))
    }
}

pub struct WorkerOutput {
    pub steps: Vec<StepStats>,
    pub send_lag: Histogram<u64>,
    /// Share of the measured window not spent waiting for events.
    pub busy: f64,
    pub window: Duration,
    /// RadSec connections opened again after one broke.
    pub reconnects: u64,
}

/// What a worker is given. Sockets are opened up front so that a failure stops the run
/// before anything is sent.
pub struct WorkerInput {
    pub plan: Arc<LoadPlan>,
    pub targets: Targets,
    pub signer: Signer,
    /// The socket and its share of the load's requests in flight.
    pub sockets: Vec<(Link, u16)>,
    /// For RadSec: how to connect again.
    pub tls: Option<Arc<TlsTarget>>,
    /// For TACACS+: where connections go.
    pub tacacs: Option<Arc<TacacsTarget>>,
    pub connected: bool,
    pub rate: Option<f64>,
    pub budget: Option<u64>,
    pub warmup: Duration,
    pub duration: Option<Duration>,
    pub timeout: Duration,
    pub retries: u32,
    pub thread_index: u32,
    /// Keeps Acct-Session-Id apart between loads, threads and runs.
    pub session_prefix: u64,
    pub rng_seed: u64,
}

/// A request's journey through the steps of its load. Other loads have one step.
#[derive(Default)]
struct Flow {
    user: u32,
    session: u64,
    step: u8,
    alive_left: u32,
    /// Attributes from the Access-Accept, in wire form.
    copied: Vec<u8>,
    eap: Option<Box<Conversation>>,
    tacacs: Option<Box<Login>>,
}

impl Flow {
    /// A counted conversation or login that began and has not ended.
    fn counted_and_started(&self) -> bool {
        self.eap.as_ref().is_some_and(|conversation| conversation.measured && conversation.started.is_some()) || self.tacacs.as_ref().is_some_and(|login| login.measured && login.started.is_some())
    }
}

struct Slot {
    in_use: bool,
    flow: Flow,
    authenticator: [u8; 16],
    /// Latency counts from here: the first send, or the scheduled send under `rate`.
    intended: Instant,
    deadline: Instant,
    retries_left: u32,
    measured: bool,
    /// Kept only when retransmissions are possible.
    packet: Vec<u8>,
    previous: u16,
    next: u16,
}

struct SocketState {
    socket: Link,
    /// A RadSec connection that broke and could not be opened again yet.
    down: bool,
    /// When to try opening it again.
    retry_at: Option<Instant>,
    capacity: u16,
    slots: Vec<Slot>,
    next_id: u8,
    in_flight: u16,
    /// Requests in flight, oldest first, threaded through `slots`: every request here has
    /// the same timeout, so the oldest is always the first to expire.
    oldest: u16,
    newest: u16,
    /// Flows whose next step found the send buffer full.
    pending: Vec<Flow>,
}

impl SocketState {
    fn new(socket: Link, capacity: u16, now: Instant) -> SocketState {
        let slots = (0..256)
            .map(|_| Slot {
                in_use: false,
                flow: Flow::default(),
                authenticator: [0; 16],
                intended: now,
                deadline: now,
                retries_left: 0,
                measured: false,
                packet: Vec::new(),
                previous: NO_SLOT,
                next: NO_SLOT,
            })
            .collect();
        SocketState { socket, down: false, retry_at: None, capacity, slots, next_id: 0, in_flight: 0, oldest: NO_SLOT, newest: NO_SLOT, pending: Vec::new() }
    }

    /// The next free Identifier in turn. Going round rather than reusing the lowest makes a
    /// late answer less likely to meet a new request.
    fn take_id(&mut self) -> Option<u8> {
        for _ in 0..256 {
            let id = self.next_id;
            self.next_id = id.wrapping_add(1);
            if !self.slots[usize::from(id)].in_use {
                return Some(id);
            }
        }
        None
    }

    fn append(&mut self, id: u16) {
        let newest = self.newest;
        self.slots[usize::from(id)].previous = newest;
        self.slots[usize::from(id)].next = NO_SLOT;
        if newest == NO_SLOT {
            self.oldest = id;
        } else {
            self.slots[usize::from(newest)].next = id;
        }
        self.newest = id;
    }

    fn unlink(&mut self, id: u16) {
        let (previous, next) = (self.slots[usize::from(id)].previous, self.slots[usize::from(id)].next);
        if previous == NO_SLOT {
            self.oldest = next;
        } else {
            self.slots[usize::from(previous)].next = next;
        }
        if next == NO_SLOT {
            self.newest = previous;
        } else {
            self.slots[usize::from(next)].previous = previous;
        }
    }
}

fn send_buffer_full(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock || error.raw_os_error() == Some(ENOBUFS)
}

enum Sent {
    Done,
    /// The send buffer is full: try again.
    Blocked(Flow),
    /// The send failed, or there is nothing to send it with: the flow is over.
    Failed,
}

struct Worker {
    plan: Arc<LoadPlan>,
    targets: Targets,
    signer: Signer,
    rng: Rng,
    sockets: Vec<SocketState>,
    connected: bool,
    stats: Vec<StepStats>,
    send_lag: Histogram<u64>,
    buffer: Box<[u8; MAX_PACKET]>,
    timeout: Duration,
    retries: u32,
    start: Instant,
    measure_start: Instant,
    send_end: Option<Instant>,
    rate: Option<f64>,
    budget: Option<u64>,
    scheduled: u64,
    next_socket: usize,
    next_session: u64,
    session_prefix: u64,
    next_user: u64,
    in_flight: usize,
    backoff_until: Option<Instant>,
    registry: mio::Registry,
    tls: Option<Arc<TlsTarget>>,
    tacacs: Option<Arc<TacacsTarget>>,
    reconnects: u64,
}

/// Seconds of session time reported per Alive request.
const SESSION_TIME_STEP: u32 = 60;

/// A worker with everything that can fail to set up done, ready to go on its own thread.
pub struct PreparedWorker {
    worker: Worker,
    poll: Poll,
    warmup: Duration,
    duration: Option<Duration>,
}

fn register(registry: &mio::Registry, socket: &mut Link, index: usize) -> io::Result<()> {
    match socket {
        Link::Udp(socket) => registry.register(socket, Token(index), Interest::READABLE),
        Link::Tls(link) => registry.register(link.source(), Token(index), Interest::READABLE),
        Link::Tacacs(link) => match link.source() {
            Some(source) => registry.register(source, Token(index), Interest::READABLE),
            None => Ok(()),
        },
    }
}

/// Sets up a worker. This runs before any thread starts: a worker that failed on its thread
/// would never reach the barrier the others wait at.
pub fn prepare_worker(input: WorkerInput) -> Result<PreparedWorker, String> {
    let poll = Poll::new().map_err(|e| format!("poll: {e}"))?;
    let now = Instant::now();
    let mut sockets = Vec::with_capacity(input.sockets.len());
    for (index, (mut socket, capacity)) in input.sockets.into_iter().enumerate() {
        register(poll.registry(), &mut socket, index).map_err(|e| format!("poll: {e}"))?;
        sockets.push(SocketState::new(socket, capacity, now));
    }
    let registry = poll.registry().try_clone().map_err(|e| format!("poll: {e}"))?;
    let stats = input.plan.steps.iter().map(|_| StepStats::new()).collect::<Result<Vec<_>, _>>()?;
    let worker = Worker {
        plan: input.plan,
        targets: input.targets,
        signer: input.signer,
        rng: Rng::new(input.rng_seed),
        sockets,
        connected: input.connected,
        stats,
        send_lag: new_histogram()?,
        buffer: Box::new([0u8; MAX_PACKET]),
        timeout: input.timeout,
        retries: input.retries,
        start: now,
        measure_start: now,
        send_end: None,
        rate: input.rate,
        budget: input.budget,
        scheduled: 0,
        next_socket: 0,
        next_session: 0,
        session_prefix: input.session_prefix,
        next_user: u64::from(input.thread_index),
        in_flight: 0,
        backoff_until: None,
        registry,
        tls: input.tls,
        tacacs: input.tacacs,
        reconnects: 0,
    };
    Ok(PreparedWorker { worker, poll, warmup: input.warmup, duration: input.duration })
}

impl PreparedWorker {
    /// Runs the worker until its load is over and every counted request has finished.
    pub fn run(self, barrier: Arc<Barrier>) -> Result<WorkerOutput, String> {
        let PreparedWorker { mut worker, mut poll, warmup, duration } = self;
        barrier.wait();
        worker.start = Instant::now();
        worker.measure_start = worker.start + warmup;
        worker.send_end = duration.map(|duration| worker.measure_start + duration);
        worker.run(&mut poll)
    }
}

impl Worker {
    fn run(mut self, poll: &mut Poll) -> Result<WorkerOutput, String> {
        let mut events = Events::with_capacity(self.sockets.len().max(16));
        let tick = (self.timeout / 10).clamp(Duration::from_millis(1), Duration::from_millis(10));
        let mut next_tick = self.start + tick;
        let mut idle = Duration::ZERO;
        let mut scan = true;
        let finished = loop {
            let loop_start = Instant::now();
            let sending = self.send_end.is_none_or(|end| loop_start < end);
            if !sending {
                self.drop_pending();
            }
            if self.backoff_until.is_some_and(|until| loop_start >= until) {
                self.backoff_until = None;
                scan = true;
            }
            if scan && sending && self.backoff_until.is_none() {
                scan = false;
                for index in 0..self.sockets.len() {
                    self.fill(index);
                }
            }
            if self.rate.is_some() && sending && self.backoff_until.is_none() {
                self.send_scheduled();
            }
            if loop_start >= next_tick {
                next_tick = loop_start + tick;
                self.check_timeouts(loop_start);
                scan = true;
            }
            if self.in_flight == 0 && self.no_more_to_send(loop_start) {
                break Instant::now();
            }

            let mut wake = next_tick;
            if let Some(end) = self.send_end.filter(|end| *end > loop_start) {
                wake = wake.min(end);
            }
            if let Some(until) = self.backoff_until {
                wake = wake.min(until);
            }
            let mut spinning = false;
            if let Some(due) = self.next_due().filter(|_| self.backoff_until.is_none()) {
                if due <= loop_start + SPIN_BEFORE_SEND {
                    spinning = true;
                } else {
                    wake = wake.min(due - SPIN_BEFORE_SEND);
                }
            }
            let wait = if spinning { Duration::ZERO } else { wake.saturating_duration_since(loop_start) };
            let before_poll = Instant::now();
            poll.poll(&mut events, Some(wait)).map_err(|e| format!("poll: {e}"))?;
            let waited = if spinning { before_poll.elapsed() + (before_poll - loop_start) } else { before_poll.elapsed() };
            if self.in_measured_window(loop_start) {
                idle += waited;
            }
            for event in events.iter() {
                if event.is_writable() {
                    self.flush(event.token().0);
                }
                if event.is_readable() || event.is_read_closed() || event.is_error() {
                    self.drain(event.token().0);
                }
            }
        };
        let window = match self.send_end {
            Some(end) => end - self.measure_start,
            None => finished - self.measure_start,
        };
        let busy = if window.is_zero() { 0.0 } else { (1.0 - idle.as_secs_f64() / window.as_secs_f64()).clamp(0.0, 1.0) };
        Ok(WorkerOutput { steps: self.stats, send_lag: self.send_lag, busy, window, reconnects: self.reconnects })
    }

    fn in_measured_window(&self, time: Instant) -> bool {
        time >= self.measure_start && self.send_end.is_none_or(|end| time < end)
    }

    fn no_more_to_send(&self, now: Instant) -> bool {
        if self.send_end.is_some_and(|end| now >= end) {
            return true;
        }
        self.budget == Some(0) && self.sockets.iter().all(|socket| socket.pending.is_empty())
    }

    fn drop_pending(&mut self) {
        for socket in &mut self.sockets {
            // A conversation that was counted ends without its result.
            let unfinished = socket.pending.iter().filter(|flow| flow.counted_and_started()).count();
            self.stats[0].timeouts += unfinished as u64;
            socket.pending.clear();
        }
    }

    fn scheduled_time(&self, index: u64) -> Option<Instant> {
        let rate = self.rate?;
        Some(self.start + Duration::from_secs_f64(index as f64 / rate))
    }

    /// When the next scheduled request can go, if one could go if it were time.
    fn next_due(&self) -> Option<Instant> {
        if self.budget == Some(0) {
            return None;
        }
        let due = self.scheduled_time(self.scheduled)?;
        if self.send_end.is_some_and(|end| due >= end) {
            return None;
        }
        self.sockets.iter().any(|socket| socket.in_flight < socket.capacity).then_some(due)
    }

    fn new_flow(&mut self) -> Flow {
        let users = match &self.plan.tacacs {
            Some(plan) => plan.users.len(),
            None => self.plan.steps[0].templates.len(),
        };
        let users = (users as u64).max(1);
        let session = self.session_prefix | (self.next_session & 0xffff_ffff);
        let flow = Flow {
            user: (self.next_user % users) as u32,
            session,
            step: 0,
            alive_left: self.plan.spec.alive,
            copied: Vec::new(),
            eap: None,
            // One request at a time on a connection, so any session id differs from the last.
            tacacs: self.plan.tacacs.as_ref().map(|_| Box::new(Login { session: (session ^ (session >> 32)) as u32, stage: 0, started: None, measured: false, rounds: 0, deadline: None })),
        };
        let mut flow = flow;
        if let Some(eap) = &self.plan.eap {
            let client = EapClient::new(eap.setup.clone(), &eap.users[flow.user as usize], &eap.password);
            let message = client.start();
            flow.eap = Some(Box::new(Conversation { client, message, state: None, started: None, measured: false, rounds: 0 }));
        }
        self.next_user += 1;
        self.next_session += 1;
        flow
    }

    /// Sends what is waiting on this socket, then, in a fixed-concurrency load, new requests
    /// up to its share.
    fn fill(&mut self, index: usize) {
        if self.sockets[index].down {
            return;
        }
        while self.sockets[index].in_flight < self.sockets[index].capacity {
            let Some(flow) = self.sockets[index].pending.pop() else { break };
            if !self.send_or_wait(index, flow, None) {
                return;
            }
        }
        if self.rate.is_some() {
            return;
        }
        while self.sockets[index].in_flight < self.sockets[index].capacity && self.budget != Some(0) {
            if self.send_end.is_some_and(|end| Instant::now() >= end) {
                return;
            }
            let flow = self.new_flow();
            match self.send(index, flow, None) {
                Sent::Done => self.spend_budget(),
                Sent::Blocked(_) => {
                    // A later attempt starts a new flow, with a new session.
                    self.back_off();
                    return;
                }
                Sent::Failed => {
                    self.spend_budget();
                    self.back_off();
                    return;
                }
            }
        }
    }

    /// Sends every scheduled request that is due and has room.
    fn send_scheduled(&mut self) {
        loop {
            if self.budget == Some(0) {
                return;
            }
            let Some(due) = self.scheduled_time(self.scheduled) else { return };
            if due > Instant::now() || self.send_end.is_some_and(|end| due >= end) {
                return;
            }
            let count = self.sockets.len();
            let Some(index) = (0..count)
                .map(|offset| (self.next_socket + offset) % count)
                .find(|index| !self.sockets[*index].down && self.sockets[*index].in_flight < self.sockets[*index].capacity && self.sockets[*index].pending.is_empty())
            else {
                // It waits, keeping its time, so what the wait costs shows in its latency.
                return;
            };
            self.next_socket = (index + 1) % count;
            let flow = self.new_flow();
            match self.send(index, flow, Some(due)) {
                Sent::Done => {
                    self.scheduled += 1;
                    self.spend_budget();
                }
                Sent::Blocked(_) => {
                    self.back_off();
                    return;
                }
                Sent::Failed => {
                    self.scheduled += 1;
                    self.spend_budget();
                    self.back_off();
                    return;
                }
            }
        }
    }

    fn spend_budget(&mut self) {
        if let Some(budget) = &mut self.budget {
            *budget -= 1;
        }
    }

    fn back_off(&mut self) {
        self.backoff_until = Some(Instant::now() + SEND_BACKOFF);
    }

    /// Sends a flow's next step, keeping it for later if the send buffer is full. False
    /// means the buffer was full.
    fn send_or_wait(&mut self, index: usize, flow: Flow, scheduled: Option<Instant>) -> bool {
        match self.send(index, flow, scheduled) {
            Sent::Done => true,
            Sent::Blocked(flow) => {
                self.sockets[index].pending.push(flow);
                self.back_off();
                false
            }
            Sent::Failed => {
                self.back_off();
                true
            }
        }
    }

    fn send(&mut self, index: usize, mut flow: Flow, scheduled: Option<Instant>) -> Sent {
        if flow.tacacs.is_some() {
            return self.send_tacacs(index, flow, scheduled);
        }
        let step = usize::from(flow.step);
        let Some(id) = self.sockets[index].take_id() else {
            return Sent::Blocked(flow);
        };
        let plan = &self.plan.steps[step];
        let template = &plan.templates[flow.user as usize];
        let (length, authenticator) = match &flow.eap {
            Some(conversation) => match template.render_eap(&mut self.buffer, id, &conversation.message, conversation.state.as_deref(), &mut self.rng, &self.signer) {
                Some(rendered) => rendered,
                None => {
                    report_once("request too long", "an EAP-Message does not fit in an Access-Request: lower --eap-fragment");
                    if conversation.measured {
                        self.stats[step].invalid += 1;
                    }
                    return Sent::Failed;
                }
            },
            None => {
                let intervals = self.plan.spec.alive.wrapping_sub(flow.alive_left).wrapping_add(1);
                template.render(&mut self.buffer, id, flow.session, intervals.wrapping_mul(SESSION_TIME_STEP), &flow.copied, &mut self.rng, &self.signer)
            }
        };
        let target = if plan.step.uses_acct_port() { self.targets.acct } else { self.targets.auth };
        let socket = &mut self.sockets[index];
        let (result, broken) = match &mut socket.socket {
            Link::Udp(udp) => (if self.connected { udp.send(&self.buffer[..length]) } else { udp.send_to(&self.buffer[..length], target) }, false),
            Link::Tls(link) => match link.send(&self.buffer[..length]) {
                Ok(true) => (Ok(length), false),
                // The rest goes when the connection can take it.
                Ok(false) => (self.registry.reregister(link.source(), Token(index), Interest::READABLE | Interest::WRITABLE).map(|()| length), false),
                Err(error) => (Err(error), true),
            },
            // `send` hands TACACS+ flows to `send_tacacs`.
            Link::Tacacs(_) => return Sent::Failed,
        };
        let now = Instant::now();
        // A conversation counts once, from its first Access-Request.
        let (intended, measured, first) = match flow.eap.as_deref() {
            Some(Conversation { started: Some(started), measured, .. }) => (*started, *measured, false),
            _ => {
                let intended = scheduled.unwrap_or(now);
                (intended, self.in_measured_window(intended), true)
            }
        };
        match result {
            Ok(_) => {}
            Err(error) if !broken && send_buffer_full(&error) => return Sent::Blocked(flow),
            Err(error) => {
                if measured {
                    self.stats[step].socket_errors += 1;
                }
                if broken {
                    self.disconnect(index, &format!("writing: {error}"));
                }
                return Sent::Failed;
            }
        }
        if measured && first {
            self.stats[step].sent += 1;
            if scheduled.is_some() {
                self.send_lag.saturating_record((now - intended).as_nanos() as u64);
            }
        }
        if let Some(conversation) = flow.eap.as_deref_mut() {
            conversation.started = Some(intended);
            conversation.measured = measured;
            conversation.rounds += 1;
        }
        let retries = self.retries;
        let socket = &mut self.sockets[index];
        let slot = &mut socket.slots[usize::from(id)];
        slot.in_use = true;
        slot.flow = flow;
        slot.authenticator = authenticator;
        slot.intended = intended;
        slot.deadline = now + self.timeout;
        slot.retries_left = retries;
        slot.measured = measured;
        if retries > 0 {
            slot.packet.clear();
            slot.packet.extend_from_slice(&self.buffer[..length]);
        }
        socket.in_flight += 1;
        socket.append(u16::from(id));
        self.in_flight += 1;
        Sent::Done
    }

    fn release(&mut self, index: usize, id: u16) -> Flow {
        let socket = &mut self.sockets[index];
        socket.unlink(id);
        socket.in_flight -= 1;
        self.in_flight -= 1;
        let slot = &mut socket.slots[usize::from(id)];
        slot.in_use = false;
        std::mem::take(&mut slot.flow)
    }

    /// Sends the next packet of a TACACS+ request, opening its connection first under
    /// `per-request`.
    fn send_tacacs(&mut self, index: usize, mut flow: Flow, scheduled: Option<Instant>) -> Sent {
        let step = self.plan.steps[0].step;
        let (Some(plan), Some(target)) = (&self.plan.tacacs, self.tacacs.clone()) else { return Sent::Failed };
        let Some(login) = flow.tacacs.as_deref() else { return Sent::Failed };
        let user = plan.users.get(flow.user as usize).map_or("", String::as_str);
        let now = Instant::now();
        // Every packet of an ASCII login shares the first one's deadline.
        let deadline = login.deadline.unwrap_or(now + self.timeout);
        let (intended, measured, first) = match login.started {
            Some(started) => (started, login.measured, false),
            None => {
                let intended = scheduled.unwrap_or(now);
                (intended, self.in_measured_window(intended), true)
            }
        };
        let packet = match login.stage {
            0 => {
                let start_time = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs());
                tacacs::start(step, login.session, &tacacs::Request { user, password: &plan.password, task: flow.session, start_time }, &target.secret)
            }
            1 => tacacs::continue_with(login.session, 3, user.as_bytes(), &target.secret),
            _ => tacacs::continue_with(login.session, 5, plan.password.as_bytes(), &target.secret),
        };
        let packet = match packet {
            Ok(packet) => packet,
            Err(reason) => {
                report_once(&format!("{}: request", step.name()), &format!("{}: {reason}", step.name()));
                if measured {
                    self.stats[0].invalid += 1;
                }
                return Sent::Failed;
            }
        };
        let Some(id) = self.sockets[index].take_id() else { return Sent::Blocked(flow) };
        let Link::Tacacs(link) = &mut self.sockets[index].socket else { return Sent::Failed };
        let opened = !link.is_open();
        let result = (|| {
            if opened {
                link.open(&target).map_err(|e| format!("connecting: {e}"))?;
            }
            let done = link.send(&packet).map_err(|e| format!("writing: {e}"))?;
            let interest = if done { Interest::READABLE } else { Interest::READABLE | Interest::WRITABLE };
            let Some(source) = link.source() else { return Err("connecting: no connection".to_string()) };
            if opened {
                self.registry.register(source, Token(index), interest).map_err(|e| format!("poll: {e}"))
            } else if !done {
                self.registry.reregister(source, Token(index), interest).map_err(|e| format!("poll: {e}"))
            } else {
                Ok(())
            }
        })();
        if let Err(reason) = result {
            if measured {
                self.stats[0].socket_errors += 1;
            }
            self.disconnect(index, &reason);
            return Sent::Failed;
        }
        if measured && first {
            self.stats[0].sent += 1;
            if scheduled.is_some() {
                self.send_lag.saturating_record((now - intended).as_nanos() as u64);
            }
        }
        if let Some(login) = flow.tacacs.as_deref_mut() {
            login.started = Some(intended);
            login.measured = measured;
            login.rounds += 1;
            login.deadline = Some(deadline);
        }
        let socket = &mut self.sockets[index];
        let slot = &mut socket.slots[usize::from(id)];
        slot.in_use = true;
        slot.flow = flow;
        slot.intended = intended;
        slot.deadline = deadline;
        slot.retries_left = 0;
        slot.measured = measured;
        socket.in_flight += 1;
        socket.append(u16::from(id));
        self.in_flight += 1;
        Sent::Done
    }

    /// What a TACACS+ reply means for the request waiting on its connection.
    fn on_tacacs_reply(&mut self, index: usize, reply: &[u8], now: Instant) {
        let oldest = self.sockets[index].oldest;
        if oldest == NO_SLOT {
            if self.in_measured_window(now) {
                self.stats[0].late += 1;
            }
            return;
        }
        let Some(target) = self.tacacs.clone() else { return };
        let step = self.plan.steps[0].step;
        let mut flow = self.release(index, oldest);
        let Some(login) = flow.tacacs.as_deref_mut() else { return };
        let expect = Expect { step, session: login.session, seq: 1 + 2 * login.stage, stage: login.stage };
        let (measured, started) = (login.measured, login.started.unwrap_or(now));
        let verdict = match tacacs::judge(reply, expect, &target.secret) {
            Ok(verdict) => verdict,
            Err(reason) => {
                report_once(&format!("{}: {}", step.name(), failure_kind(&reason)), &format!("{}: {reason}", step.name()));
                if measured {
                    self.stats[0].invalid += 1;
                }
                self.request_over(index);
                return;
            }
        };
        if matches!(verdict, Verdict::GetUser | Verdict::GetPass) {
            login.stage += 1;
            // A counted login goes on after sending stops, to its result.
            if measured || self.send_end.is_none_or(|end| now < end) {
                self.send_or_wait(index, flow, None);
            } else {
                self.request_over(index);
            }
            return;
        }
        if measured {
            let stats = &mut self.stats[0];
            match verdict {
                Verdict::Accept => stats.accept += 1,
                Verdict::Accounting => stats.accounting += 1,
                _ => stats.reject += 1,
            }
            stats.rounds += login.rounds;
            stats.latency.saturating_record((now - started).as_nanos() as u64);
        }
        self.request_over(index);
    }

    /// A TACACS+ request ended: under `per-request` its connection goes with it.
    fn request_over(&mut self, index: usize) {
        if self.tacacs.as_ref().is_some_and(|target| target.connect == Connect::PerRequest) {
            self.close_tacacs(index);
        }
    }

    fn close_tacacs(&mut self, index: usize) {
        if let Link::Tacacs(link) = &mut self.sockets[index].socket {
            if let Some(source) = link.source() {
                let _ = self.registry.deregister(source);
            }
            link.close();
        }
    }

    /// Reads everything waiting on a socket, then refills it.
    fn drain(&mut self, index: usize) {
        if let Link::Tacacs(link) = &mut self.sockets[index].socket {
            let received = link.receive();
            let now = Instant::now();
            for packet in &received.packets {
                self.on_tacacs_reply(index, packet, now);
            }
            if let Some(reason) = received.closed {
                self.disconnect(index, &reason);
            }
            if self.send_end.is_none_or(|end| Instant::now() < end) && self.backoff_until.is_none() {
                self.fill(index);
            }
            return;
        }
        if let Link::Tls(link) = &mut self.sockets[index].socket {
            let received = link.receive();
            let now = Instant::now();
            for packet in &received.packets {
                self.on_reply(index, packet, now);
            }
            if let Some(reason) = received.closed {
                self.disconnect(index, &reason);
            }
            if self.send_end.is_none_or(|end| Instant::now() < end) && self.backoff_until.is_none() {
                self.fill(index);
            }
            return;
        }
        let mut reply = [0u8; MAX_PACKET];
        loop {
            let Link::Udp(udp) = &self.sockets[index].socket else { return };
            let received = if self.connected {
                udp.recv(&mut reply)
            } else {
                match udp.recv_from(&mut reply) {
                    // Nothing else should be writing to this port, but it is not connected.
                    Ok((_, from)) if from.ip() != self.targets.auth.ip() => continue,
                    Ok((length, _)) => Ok(length),
                    Err(error) => Err(error),
                }
            };
            let now = Instant::now();
            match received {
                Ok(length) => self.on_reply(index, &reply[..length], now),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    // An ICMP error says nothing of which request it was for.
                    let socket = &self.sockets[index];
                    let step = match socket.oldest {
                        NO_SLOT => 0,
                        oldest => usize::from(socket.slots[usize::from(oldest)].flow.step),
                    };
                    if self.in_measured_window(now) {
                        self.stats[step].socket_errors += 1;
                    }
                    break;
                }
            }
        }
        if self.send_end.is_none_or(|end| Instant::now() < end) && self.backoff_until.is_none() {
            self.fill(index);
        }
    }

    fn on_reply(&mut self, index: usize, reply: &[u8], now: Instant) {
        let Some(&id) = reply.get(1) else {
            if self.in_measured_window(now) {
                self.stats[0].invalid += 1;
            }
            return;
        };
        let slot = &self.sockets[index].slots[usize::from(id)];
        if !slot.in_use {
            if self.in_measured_window(now) {
                self.stats[0].late += 1;
            }
            return;
        }
        let step_index = usize::from(slot.flow.step);
        let step = self.plan.steps[step_index].step;
        let measured = slot.measured;
        let intended = slot.intended;
        let request_authenticator = slot.authenticator;
        let answer = match verify_response(reply, step, &slot.authenticator, &self.signer) {
            Ok(answer) => answer,
            Err(_) => {
                // The request stays in flight: the real answer can still come.
                if measured {
                    self.stats[step_index].invalid += 1;
                }
                return;
            }
        };
        if step.is_eap() {
            let flow = self.release(index, u16::from(id));
            self.on_eap_answer(index, flow, answer, reply, &request_authenticator, now);
            return;
        }
        if measured {
            let stats = &mut self.stats[step_index];
            match answer {
                Answer::Accept => stats.accept += 1,
                Answer::Reject => stats.reject += 1,
                Answer::Challenge => stats.challenge += 1,
                Answer::Accounting => stats.accounting += 1,
            }
            stats.latency.saturating_record((now - intended).as_nanos() as u64);
        }
        let mut flow = self.release(index, u16::from(id));
        // The last step of a one-step load, or a flow that was not accepted, ends here.
        let continues = step_index + 1 < self.plan.steps.len() && matches!(answer, Answer::Accept | Answer::Accounting);
        if !continues {
            return;
        }
        if answer == Answer::Accept {
            copy_for_accounting(reply, &mut flow.copied);
        }
        self.advance(&mut flow);
        if self.send_end.is_none_or(|end| now < end) {
            self.send_or_wait(index, flow, None);
        }
    }

    /// Writes what a connection still holds, and stops waiting to write once it is out.
    fn flush(&mut self, index: usize) {
        if let Link::Tacacs(link) = &mut self.sockets[index].socket {
            if !link.is_open() {
                return;
            }
            match link.flush() {
                Ok(true) => {
                    let watched = match link.source() {
                        Some(source) => self.registry.reregister(source, Token(index), Interest::READABLE),
                        None => Ok(()),
                    };
                    if let Err(error) = watched {
                        self.disconnect(index, &format!("poll: {error}"));
                    }
                }
                Ok(false) => {}
                Err(error) => self.disconnect(index, &format!("writing: {error}")),
            }
            return;
        }
        let Link::Tls(link) = &mut self.sockets[index].socket else { return };
        match link.flush() {
            Ok(true) => {
                if let Err(error) = self.registry.reregister(link.source(), Token(index), Interest::READABLE) {
                    self.disconnect(index, &format!("poll: {error}"));
                }
            }
            Ok(false) => {}
            Err(error) => self.disconnect(index, &format!("writing: {error}")),
        }
    }

    /// A connection broke: what waited on it counts as socket errors, and it is opened
    /// again, except a TACACS+ one under `per-request`, which the next request opens.
    fn disconnect(&mut self, index: usize, reason: &str) {
        let tacacs = matches!(self.sockets[index].socket, Link::Tacacs(_));
        let (label, name) = if tacacs { ("tacacs", "TACACS+") } else { ("radsec", "RadSec") };
        report_once(&format!("{label}: {}", failure_kind(reason)), &format!("a {name} connection ended: {reason}"));
        while self.sockets[index].oldest != NO_SLOT {
            let oldest = self.sockets[index].oldest;
            let slot = &self.sockets[index].slots[usize::from(oldest)];
            if slot.measured {
                self.stats[usize::from(slot.flow.step)].socket_errors += 1;
            }
            self.release(index, oldest);
        }
        let unsent = std::mem::take(&mut self.sockets[index].pending);
        for flow in unsent {
            if flow.eap.as_ref().is_some_and(|conversation| conversation.measured) || flow.tacacs.as_ref().is_some_and(|login| login.measured) {
                self.stats[usize::from(flow.step)].socket_errors += 1;
            }
        }
        self.drop_connection(index);
    }

    /// Leaves a connection: a RadSec or `keep` TACACS+ one is opened again.
    fn drop_connection(&mut self, index: usize) {
        match &mut self.sockets[index].socket {
            Link::Tls(link) => {
                let _ = self.registry.deregister(link.source());
            }
            Link::Tacacs(link) => {
                let answered = link.answered();
                self.close_tacacs(index);
                if self.tacacs.as_ref().is_some_and(|target| target.connect == Connect::PerRequest) {
                    return;
                }
                // A server at its connection limit takes the connection and closes it at once:
                // opening it again at once would only repeat that as fast as it can.
                if !answered {
                    self.sockets[index].down = true;
                    self.sockets[index].retry_at = Some(Instant::now() + RECONNECT_BACKOFF);
                    return;
                }
            }
            Link::Udp(_) => return,
        }
        self.sockets[index].down = true;
        self.connect_again(index);
    }

    /// Opens a broken connection again; a failure leaves it down for the next tick.
    fn connect_again(&mut self, index: usize) {
        if let Some(target) = self.tacacs.clone() {
            match TacacsLink::connect(&target) {
                Ok(link) => {
                    let mut socket = Link::Tacacs(Box::new(link));
                    if let Err(error) = register(&self.registry, &mut socket, index) {
                        report_once("tacacs: poll", &format!("a TACACS+ connection could not be watched: {error}"));
                        return;
                    }
                    self.sockets[index].socket = socket;
                    self.sockets[index].down = false;
                    self.sockets[index].retry_at = None;
                    self.reconnects += 1;
                }
                Err(error) => {
                    self.sockets[index].retry_at = Some(Instant::now() + RECONNECT_BACKOFF);
                    report_once(&format!("tacacs: {}", failure_kind(&error)), &format!("a TACACS+ connection could not be opened again: {error}"));
                }
            }
            return;
        }
        let Some(target) = self.tls.clone() else { return };
        match TlsLink::connect(&target) {
            Ok(link) => {
                let mut socket = Link::Tls(Box::new(link));
                if let Err(error) = register(&self.registry, &mut socket, index) {
                    report_once("radsec: poll", &format!("a RadSec connection could not be watched: {error}"));
                    return;
                }
                self.sockets[index].socket = socket;
                self.sockets[index].down = false;
                self.sockets[index].retry_at = None;
                self.reconnects += 1;
            }
            Err(error) => {
                self.sockets[index].retry_at = Some(Instant::now() + RECONNECT_BACKOFF);
                report_once(&format!("radsec: {}", failure_kind(&error)), &format!("a RadSec connection could not be opened again: {error}"));
            }
        }
    }

    /// What a verified answer means for an EAP conversation: the next round, or its end.
    fn on_eap_answer(&mut self, index: usize, mut flow: Flow, answer: Answer, reply: &[u8], request_authenticator: &[u8; 16], now: Instant) {
        let Some(conversation) = flow.eap.as_deref_mut() else { return };
        let (measured, started) = (conversation.measured, conversation.started.unwrap_or(now));
        let (eap, state) = eap_parts(reply);
        let failure = match answer {
            Answer::Challenge => match conversation.client.next(&eap) {
                Ok(message) => {
                    conversation.message = message;
                    conversation.state = state;
                    // A counted conversation goes on after sending stops, to its result.
                    if measured || self.send_end.is_none_or(|end| now < end) {
                        self.send_or_wait(index, flow, None);
                    }
                    return;
                }
                Err(reason) => Some(reason),
            },
            Answer::Accept if eap.first() != Some(&CODE_SUCCESS) => Some("an Access-Accept without EAP-Success".to_string()),
            Answer::Accept => conversation.client.check_keys(reply, request_authenticator, self.signer.secret()).err(),
            _ => None,
        };
        if !measured {
            return;
        }
        let stats = &mut self.stats[0];
        if let Some(reason) = failure {
            let step = self.plan.steps[0].step.name();
            report_once(&format!("{step}: {}", failure_kind(&reason)), &format!("{step}: {reason}"));
            stats.invalid += 1;
            return;
        }
        match answer {
            Answer::Accept => stats.accept += 1,
            _ => stats.reject += 1,
        }
        stats.rounds += conversation.rounds;
        stats.latency.saturating_record((now - started).as_nanos() as u64);
    }

    /// Steps of a session are Access-Request, Start, Alive if any, Stop; Alive repeats.
    fn advance(&self, flow: &mut Flow) {
        let is_alive = self.plan.steps[usize::from(flow.step)].step == Step::AcctAlive;
        if is_alive && flow.alive_left > 0 {
            flow.alive_left -= 1;
            if flow.alive_left > 0 {
                return;
            }
        }
        flow.step += 1;
    }

    fn check_timeouts(&mut self, now: Instant) {
        for index in 0..self.sockets.len() {
            if self.sockets[index].down && self.sockets[index].retry_at.is_none_or(|at| now >= at) {
                self.connect_again(index);
            }
        }
        for index in 0..self.sockets.len() {
            loop {
                let socket = &self.sockets[index];
                let oldest = socket.oldest;
                if oldest == NO_SLOT || socket.slots[usize::from(oldest)].deadline > now {
                    break;
                }
                if socket.slots[usize::from(oldest)].retries_left > 0 {
                    if !self.retransmit(index, oldest, now) {
                        break;
                    }
                } else {
                    let (step, measured) = {
                        let slot = &socket.slots[usize::from(oldest)];
                        (usize::from(slot.flow.step), slot.measured)
                    };
                    if measured {
                        self.stats[step].timeouts += 1;
                    }
                    self.release(index, oldest);
                    // A late TACACS+ reply would be taken for the next request's.
                    if matches!(self.sockets[index].socket, Link::Tacacs(_)) {
                        self.drop_connection(index);
                        break;
                    }
                }
            }
        }
    }

    /// Sends the same bytes again. False means the send buffer was full.
    fn retransmit(&mut self, index: usize, id: u16, now: Instant) -> bool {
        let socket = &self.sockets[index];
        let slot = &socket.slots[usize::from(id)];
        let step = usize::from(slot.flow.step);
        let target = if self.plan.steps[step].step.uses_acct_port() { self.targets.acct } else { self.targets.auth };
        let result = match &socket.socket {
            Link::Udp(udp) => if self.connected { udp.send(&slot.packet) } else { udp.send_to(&slot.packet, target) },
            // Nothing is sent again over TCP: `run` refuses retries with TLS and gives
            // TACACS+ loads none.
            Link::Tls(_) | Link::Tacacs(_) => Ok(0),
        };
        let measured = slot.measured;
        match result {
            Ok(_) => {
                if measured {
                    self.stats[step].retransmits += 1;
                }
            }
            Err(error) if send_buffer_full(&error) => return false,
            Err(_) => {
                if measured {
                    self.stats[step].socket_errors += 1;
                }
            }
        }
        let socket = &mut self.sockets[index];
        socket.unlink(id);
        socket.append(id);
        let slot = &mut socket.slots[usize::from(id)];
        slot.retries_left -= 1;
        slot.deadline = now + self.timeout;
        true
    }
}
