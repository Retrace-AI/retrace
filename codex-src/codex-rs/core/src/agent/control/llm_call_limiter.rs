use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use codex_protocol::ThreadId;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

use super::AgentControl;

/// Maximum number of concurrent LLM sampling streams shared across the root
/// agent and every sub-agent spawned beneath it in a single mission/session
/// tree.
///
/// This is the hard ceiling the product requires: the main agent and all of its
/// sub-agents *combined* may only have this many model calls in flight at once.
pub(crate) const MAX_CONCURRENT_LLM_CALLS: usize = 2;

/// Fair, shared limiter that bounds how many LLM sampling streams can run at
/// once across a root session and all sub-agents spawned from it.
///
/// `AgentControl` owns one of these and clones the inner `Arc` into every
/// sub-agent (the whole `AgentControl` is cloned on spawn), so the main agent
/// and all sub-agents contend for the same tiny pool of permits.
///
/// Tokio's [`Semaphore`] hands out permits in FIFO order, which gives us the
/// "rotation" behaviour the product requires: an agent that asks for a slot
/// while all slots are taken waits in line behind earlier waiters and is served
/// before any later arrival. That prevents a busy agent from monopolising the
/// model while a starved agent never gets a turn.
pub(super) struct LlmCallLimiter {
    semaphore: Arc<Semaphore>,
    /// Threads that have already surfaced a user-visible "waiting for a shared
    /// model slot" notice. Tracked so the notice is shown at most once per
    /// thread instead of on every contended sampling attempt (which, with only
    /// two shared slots and several agents, would otherwise flood history).
    announced_wait_threads: Mutex<HashSet<ThreadId>>,
}

impl Default for LlmCallLimiter {
    fn default() -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_LLM_CALLS)),
            announced_wait_threads: Mutex::new(HashSet::new()),
        }
    }
}

/// RAII slot held for the duration of a single model sampling stream. Dropping
/// it returns the slot to the shared pool so a waiting agent can proceed.
///
/// The permit must be scoped to the model stream *only*. It must never be held
/// while awaiting sub-agents (e.g. across a `wait_agent` tool call), or a parent
/// could occupy a slot while blocking on a child that itself needs a slot —
/// a self-inflicted deadlock.
pub(crate) struct LlmCallPermit {
    _permit: OwnedSemaphorePermit,
}

impl LlmCallLimiter {
    /// Try to take a slot without waiting. Returns `None` when all slots are
    /// currently in use.
    fn try_acquire(&self) -> Option<LlmCallPermit> {
        Arc::clone(&self.semaphore)
            .try_acquire_owned()
            .ok()
            .map(|permit| LlmCallPermit { _permit: permit })
    }

    /// Wait (FIFO) until a slot frees, then take it.
    async fn acquire(&self) -> LlmCallPermit {
        let permit = Arc::clone(&self.semaphore)
            .acquire_owned()
            .await
            .expect("shared LLM-call semaphore is never closed");
        LlmCallPermit { _permit: permit }
    }

    /// Number of slots currently available. Used by tests and diagnostics.
    #[cfg_attr(not(test), allow(dead_code))]
    fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// Returns `true` the first time a given thread has to wait for a slot,
    /// `false` afterwards, so callers can show a one-time wait notice per thread.
    fn take_wait_announcement(&self, thread_id: ThreadId) -> bool {
        self.announced_wait_threads
            .lock()
            .expect("llm-call wait announcement mutex is never poisoned")
            .insert(thread_id)
    }
}

impl AgentControl {
    /// Try to grab a shared LLM-call slot without waiting.
    ///
    /// Callers use this on the fast path so they can proceed silently when a
    /// slot is free, and only surface a visible "waiting" state (then call
    /// [`AgentControl::acquire_llm_call_slot`]) when this returns `None`.
    pub(crate) fn try_acquire_llm_call_slot(&self) -> Option<LlmCallPermit> {
        self.llm_call_limiter.try_acquire()
    }

    /// Wait (FIFO) for a shared LLM-call slot, then take it.
    pub(crate) async fn acquire_llm_call_slot(&self) -> LlmCallPermit {
        self.llm_call_limiter.acquire().await
    }

    /// Records and reports whether `thread_id` should surface a one-time,
    /// user-visible notice that it is waiting for one of the shared model slots.
    /// Returns `true` only on the first contended wait for that thread.
    pub(crate) fn should_announce_llm_wait(&self, thread_id: ThreadId) -> bool {
        self.llm_call_limiter.take_wait_announcement(thread_id)
    }

    #[cfg(test)]
    pub(crate) fn available_llm_call_slots(&self) -> usize {
        self.llm_call_limiter.available_permits()
    }
}

#[cfg(test)]
#[path = "llm_call_limiter_tests.rs"]
mod tests;
