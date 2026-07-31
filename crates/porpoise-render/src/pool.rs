//! A fixed pool of worker threads that rasterize pages off the UI thread.
//!
//! The contract is that submitting work never blocks and never fails loudly:
//! the caller submits, keeps drawing, and picks up results whenever they appear.
//! A viewer that blocks on rasterization cannot scroll smoothly no matter how
//! fast the renderer is.
//!
//! # More than one document
//!
//! A merge (`docs/goal-5-plan.md` §4) can put pages from more than one document on
//! screen, and the pool serves all of them rather than being rebuilt per document.
//! The obvious alternative — one pool per document — was considered and rejected:
//! [`RenderPool::recommended_workers`] already sizes itself for one machine, so two
//! pools would mean up to twice the worker threads competing for the same cores,
//! three pools three times, with nothing bounding it as documents accumulate over a
//! session. Instead there is one pool, sized once, and [`RenderPool::add_document`]
//! grows the list of documents it can be asked to rasterize from.
//!
//! The list only ever grows. A worker that reads it after [`RenderPool::submit`]
//! validated a document index is guaranteed to still find it there — nothing
//! removes an entry or renumbers one — so the read on the hot path never has to
//! guess.
//!
//! # Cancellation
//!
//! Queued work can be dropped wholesale with [`RenderPool::cancel_pending`],
//! which is what happens when the viewport moves and the previous request order
//! is obsolete. Work a thread has already *started* cannot be cancelled — Rust
//! has no way to interrupt a running thread — so up to one job per worker may
//! complete after being cancelled. Those results come back anyway and the caller
//! discards the ones it no longer wants, which costs a little wasted work bounded
//! by the worker count.
//!
//! # Why workers call [`render_with_timeout`]
//!
//! A hung render would otherwise consume its worker permanently, and enough of
//! them would starve the pool with no way to recover. Going through
//! [`render_with_timeout`] means a hang abandons one anonymous thread while the
//! worker itself returns to the queue and stays useful. That leaks a thread per
//! hang — the honest limitation described in `docs/goal-1-plan.md`, section 2,
//! whose real fix is process isolation — but it keeps the pool alive.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

use porpoise_doc::Document;

use crate::{RenderError, RenderRequest, RenderedPage, Renderer, render_with_timeout};

/// Upper bound on queued jobs.
///
/// A caller that tracks its own in-flight set will never reach this. It exists so
/// that a caller which does not cannot grow the queue without limit.
const MAX_QUEUED: usize = 64;

/// One unit of work.
#[derive(Debug, Clone, Copy)]
struct Job {
    /// Which document to rasterize from, an index into the pool's document list.
    document: usize,
    page_index: usize,
    scale: f32,
    tag: i64,
}

/// A finished render, carrying back enough context to match it to its request.
#[derive(Debug)]
pub struct RenderOutcome {
    /// Which document the page was rasterized from.
    pub document: usize,
    /// The page that was rasterized.
    pub page_index: usize,
    /// The scale it was rasterized at.
    pub scale: f32,
    /// Caller-defined marker passed through untouched.
    ///
    /// The viewer puts the zoom rung here, which lets it match a result to a
    /// cache key without this crate knowing anything about zoom bucketing.
    pub tag: i64,
    /// The rasterized page, or why it failed.
    pub result: Result<RenderedPage, RenderError>,
}

struct Shared {
    queue: Mutex<VecDeque<Job>>,
    ready: Condvar,
    shutdown: AtomicBool,
    active: AtomicUsize,
}

/// A pool of rasterizing workers.
///
/// Dropping the pool signals shutdown but does **not** wait for workers to
/// finish. A worker mid-render would otherwise delay process exit by up to the
/// job timeout, which is a poor trade for a viewer being closed.
pub struct RenderPool {
    shared: Arc<Shared>,
    /// Every document the pool can be asked to rasterize from, index 0 being the
    /// one it was constructed with. Shared with every worker; see the module docs
    /// for why appending to it is the only mutation it ever needs.
    documents: Arc<RwLock<Vec<Arc<Document>>>>,
    results: Receiver<RenderOutcome>,
    worker_count: usize,
}

impl RenderPool {
    /// Spawns `workers` threads rasterizing pages of `document`.
    ///
    /// `workers` is clamped to at least one. `job_timeout` bounds a single page,
    /// after which the render is abandoned and reported as
    /// [`RenderError::TimedOut`].
    pub fn new<R>(
        document: Arc<Document>,
        renderer: R,
        workers: usize,
        job_timeout: Duration,
    ) -> Self
    where
        R: Renderer + Clone + Send + 'static,
    {
        let worker_count = workers.max(1);
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            shutdown: AtomicBool::new(false),
            active: AtomicUsize::new(0),
        });
        let documents = Arc::new(RwLock::new(vec![document]));
        let (sender, results) = mpsc::channel();

        for index in 0..worker_count {
            let shared = Arc::clone(&shared);
            let documents = Arc::clone(&documents);
            let renderer = renderer.clone();
            let sender = sender.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("porpoise-render-{index}"))
                .spawn(move || worker_loop(&shared, &documents, &renderer, job_timeout, &sender));

            if let Err(error) = spawned {
                // One worker failing to spawn is survivable as long as another
                // exists; the queue is simply served more slowly.
                tracing::warn!(worker = index, %error, "could not spawn a render worker");
            }
        }

        Self {
            shared,
            documents,
            results,
            worker_count,
        }
    }

    /// A pool sized for this machine, leaving a core for the UI thread.
    ///
    /// Sized once regardless of how many documents the pool ends up serving — see
    /// the module docs for why that is the point rather than an oversight.
    #[must_use]
    pub fn recommended_workers() -> usize {
        std::thread::available_parallelism()
            .map(|cores| cores.get().saturating_sub(1).clamp(1, 4))
            .unwrap_or(1)
    }

    /// Registers another document the pool can be asked to rasterize from,
    /// returning the index [`Self::submit`] should use for it.
    ///
    /// The list only grows: nothing in this pool ever removes or renumbers an
    /// entry, which is what lets a worker trust an index once `submit` has
    /// validated it.
    pub fn add_document(&self, document: Arc<Document>) -> usize {
        let Ok(mut documents) = self.documents.write() else {
            // Poisoned means some other thread panicked while holding this lock,
            // which nothing in this module's own code does. Reporting index 0
            // would be a guess that could point a future submission at the wrong
            // document, so this is the one place a caller can be told the
            // document was not actually added — by the number never appearing in
            // a later `submit` that returns `true`. Recorded rather than
            // papered over; see `docs/goal-1-plan.md` §2 for this project's
            // stance on more of this kind of degradation.
            tracing::error!("render pool's document list is poisoned; refusing to add one");
            return usize::MAX;
        };
        documents.push(document);
        documents.len() - 1
    }

    /// Queues a page for rasterization. Never blocks.
    ///
    /// Returns `false` if `document` names no document this pool knows about, or
    /// if the queue was full and the job was refused.
    pub fn submit(&self, document: usize, page_index: usize, scale: f32, tag: i64) -> bool {
        let known = self
            .documents
            .read()
            .is_ok_and(|documents| document < documents.len());
        if !known {
            return false;
        }

        let Ok(mut queue) = self.shared.queue.lock() else {
            return false;
        };
        if queue.len() >= MAX_QUEUED {
            // Drop the least prioritized rather than the newest: callers submit in
            // priority order, so the back of the queue is the most speculative.
            queue.pop_back();
        }
        queue.push_back(Job {
            document,
            page_index,
            scale,
            tag,
        });
        drop(queue);
        self.shared.ready.notify_one();
        true
    }

    /// Discards every queued job, returning how many were dropped.
    ///
    /// Work already in progress is unaffected; see the module docs.
    pub fn cancel_pending(&self) -> usize {
        let Ok(mut queue) = self.shared.queue.lock() else {
            return 0;
        };
        let dropped = queue.len();
        queue.clear();
        dropped
    }

    /// Takes one finished render, if any is waiting.
    pub fn try_recv(&self) -> Option<RenderOutcome> {
        self.results.try_recv().ok()
    }

    /// Jobs waiting to start.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.shared.queue.lock().map_or(0, |queue| queue.len())
    }

    /// Jobs currently being rasterized.
    #[must_use]
    pub fn active(&self) -> usize {
        self.shared.active.load(Ordering::Acquire)
    }

    /// Whether any work is queued or running.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.active() > 0 || self.queued() > 0
    }

    /// Number of worker threads requested.
    #[must_use]
    pub fn worker_count(&self) -> usize {
        self.worker_count
    }
}

impl Drop for RenderPool {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.ready.notify_all();
    }
}

/// Blocks until a job is available, or returns `None` once shutting down.
fn next_job(shared: &Shared) -> Option<Job> {
    let mut queue = shared.queue.lock().ok()?;
    loop {
        if shared.shutdown.load(Ordering::Acquire) {
            return None;
        }
        if let Some(job) = queue.pop_front() {
            return Some(job);
        }
        queue = shared.ready.wait(queue).ok()?;
    }
}

fn worker_loop<R>(
    shared: &Shared,
    documents: &Arc<RwLock<Vec<Arc<Document>>>>,
    renderer: &R,
    job_timeout: Duration,
    results: &Sender<RenderOutcome>,
) where
    R: Renderer + Clone + Send + 'static,
{
    while let Some(job) = next_job(shared) {
        // The list only grows, and `submit` already checked this index against it
        // — so `None` here means the list was poisoned between then and now, not
        // an ordinary race. Dropping the job silently is still the right call: a
        // worker must never guess which document a page came from.
        let document = documents
            .read()
            .ok()
            .and_then(|documents| documents.get(job.document).cloned());
        let Some(document) = document else {
            tracing::error!(document = job.document, "render job named an unknown document");
            continue;
        };

        shared.active.fetch_add(1, Ordering::Release);

        let result = render_with_timeout(
            renderer.clone(),
            document,
            RenderRequest {
                page_index: job.page_index,
                scale: job.scale,
            },
            job_timeout,
        );

        shared.active.fetch_sub(1, Ordering::Release);

        let outcome = RenderOutcome {
            document: job.document,
            page_index: job.page_index,
            scale: job.scale,
            tag: job.tag,
            result,
        };

        // A send error means the pool was dropped and nobody is listening.
        if results.send(outcome).is_err() {
            return;
        }
    }
}
