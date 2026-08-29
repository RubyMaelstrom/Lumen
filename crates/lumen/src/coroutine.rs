//! Coroutine continuations for generators and async functions.
//!
//! Compiler-supported bodies retain a heap-owned bytecode VM stack, including ordinary and
//! delegated yields and awaits. A body containing a construct the bytecode compiler cannot yet
//! lower falls back to a bounded OS-thread worker: control is handed back and forth with a pair of
//! channels in strict ping-pong, so exactly one side touches the shared [`Interp`] at a time.
//!
//! Fallback workers are pooled and capped. Finished workers return to an idle pool and are handed
//! the next uncompilable coroutine instead of paying a spawn and stack reservation on every call.
//!
//! The running coroutine's channels live in a thread-local [`YIELDER`], so a `yield` buried deep in
//! eval finds the right channel and nested coroutines (each on their own worker) need no extra
//! bookkeeping — every thread reads its own thread-local.
//!
//! Address and teardown safety: `Engine` boxes the interpreter, so moving the public engine does
//! not invalidate a captured pointer. `Interp::drop` wakes every suspended body and waits for its
//! Rust stack to unwind before destroying the object graph the body references.

use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, OnceLock};

use crate::interpreter::Interp;
use crate::value::Value;

/// Driver → generator: resume the body.
#[derive(Clone)]
pub enum Resume {
    /// `next(v)` — the `yield` expression evaluates to `v`.
    Next(Value),
    /// `return(v)` — inject a return completion at the suspended `yield`.
    Return(Value),
    /// `throw(e)` — inject a throw at the suspended `yield`.
    Throw(Value),
    /// Host realm teardown — unwind through the engine's non-catchable interruption completion.
    Terminate,
}

// `Resume`/`Suspend` carry `Value`s (which hold non-`Send` `Rc`s). Transferring them across the
// channel is sound because of the strict ping-pong: a value is produced on one side only after the
// other side has parked, so it is never touched on two threads at once.
unsafe impl Send for Resume {}
unsafe impl Send for Suspend {}

/// Generator → driver: the body parked or finished.
pub enum Suspend {
    /// `yield v` — parked, produced `v` (a generator value).
    Yield(Value),
    /// `await v` — parked waiting for `v` to settle (async functions/generators).
    Await(Value),
    /// The body ran to completion / `return v`.
    Done(Value),
    /// The body threw `e` and it escaped.
    Throw(Value),
}

/// A `*mut Interp` carried to the generator thread. Sound only under the strict ping-pong handoff:
/// when the generator thread dereferences it the driver is parked (not touching the interpreter),
/// and vice versa, so the two `&mut` reborrows are never *used* concurrently.
pub struct InterpPtr(pub *mut Interp);
unsafe impl Send for InterpPtr {}

/// The generator body, boxed. It captures `Rc`s (the function + its scope) so it is not really
/// `Send`; the strict handoff makes moving it to the worker thread sound.
pub struct SendBody(pub Box<dyn FnOnce(&mut Interp) -> Suspend>);
unsafe impl Send for SendBody {}

/// The generator-thread side of the channels, kept in the worker thread's TLS.
struct Yielder {
    suspend_tx: Sender<Suspend>,
    resume_rx: Receiver<Resume>,
}

thread_local! {
    static YIELDER: RefCell<Option<Yielder>> = const { RefCell::new(None) };
    /// Set on the coroutine thread when the body is an *async* generator, so `yield` knows to
    /// `Await` its operand (AsyncGeneratorYield) before suspending.
    static ASYNC_GEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the current thread is executing a generator body (so `yield` is legal here).
pub fn in_coroutine() -> bool {
    YIELDER.with(|y| y.borrow().is_some())
}

/// Mark the running coroutine thread as an async generator body.
pub fn set_async_gen(v: bool) {
    ASYNC_GEN.with(|c| c.set(v));
}

/// Whether the running coroutine is an async generator (its `yield` awaits the operand).
pub fn in_async_gen() -> bool {
    ASYNC_GEN.with(|c| c.get())
}

/// The driver side of one coroutine, stored on the generator object in `Interp.generators`. Either
/// an OS-thread-backed coroutine (generators, and async bodies the bytecode compiler declined) or a
/// bytecode [`VmCoro`](crate::bytecode::VmCoro) (async bodies that compile) — both drive the same
/// way, so `drive_async`/`drive_generator` are agnostic.
pub enum Coroutine {
    /// A generator in the ECMA-262 suspended-start state. Its execution context is represented by
    /// the captured body, but no native stack is reserved until the first actual resumption.
    Lazy(LazyCoro),
    Thread(ThreadCoro),
    Vm(Box<crate::bytecode::VmCoro>),
    /// The explicit state machine for the built-in async algorithm Array.fromAsync. Unlike a
    /// source async function it has no bytecode body, but it parks at the same Await boundaries.
    FromAsync(Box<crate::builtins::FromAsyncCoro>),
}

impl Coroutine {
    #[inline]
    pub(crate) fn resume(&mut self, i: &mut Interp, signal: Resume) -> Suspend {
        match self {
            Coroutine::Lazy(_) => self.resume_lazy(i, signal),
            Coroutine::Thread(c) => c.resume(i, signal),
            Coroutine::Vm(c) => c.resume(i, signal),
            Coroutine::FromAsync(c) => c.resume(i, signal),
        }
    }

    /// Materialize a suspended-start generator's native execution context only when GeneratorResume
    /// actually runs it. GeneratorResumeAbrupt with return/throw completes it without allocating or
    /// evaluating the body (ECMA-262 GeneratorResumeAbrupt).
    fn resume_lazy(&mut self, i: &mut Interp, signal: Resume) -> Suspend {
        let placeholder = Coroutine::Lazy(LazyCoro {
            body: None,
            done: true,
            started: false,
        });
        let Coroutine::Lazy(mut lazy) = std::mem::replace(self, placeholder) else {
            unreachable!()
        };
        let result = match signal {
            Resume::Next(value) => {
                lazy.started = true;
                let body = lazy
                    .body
                    .take()
                    .expect("a suspended-start coroutine retains its body");
                match spawn_coroutine(i as *mut Interp, body) {
                    Ok(mut running) => {
                        let result = running.resume(i, Resume::Next(value));
                        *self = running;
                        return result;
                    }
                    Err(_) => Suspend::Throw(i.make_error("Error", UNSUPPORTED_MSG)),
                }
            }
            Resume::Return(value) => Suspend::Done(value),
            Resume::Throw(error) => Suspend::Throw(error),
            Resume::Terminate => Suspend::Done(Value::Undefined),
        };
        lazy.done = true;
        *self = Coroutine::Lazy(lazy);
        result
    }

    /// Whether the body has finished (further resumes are no-ops).
    #[inline]
    pub fn done(&self) -> bool {
        match self {
            Coroutine::Lazy(c) => c.done,
            Coroutine::Thread(c) => c.done,
            Coroutine::Vm(c) => c.done,
            Coroutine::FromAsync(c) => c.done(),
        }
    }
    /// Whether the first resume has happened (distinguishes suspendedStart from a suspended yield).
    #[inline]
    pub fn started(&self) -> bool {
        match self {
            Coroutine::Lazy(c) => c.started,
            Coroutine::Thread(c) => c.started,
            Coroutine::Vm(c) => c.started,
            Coroutine::FromAsync(c) => c.started(),
        }
    }

    /// Acknowledge teardown of a thread-backed suspended body before its interpreter disappears.
    pub(crate) fn terminate(&mut self, i: &mut Interp) {
        match self {
            Coroutine::Lazy(c) => {
                c.body.take();
                c.done = true;
            }
            Coroutine::Thread(c) => c.terminate(i),
            Coroutine::Vm(_) => {}
            Coroutine::FromAsync(c) => c.terminate(),
        }
    }
}

/// A generator's not-yet-resumed execution state. Keeping this closure is equivalent to the
/// specification's suspended execution context while avoiding one large native stack per inert
/// generator object.
pub struct LazyCoro {
    body: Option<SendBody>,
    done: bool,
    started: bool,
}

/// Create a suspended-start coroutine without allocating an OS worker.
pub fn lazy_coroutine(body: SendBody) -> Coroutine {
    Coroutine::Lazy(LazyCoro {
        body: Some(body),
        done: false,
        started: false,
    })
}

/// An OS-thread-backed coroutine (a pooled worker runs the body; see [`spawn_coroutine`]).
pub struct ThreadCoro {
    resume_tx: Sender<Resume>,
    suspend_rx: Receiver<Suspend>,
    /// Set once the body has finished (Done/Throw); further resumes are no-ops.
    pub done: bool,
    /// Set on the first resume — distinguishes "suspendedStart" from a suspended yield.
    pub started: bool,
    /// Frame-ownership tag: `FnFrame`s pushed while this coroutine's body runs carry this id
    /// (via `Interp::cur_coro`), so a worker-thread panic can evict exactly the dead body's
    /// frames — they may be interleaved with the driver's own frames across suspensions, so a
    /// watermark truncate cannot find them.
    pub(crate) id: u32,
}

/// Allocates [`ThreadCoro::id`]s; 0 is reserved for "not in a coroutine body" (the main driver).
static CORO_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

impl ThreadCoro {
    /// Hand control to the generator and block until it next parks or finishes. Saves/restores the
    /// interpreter's scalar execution context (`strict`, recursion `depth`, tail-call eligibility
    /// `tco_ok`) across the handoff so the driver and the body don't clobber each other's. `tco_ok`
    /// matters because a coroutine body executes outside `Interp::call`'s tail-call trampoline: if a
    /// leaked `tco_ok == true` reached an `async`/generator body, its `return f(...)` would be parked
    /// as a pending tail call that nothing ever runs, and the body would resolve to `undefined`.
    pub(crate) fn resume(&mut self, i: &mut Interp, signal: Resume) -> Suspend {
        if self.done {
            return Suspend::Done(Value::Undefined);
        }
        i.activate_gc_heap();
        self.started = true;
        let (saved_strict, saved_depth, saved_tco) = (i.strict, i.depth, i.tco_ok);
        let saved_coro = std::mem::replace(&mut i.cur_coro, self.id);
        let _ = self.resume_tx.send(signal);
        let s = self.suspend_rx.recv();
        // `send`/`recv` is also the execution-context ownership handoff. The worker clears its
        // activation before publishing a terminal outcome; restore the driver activation before
        // any returned object is inspected or dropped here.
        i.activate_gc_heap();
        i.cur_coro = saved_coro;
        i.strict = saved_strict;
        i.depth = saved_depth;
        i.tco_ok = saved_tco;
        match s {
            Ok(s) => {
                if matches!(s, Suspend::Done(_) | Suspend::Throw(_)) {
                    self.done = true;
                }
                s
            }
            // The worker died (panicked) — treat as a finished generator. Its unwind skipped the
            // straight-line `fn_frames.pop()`s of any calls the body had in flight — across ALL
            // its resumes, whose frames may be interleaved with the driver's — while dropping
            // their callee handles, so those frames' `fn_ptr`s no longer point at live objects
            // (see `FnFrame::fn_ptr`). Evict exactly this body's frames by ownership tag before
            // anything (an error's `capture_stack`, `f.caller` reflection) reconstructs a handle
            // through one.
            Err(_) => {
                i.fn_frames.retain(|f| f.coro != self.id);
                self.done = true;
                Suspend::Done(Value::Undefined)
            }
        }
    }

    /// Stop a suspended body before the interpreter which owns its raw pointer is dropped.
    fn terminate(&mut self, i: &mut Interp) {
        if self.done {
            return;
        }
        // `resume` performs the normal scalar-context handoff and waits for the worker's unwind
        // acknowledgement. `Terminate` becomes the same non-catchable control completion used by
        // host script interruption, so this works under both panic=unwind and panic=abort builds.
        let _ = self.resume(i, Resume::Terminate);
    }
}

/// Park the running coroutine, hand `msg` (a `Yield` or `Await`) to the driver, and block until
/// resumed. Restores the body's scalar context (which the driver mutated while it ran).
fn park(i: &mut Interp, msg: Suspend) -> Resume {
    let (gen_strict, gen_depth, gen_tco) = (i.strict, i.depth, i.tco_ok);
    let resumed = YIELDER.with(|y| {
        let b = y.borrow();
        let yl = b.as_ref().expect("suspend outside a coroutine");
        let _ = yl.suspend_tx.send(msg);
        yl.resume_rx.recv()
    });
    match resumed {
        Ok(r) => {
            i.strict = gen_strict;
            i.depth = gen_depth;
            i.tco_ok = gen_tco;
            r
        }
        // Normal engine teardown uses the acknowledged termination flag above. A disconnected
        // driver here means an abnormal owner failure; never touch its potentially-destroyed
        // interpreter again.
        Err(_) => loop {
            std::thread::park();
        },
    }
}

/// `yield value` — park producing a generator value.
pub fn coroutine_yield(i: &mut Interp, value: Value) -> Resume {
    park(i, Suspend::Yield(value))
}

/// `await value` — park waiting for `value` to settle.
pub fn coroutine_await(i: &mut Interp, value: Value) -> Resume {
    park(i, Suspend::Await(value))
}

/// Thrown as a JS `Error` when a coroutine cannot start (wasm32 has no OS threads, so
/// `std::thread::Builder::spawn` reports `Unsupported` there).
pub const UNSUPPORTED_MSG: &str = "native coroutine capacity is exhausted or unavailable";

/// One unit of work for a pooled worker: the interpreter pointer, the body to run, and this
/// coroutine's channel ends. All fields are `Send` (via the `unsafe impl`s above / channel `Send`),
/// so `Job` is `Send`; moving the captured `Rc`s to the worker is sound for the same ping-pong
/// reason `spawn` was — the driver stops touching them the instant it sends.
struct Job {
    ptr: InterpPtr,
    body: SendBody,
    resume_rx: Receiver<Resume>,
    suspend_tx: Sender<Suspend>,
    /// Holds one slot in the process-wide native-coroutine budget until this body terminates.
    _permit: LivePermit,
}

/// Idle worker threads waiting for their next coroutine. Guarded by a plain `Mutex`: under the
/// strict ping-pong exactly one thread touches the interpreter at a time, so contention is
/// near-zero (a worker only pushes itself back *after* handing its final value to the driver).
static IDLE: Mutex<Vec<Sender<Job>>> = Mutex::new(Vec::new());

const DEFAULT_COROUTINE_STACK_SIZE: usize = 64 * 1024 * 1024;
const MIN_COROUTINE_STACK_SIZE: usize = 2 * 1024 * 1024;
const MAX_COROUTINE_STACK_SIZE: usize = 64 * 1024 * 1024;
const DEFAULT_MAX_LIVE_COROUTINES: usize = 64;
const MAX_CONFIGURED_LIVE_COROUTINES: usize = 1_024;

static COROUTINE_STACK_SIZE: OnceLock<usize> = OnceLock::new();
static MAX_LIVE_COROUTINES: OnceLock<usize> = OnceLock::new();
static LIVE_COROUTINES: AtomicUsize = AtomicUsize::new(0);

fn bounded_value(raw: Option<&str>, default: usize, minimum: usize, maximum: usize) -> usize {
    raw.and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(minimum, maximum)
}

/// Native stacks remain necessary for tree-walked generators. Keep the correctness-tested 64 MiB
/// default, but let embedders trade recursion headroom for address-space use. The hard ceiling
/// prevents an environment typo from reserving unbounded virtual memory.
fn coroutine_stack_size() -> usize {
    *COROUTINE_STACK_SIZE.get_or_init(|| {
        bounded_value(
            std::env::var("LUMEN_COROUTINE_STACK_BYTES").ok().as_deref(),
            DEFAULT_COROUTINE_STACK_SIZE,
            MIN_COROUTINE_STACK_SIZE,
            MAX_COROUTINE_STACK_SIZE,
        )
    })
}

/// `LUMEN_MAX_COROUTINE_THREADS` bounds simultaneously live stackful bodies. Bytecode VM
/// continuations do not consume this budget. Exhaustion is reported to JS instead of blocking the
/// sole Agent driver, which could deadlock when a running coroutine creates a nested generator.
fn max_live_coroutines() -> usize {
    *MAX_LIVE_COROUTINES.get_or_init(|| {
        bounded_value(
            std::env::var("LUMEN_MAX_COROUTINE_THREADS").ok().as_deref(),
            DEFAULT_MAX_LIVE_COROUTINES,
            1,
            MAX_CONFIGURED_LIVE_COROUTINES,
        )
    })
}

struct LivePermit;

impl LivePermit {
    fn acquire() -> std::io::Result<Self> {
        LIVE_COROUTINES
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                (live < max_live_coroutines()).then_some(live + 1)
            })
            .map(|_| Self)
            .map_err(|_| std::io::Error::other("native coroutine limit reached"))
    }
}

impl Drop for LivePermit {
    fn drop(&mut self) {
        let previous = LIVE_COROUTINES.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0);
    }
}

/// Keep enough warm workers for ordinary async bursts, but release excess high-water capacity.
/// Live coroutines still retain their workers; this bound applies only after a body has completed
/// or has been explicitly unwound during realm teardown.
const MAX_IDLE_WORKERS: usize = 8;

/// Grab an idle worker, or start a new one. `Err` when the platform cannot spawn threads (wasm32).
fn get_worker() -> std::io::Result<Sender<Job>> {
    if let Some(tx) = IDLE.lock().unwrap().pop() {
        return Ok(tx);
    }
    let (job_tx, job_rx) = channel::<Job>();
    let self_tx = job_tx.clone();
    std::thread::Builder::new()
        .name("lumen-coroutine".to_string())
        .stack_size(coroutine_stack_size())
        .spawn(move || worker_loop(job_rx, self_tx))?;
    Ok(job_tx)
}

/// A pooled worker: run one coroutine to completion, return to the idle pool, repeat. Normal engine
/// teardown is an acknowledged unwind, so a worker is reusable even when its generator was still
/// suspended.
fn worker_loop(job_rx: Receiver<Job>, self_tx: Sender<Job>) {
    while let Ok(job) = job_rx.recv() {
        run_job(job);
        let mut idle = IDLE.lock().unwrap();
        if idle.len() >= MAX_IDLE_WORKERS {
            return;
        }
        idle.push(self_tx.clone());
    }
}

struct GcActivation;

impl Drop for GcActivation {
    fn drop(&mut self) {
        // Run before the terminal channel send (and, during unwinding, before channel teardown)
        // so the driver's wakeup cannot race an Rc decrement in this worker's activation slot.
        crate::value::deactivate_gc_heap();
    }
}

/// Set up this thread's coroutine TLS, run the body from its first drive to completion, and hand
/// the outcome to the driver. Resets the per-thread coroutine state a reused worker would otherwise
/// inherit from its previous job.
fn run_job(job: Job) {
    let Job {
        ptr,
        body,
        resume_rx,
        suspend_tx,
        _permit,
    } = job;
    let SendBody(body) = body;
    // A plain async body never sets ASYNC_GEN, so it must not inherit a previous async-generator
    // job's `true`.
    ASYNC_GEN.with(|c| c.set(false));
    YIELDER.with(|y| {
        *y.borrow_mut() = Some(Yielder {
            suspend_tx: suspend_tx.clone(),
            resume_rx,
        })
    });
    // Park until the first next()/return()/throw(); the body doesn't run before then.
    let first = YIELDER.with(|y| y.borrow().as_ref().unwrap().resume_rx.recv());
    let outcome = match first {
        Err(_) => {
            YIELDER.with(|y| *y.borrow_mut() = None);
            return; // dropped before first drive
        }
        Ok(Resume::Next(_)) => {
            let interp = unsafe { &mut *ptr.0 };
            interp.activate_gc_heap();
            let _activation = GcActivation;
            body(interp)
        }
        Ok(Resume::Return(v)) => Suspend::Done(v),
        Ok(Resume::Throw(e)) => Suspend::Throw(e),
        Ok(Resume::Terminate) => Suspend::Done(Value::Undefined),
    };
    let _ = suspend_tx.send(outcome);
    // Clear the TLS so the next job starts clean and `in_coroutine()` reads false between jobs.
    YIELDER.with(|y| *y.borrow_mut() = None);
}

/// Spawn a coroutine over `body` on a pooled worker, parked until its first [`Coroutine::resume`].
/// `Err` when the platform cannot spawn threads (wasm32).
pub fn spawn_coroutine(interp: *mut Interp, body: SendBody) -> std::io::Result<Coroutine> {
    let permit = LivePermit::acquire()?;
    let (resume_tx, resume_rx) = channel::<Resume>();
    let (suspend_tx, suspend_rx) = channel::<Suspend>();
    let worker = get_worker()?;
    let job = Job {
        ptr: InterpPtr(interp),
        body,
        resume_rx,
        suspend_tx,
        _permit: permit,
    };
    // The worker is idle in `job_rx.recv()`; hand it this coroutine. A send failure means the worker
    // vanished — surface it like a failed spawn rather than wedging.
    worker
        .send(job)
        .map_err(|_| std::io::Error::other("coroutine worker unavailable"))?;
    Ok(Coroutine::Thread(ThreadCoro {
        id: CORO_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        resume_tx,
        suspend_rx,
        done: false,
        started: false,
    }))
}

#[cfg(test)]
mod tests {
    use super::bounded_value;

    #[test]
    fn coroutine_environment_limits_are_clamped_and_invalid_values_use_defaults() {
        assert_eq!(bounded_value(None, 8, 2, 16), 8);
        assert_eq!(bounded_value(Some("invalid"), 8, 2, 16), 8);
        assert_eq!(bounded_value(Some("0"), 8, 2, 16), 2);
        assert_eq!(bounded_value(Some("999"), 8, 2, 16), 16);
        assert_eq!(bounded_value(Some("12"), 8, 2, 16), 12);
    }
}
