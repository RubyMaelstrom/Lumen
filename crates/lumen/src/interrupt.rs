//! Host-driven script interruption.
//!
//! HTML §8.1.4.5 permits a user agent to abort a running script without executing author
//! `catch`/`finally` machinery; the Worker termination algorithm requires that ability from a
//! thread running in parallel with the worker. This is therefore an engine control completion,
//! not a JavaScript exception.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// Why the host stopped the current JavaScript execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterruptReason {
    Cancelled,
    UserNavigation,
    DeadlineExceeded,
}

impl InterruptReason {
    pub fn message(self) -> &'static str {
        match self {
            Self::Cancelled => "JavaScript execution cancelled by the host",
            Self::UserNavigation => "JavaScript execution yielded to user navigation",
            Self::DeadlineExceeded => "JavaScript execution deadline exceeded",
        }
    }
}

/// Thread-safe control handle polled by every execution tier.
///
/// Cancellation is permanent and is used for realm/worker teardown. User-navigation requests and
/// deadlines are reusable: the host clears/rearms them at the next task boundary.
#[derive(Debug, Default)]
pub struct RuntimeInterrupt {
    cancelled: AtomicBool,
    user_navigation: AtomicBool,
    deadline_armed: AtomicBool,
    deadline: Mutex<Option<Instant>>,
}

impl RuntimeInterrupt {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn request_user_navigation(&self) {
        self.user_navigation.store(true, Ordering::Release);
    }

    pub fn begin_user_interaction(&self) {
        self.user_navigation.store(false, Ordering::Release);
    }

    pub fn user_navigation_requested(&self) -> bool {
        self.user_navigation.load(Ordering::Acquire)
    }

    pub fn set_deadline(&self, deadline: Option<Instant>) {
        let armed = deadline.is_some();
        *self
            .deadline
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = deadline;
        self.deadline_armed.store(armed, Ordering::Release);
    }

    /// The currently active reason, in host-control priority order.
    pub fn current_reason(&self) -> Option<InterruptReason> {
        if self.cancelled.load(Ordering::Acquire) {
            return Some(InterruptReason::Cancelled);
        }
        if self.user_navigation.load(Ordering::Acquire) {
            return Some(InterruptReason::UserNavigation);
        }
        if self.deadline_armed.load(Ordering::Acquire)
            && self
                .deadline
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Some(InterruptReason::DeadlineExceeded);
        }
        None
    }
}
