//! The tensor scheduler: accelerator time as a scheduled resource.
//!
//! This is the first of the architecture's five bets, and the one nothing else
//! has. Today an NPU is reached through a vendor driver that serves requests in
//! the order they arrive: no priorities, no deadlines, no preemption, and no
//! way for the system to know what a job cost. The result is the behaviour
//! everyone has felt — the keyboard stutters while something in the background
//! summarises your mail.
//!
//! Here a unit of inference work is a *job*, and a job carries what the
//! scheduler needs to make a decision:
//!
//! - a **class**, saying how much latency it can tolerate;
//! - a **deadline**, where it has one, so the scheduler can refuse the work
//!   rather than accept it and miss;
//! - a **segment count**, because that is the granularity at which an
//!   accelerator can actually be taken away.
//!
//! **Preemption at segment boundaries.** NPUs do not preempt mid-layer; nothing
//! stops a matrix multiply halfway. So a graph is split into segments with
//! bounded runtime, and a higher-priority job lands *between* them. This is the
//! same compromise GPU compositors make, and it is what turns an unbounded
//! stall into a bounded one.
//!
//! **What is modelled here and what is real.** The scheduling, the admission
//! control, the preemption and the accounting are real. The accelerator is not:
//! there is no NPU in QEMU, so a segment is executed by a CPU loop and its cost
//! is measured rather than assumed. Energy is derived from measured time and a
//! fixed power figure — a model, and labelled as one. None of that changes the
//! part being tested, which is what the scheduler decides and when.

use crate::sync::SpinLock;
use crate::{sched, time};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

/// How urgent a job is, and therefore what it may take the device away from.
///
/// Ordered deliberately: a lower number outranks a higher one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Qos {
    /// The user is waiting on this right now — voice, keyboard, camera assist.
    Interactive = 0,
    /// The user asked for it and is watching.
    Foreground = 1,
    /// Indexing, summarising. Nobody is waiting.
    Background = 2,
    /// Runs only when nothing else wants the device.
    Opportunistic = 3,
}

impl Qos {
    pub fn from_u64(v: u64) -> Qos {
        match v {
            0 => Qos::Interactive,
            1 => Qos::Foreground,
            2 => Qos::Background,
            _ => Qos::Opportunistic,
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Qos::Interactive => "interactive",
            Qos::Foreground => "foreground",
            Qos::Background => "background",
            Qos::Opportunistic => "opportunistic",
        }
    }
}

/// Assumed power draw of the accelerator while a segment runs, in milliwatts.
/// A phone-class NPU figure. Energy below is this times measured time — a
/// model, not a measurement, and the interface is the point rather than the
/// number.
const DEVICE_POWER_MW: u64 = 2500;

/// Starting guess for segment cost, replaced by measurement after the first.
const INITIAL_SEGMENT_US: u64 = 1000;

// ---- thermal model -------------------------------------------------------
//
// A phone has no fan. Sustained accelerator work heats the package until
// something has to give, and today what gives is the whole device: the governor
// notices too late and throttles everything, including the thing the user is
// waiting for. Here temperature is an *input to admission* instead, so the work
// that gives way is chosen rather than whatever happened to be running.
//
// The model is deliberately crude — a single lumped temperature that rises with
// work and decays towards ambient. Real thermal behaviour needs the part's own
// characterisation. What is being tested is what the scheduler does with the
// number, not the number.

/// How much one segment heats the device, in thousandths of a degree.
///
/// Chosen so that a few hundred segments span the whole range from ambient to
/// critical: a model where any realistic workload saturates instantly cannot
/// demonstrate the band between the two limits, which is the part that matters.
const RISE_PER_SEGMENT_MILLI_C: u64 = 200;
/// How fast it cools when the device is idle, per millisecond.
const DECAY_PER_MS_MILLI_C: u64 = 120;
/// Above this, work nobody is waiting for is refused. Deliberately early: the
/// point of having classes is that something yields long before the device is
/// in trouble.
const THROTTLE_MILLI_C: u64 = 15_000;
/// Above this, nothing new is admitted at all — not even work the user is
/// waiting on. A wide gap from the throttle point on purpose: between the two,
/// the device is still doing everything that matters to whoever is holding it.
const CRITICAL_MILLI_C: u64 = 45_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JobState {
    Queued,
    Running,
    Done,
    /// Refused at submission: the deadline could not be met, the energy budget
    /// could not cover the work, or there was no thermal headroom for it.
    Refused,
    /// Stopped part way through because it ran out of the energy it asked for.
    /// Not a failure — a budget doing its job.
    OverBudget,
}

pub struct Job {
    pub id: u64,
    pub owner: usize,
    pub qos: Qos,
    pub segments: u32,
    pub done_segments: u32,
    /// Absolute microsecond deadline, if it has one.
    pub deadline_us: Option<u64>,
    pub submitted_us: u64,
    pub finished_us: u64,
    pub state: JobState,
    /// Times the device was taken away from this job mid-run.
    pub preemptions: u64,
    /// Modelled energy, in microjoules.
    pub energy_uj: u64,
    /// What it was allowed to spend, if it said.
    pub energy_budget_uj: Option<u64>,
    /// Why it was refused, for the report.
    pub refusal: &'static str,
    /// Measured device time, in microseconds.
    pub device_us: u64,
}

impl Job {
    /// Latency so far, in microseconds.
    pub fn latency_us(&self) -> u64 {
        let end = if self.state == JobState::Done {
            self.finished_us
        } else {
            time::now_us()
        };
        end.saturating_sub(self.submitted_us)
    }

    /// Whether this job met its deadline. `None` if it had none, or has not
    /// finished — a refused job never ran, and reporting it as "met" because
    /// its finish time is zero would be the most flattering possible lie.
    pub fn met_deadline(&self) -> Option<bool> {
        if self.state != JobState::Done {
            return None;
        }
        let d = self.deadline_us?;
        Some(self.finished_us <= d)
    }

    pub fn state_label(&self) -> &'static str {
        match self.state {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::Refused => "refused",
            JobState::OverBudget => "overbudget",
        }
    }
}

struct Device {
    jobs: Vec<Job>,
    next_id: u64,
    /// Degrees above ambient, in thousandths.
    temp_milli_c: u64,
    thermal_updated_us: u64,
    /// Running average of what a segment actually costs.
    us_per_segment: u64,
    /// The job the device ran last, for counting preemptions.
    last_run: Option<u64>,
    segments_run: u64,
    peak_milli_c: u64,
}

static DEVICE: SpinLock<Device> = SpinLock::new(Device {
    jobs: Vec::new(),
    next_id: 1,
    temp_milli_c: 0,
    thermal_updated_us: 0,
    us_per_segment: INITIAL_SEGMENT_US,
    last_run: None,
    segments_run: 0,
    peak_milli_c: 0,
});

static REFUSED: AtomicU64 = AtomicU64::new(0);
static PREEMPTIONS: AtomicU64 = AtomicU64::new(0);

/// Why a submission was refused.
pub enum Reject {
    /// The work does not fit before the deadline, even at the front of the queue.
    Undeliverable { needed_us: u64, available_us: u64 },
    /// The device is too hot for work of this class right now.
    NoHeadroom { temp_milli_c: u64, limit_milli_c: u64 },
}

/// Apply cooling since the last update and return the current temperature.
fn temperature_locked(dev: &mut Device) -> u64 {
    let now = time::now_us();
    if dev.thermal_updated_us == 0 {
        dev.thermal_updated_us = now;
        return dev.temp_milli_c;
    }
    let ms = now.saturating_sub(dev.thermal_updated_us) / 1000;
    if ms > 0 {
        dev.temp_milli_c = dev.temp_milli_c.saturating_sub(ms * DECAY_PER_MS_MILLI_C);
        dev.thermal_updated_us = now;
    }
    dev.temp_milli_c
}

/// Degrees above ambient, in thousandths.
pub fn temperature() -> u64 {
    let mut dev = DEVICE.lock();
    temperature_locked(&mut dev)
}

pub fn throttle_point() -> u64 {
    THROTTLE_MILLI_C
}
pub fn critical_point() -> u64 {
    CRITICAL_MILLI_C
}

/// Offer a job to the device.
///
/// A job with a deadline is admitted only if it can actually be met, given what
/// is already queued at its priority or above. Refusing is the point: a system
/// that accepts everything and misses silently gives an application no way to
/// degrade gracefully, and gives the user a stutter instead of a smaller model.
pub fn submit(
    owner: usize,
    qos: Qos,
    segments: u32,
    deadline_in_us: Option<u64>,
) -> Result<u64, Reject> {
    submit_with_budget(owner, qos, segments, deadline_in_us, None)
}

/// Offer a job, saying what it may spend.
///
/// Two reasons to refuse, and both are better than accepting: the deadline
/// cannot be met, or the device is too hot for work of this class. A caller
/// told no can choose a smaller model, wait, or do without. A caller told yes
/// and then throttled mid-run cannot do any of those things.
///
/// `energy_budget_uj` is a cap rather than a reason to refuse: the job runs
/// until it has spent that much and then stops, which is what "do as much as
/// this buys" means.
pub fn submit_with_budget(
    owner: usize,
    qos: Qos,
    segments: u32,
    deadline_in_us: Option<u64>,
    energy_budget_uj: Option<u64>,
) -> Result<u64, Reject> {
    let now = time::now_us();
    let mut dev = DEVICE.lock();

    let cost = dev.us_per_segment;
    let needed_us = segments as u64 * cost;
    let needed_uj = needed_us * DEVICE_POWER_MW / 1000;

    // Thermal headroom. Interactive work is the user waiting, and is admitted
    // until the device is genuinely in trouble; everything else yields first,
    // which is the whole point of having classes.
    let temp = temperature_locked(&mut dev);
    let limit = match qos {
        Qos::Interactive => CRITICAL_MILLI_C,
        Qos::Foreground => CRITICAL_MILLI_C,
        _ => THROTTLE_MILLI_C,
    };
    if temp >= limit {
        REFUSED.fetch_add(1, Ordering::Relaxed);
        record_refusal(&mut dev, owner, qos, segments, deadline_in_us, now, energy_budget_uj,
                       "no thermal headroom");
        return Err(Reject::NoHeadroom { temp_milli_c: temp, limit_milli_c: limit });
    }

    // An energy budget is a cap, not a promise. "Do as much as five millijoules
    // buys" is a reasonable thing for background work to ask, so a cap smaller
    // than the work is admitted and binds later rather than being refused —
    // unlike a deadline, which is a request to be *finished* and can be
    // answered honestly at submission.
    let _ = needed_uj;

    if let Some(window) = deadline_in_us {
        // Work already queued that this job cannot push in front of.
        let ahead: u64 = dev
            .jobs
            .iter()
            .filter(|j| {
                matches!(j.state, JobState::Queued | JobState::Running) && j.qos <= qos
            })
            .map(|j| (j.segments - j.done_segments) as u64 * cost)
            .sum();

        if needed_us + ahead > window {
            REFUSED.fetch_add(1, Ordering::Relaxed);
            record_refusal(&mut dev, owner, qos, segments, deadline_in_us, now,
                           energy_budget_uj, "deadline cannot be met");
            return Err(Reject::Undeliverable {
                needed_us: needed_us + ahead,
                available_us: window,
            });
        }
    }

    let id = dev.next_id;
    dev.next_id += 1;
    dev.jobs.push(Job {
        id,
        owner,
        qos,
        segments,
        done_segments: 0,
        deadline_us: deadline_in_us.map(|w| now + w),
        submitted_us: now,
        finished_us: 0,
        state: JobState::Queued,
        preemptions: 0,
        energy_uj: 0,
        energy_budget_uj,
        device_us: 0,
        refusal: "",
    });
    drop(dev);

    // The worker is asleep until there is something to run. Waking it here,
    // after the lock is released, is what lets it stop polling — and polling
    // once a tick forever is what an idle phone cannot afford.
    sched::wake_all_on(DEVICE_TOKEN);
    Ok(id)
}

/// Keep a record of work that was turned away. A refusal nobody can see is
/// indistinguishable from work that silently never ran.
#[allow(clippy::too_many_arguments)]
fn record_refusal(
    dev: &mut Device,
    owner: usize,
    qos: Qos,
    segments: u32,
    deadline_in_us: Option<u64>,
    now: u64,
    energy_budget_uj: Option<u64>,
    why: &'static str,
) {
    let id = dev.next_id;
    dev.next_id += 1;
    dev.jobs.push(Job {
        id,
        owner,
        qos,
        segments,
        done_segments: 0,
        deadline_us: deadline_in_us.map(|w| now + w),
        submitted_us: now,
        finished_us: 0,
        state: JobState::Refused,
        preemptions: 0,
        energy_uj: 0,
        energy_budget_uj,
        device_us: 0,
        refusal: why,
    });
}

/// Pick the next job: highest class first, and within a class the nearest
/// deadline. Opportunistic work is only chosen when nothing else is waiting,
/// which is what makes it the elastic band rather than just low priority.
fn pick(dev: &Device) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, j) in dev.jobs.iter().enumerate() {
        if !matches!(j.state, JobState::Queued | JobState::Running) {
            continue;
        }
        if j.done_segments >= j.segments {
            continue;
        }
        let better = match best {
            None => true,
            Some(b) => {
                let c = &dev.jobs[b];
                (j.qos, j.deadline_us.unwrap_or(u64::MAX))
                    < (c.qos, c.deadline_us.unwrap_or(u64::MAX))
            }
        };
        if better {
            best = Some(i);
        }
    }
    best
}

/// One segment of work.
///
/// Stands in for a slice of a graph: a bounded, non-preemptible chunk of
/// arithmetic. The numbers are meaningless; the time it takes is not, because
/// that is what the scheduler measures and budgets against.
fn run_segment() -> u64 {
    const ITERATIONS: u64 = 200_000;
    let start = time::now_us();
    let mut acc: u64 = 0x9E37_79B9_7F4A_7C15;
    for i in 0..ITERATIONS {
        acc = acc.wrapping_mul(6364136223846793005).wrapping_add(i);
        acc ^= acc >> 29;
    }
    // Keep the compiler from deciding none of that was necessary.
    core::hint::black_box(acc);
    time::now_us().saturating_sub(start)
}

/// What the worker blocks on when there is no work.
///
/// Any value that nothing else uses; channels block on their own index, so
/// this is deliberately far from those.
const DEVICE_TOKEN: u64 = 0xDEC0_DE00_7E45_0201;

/// The device worker. One thread, one accelerator.
///
/// It sleeps on `DEVICE_TOKEN` rather than waking once a tick to ask whether
/// anything has arrived. That polling loop was invisible while the kernel had
/// a periodic tick — one more wakeup among a hundred — and became the entire
/// idle cost the moment the tick went away. It is the usual shape of the
/// problem: power is not spent by the thing you are measuring, it is spent by
/// whatever was hiding behind it.
pub fn worker(_: usize) {
    loop {
        let chosen = {
            let mut dev = DEVICE.lock();
            match pick(&dev) {
                None => {
                    dev.last_run = None;
                    // Marked blocked while still holding the device lock, so a
                    // submit either lands before this and is seen, or finds
                    // this thread already waiting and wakes it. The same
                    // structural fix as `ipc::recv_or_prepare`, for the same
                    // reason: checking and then blocking has a window in it.
                    sched::prepare_block(DEVICE_TOKEN);
                    None
                }
                Some(i) => {
                    let id = dev.jobs[i].id;

                    // Taking the device from a job that still has work is a
                    // preemption, and belongs on that job's record rather than
                    // in an aggregate nobody can attribute.
                    if let Some(prev) = dev.last_run {
                        if prev != id {
                            if let Some(p) = dev.jobs.iter_mut().find(|j| {
                                j.id == prev && j.done_segments < j.segments
                            }) {
                                p.preemptions += 1;
                                PREEMPTIONS.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    dev.last_run = Some(id);
                    dev.jobs[i].state = JobState::Running;
                    Some(id)
                }
            }
        };

        let Some(id) = chosen else {
            sched::block(DEVICE_TOKEN);
            continue;
        };

        // Outside the lock: a segment is the one thing here that takes real
        // time, and holding the device lock across it would stop anything else
        // from being submitted or inspected.
        let elapsed = run_segment();

        let finished = {
            let mut dev = DEVICE.lock();
            let cost = dev.us_per_segment;
            // Exponential average, so admission decisions track reality rather
            // than the guess this started with.
            dev.us_per_segment = (cost * 3 + elapsed) / 4;
            dev.segments_run += 1;

            // Work makes heat, and the cooling since the last segment is
            // applied at the same time.
            temperature_locked(&mut dev);
            dev.temp_milli_c += RISE_PER_SEGMENT_MILLI_C;
            if dev.temp_milli_c > dev.peak_milli_c {
                dev.peak_milli_c = dev.temp_milli_c;
            }

            let mut finished = None;
            if let Some(j) = dev.jobs.iter_mut().find(|j| j.id == id) {
                j.done_segments += 1;
                j.device_us += elapsed;
                j.energy_uj += elapsed * DEVICE_POWER_MW / 1000;

                // A budget that is not enforced is a comment.
                let spent_out = matches!(j.energy_budget_uj, Some(b) if j.energy_uj >= b);
                if j.done_segments >= j.segments {
                    j.state = JobState::Done;
                    j.finished_us = time::now_us();
                    finished = Some(j.id);
                } else if spent_out {
                    j.state = JobState::OverBudget;
                    j.finished_us = time::now_us();
                    finished = Some(j.id);
                }
            }
            finished
        };

        // Woken after the lock is dropped, and only once the job is marked
        // done — a waiter that marked itself blocked while holding the device
        // cannot have been missed.
        if let Some(done) = finished {
            sched::wake_all_on(token(done));
        }
    }
}

/// Has this job finished? Marks the caller as waiting if not, atomically —
/// the same discipline `ipc::recv_or_prepare` uses, and for the same reason.
pub fn finished_or_prepare(id: u64, token: u64) -> Option<bool> {
    let dev = DEVICE.lock();
    let j = dev.jobs.iter().find(|j| j.id == id)?;
    match j.state {
        JobState::Done => Some(true),
        JobState::Refused | JobState::OverBudget => Some(false),
        _ => {
            sched::prepare_block(token);
            None
        }
    }
}

/// Wake token for a job.
pub const fn token(id: u64) -> u64 {
    0x7E45_0000_0000 | id
}

/// (id, class, segments, done, latency us, deadline us or 0, met, preemptions,
/// energy uj, device us, state)
pub fn stat(id: u64) -> Option<(u64, u64, u32, u32, u64, u64, u64, u64, u64, u64, u64)> {
    let dev = DEVICE.lock();
    let j = dev.jobs.iter().find(|j| j.id == id)?;
    Some((
        j.id,
        j.qos as u64,
        j.segments,
        j.done_segments,
        j.latency_us(),
        j.deadline_us.unwrap_or(0),
        match j.met_deadline() {
            Some(true) => 1,
            Some(false) => 0,
            None => 2,
        },
        j.preemptions,
        j.energy_uj,
        j.device_us,
        j.state as u64,
    ))
}

/// Run `f` over every job. Must not call back into this module.
pub fn for_each(mut f: impl FnMut(&Job)) {
    let dev = DEVICE.lock();
    for j in dev.jobs.iter() {
        f(j);
    }
}

/// Measured cost of one segment, in microseconds.
pub fn segment_cost_us() -> u64 {
    DEVICE.lock().us_per_segment
}

/// (segments executed, jobs refused, preemptions)
pub fn totals() -> (u64, u64, u64) {
    let dev = DEVICE.lock();
    (
        dev.segments_run,
        REFUSED.load(Ordering::Relaxed),
        PREEMPTIONS.load(Ordering::Relaxed),
    )
}

/// Hottest the device has been, in thousandths of a degree above ambient.
pub fn peak_temperature() -> u64 {
    DEVICE.lock().peak_milli_c
}

/// Total modelled energy across every job, in microjoules.
pub fn total_energy_uj() -> u64 {
    DEVICE.lock().jobs.iter().map(|j| j.energy_uj).sum()
}
