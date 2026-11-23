//! The write queue of one shard (spec `write-path.md`, section 7).
//!
//! Commands wait here in first-in first-out order until one of the writer tasks (one per recipe
//! connection) takes them. A new command is refused before it is queued when
//! [`Admission`](crate::admission::Admission) says so; a refused command did no work and has no
//! record. A command that was admitted and then waited longer than four times the limit is dropped
//! at the front with the same reply. The commit time of every finished command feeds the
//! commit-latency watch.
//!
//! The queue is generic over a [`CommandRunner`] so its rules run in tests on a fake runner and a
//! manual clock; the real runner is the [`Executor`].

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bhdata_api::shard::{Command, Outcome, ShardBackend};
use bhdata_api::types::config::WriterConfig;
use bhdata_api::types::{Clock, Error};
use bhdata_pg::errors::OVERLOAD_RETRY_AFTER;
use tokio::sync::{oneshot, Notify};

use crate::admission::{Admission, CommitLatency, QueueState, Verdict};
use crate::exec::{Executor, Finished};

/// Whatever runs one command to its end. The [`Executor`] is the real one.
pub trait CommandRunner: Send + Sync + 'static {
    /// Runs one command; an `Err` stored nothing.
    fn run(&self, cmd: &Command) -> impl Future<Output = Result<Finished, Error>> + Send;
}

impl CommandRunner for Executor {
    async fn run(&self, cmd: &Command) -> Result<Finished, Error> {
        Executor::run(self, cmd).await
    }
}

/// What `bhdatactl status` shows about the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueStatus {
    /// Commands waiting.
    pub len: usize,
    /// How long the oldest has waited.
    pub oldest_wait: Option<Duration>,
    /// Commands being run right now.
    pub running: usize,
    /// The 99th-percentile commit time of the last 10 seconds, if any command committed.
    pub commit_p99: Option<Duration>,
    /// Whether the commit-latency alarm is on.
    pub alarm: bool,
    /// Whether the queue has been shut down.
    pub closed: bool,
}

struct Queued {
    cmd: Command,
    queued_at: Duration,
    reply: oneshot::Sender<Result<Outcome, Error>>,
}

struct State {
    items: VecDeque<Queued>,
    latency: CommitLatency,
    closed: bool,
    running: usize,
}

impl State {
    fn queue_state(&self, now: Duration) -> QueueState {
        QueueState {
            len: self.items.len(),
            oldest_wait: self.items.front().map(|q| now.saturating_sub(q.queued_at)),
        }
    }
}

enum Take {
    Item(Queued),
    Empty,
    Stop,
}

struct Inner<R> {
    runner: R,
    clock: Arc<dyn Clock>,
    admission: Admission,
    state: Mutex<State>,
    /// Wakes one idle writer; a stored permit covers a push that lands just before a writer waits.
    wake: Notify,
    /// Wakes whoever waits for the running commands to finish.
    idle: Notify,
}

impl<R: CommandRunner> Inner<R> {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn take(&self) -> Take {
        let mut st = self.lock();
        match st.items.pop_front() {
            Some(item) => {
                st.running += 1;
                if !st.items.is_empty() {
                    // More is waiting: wake another writer so the queue does not drain one by one.
                    self.wake.notify_one();
                }
                Take::Item(item)
            }
            None if st.closed => Take::Stop,
            None => Take::Empty,
        }
    }

    async fn process(&self, q: Queued) {
        let _running = RunGuard { inner: self };
        let now = self.clock.monotonic();
        let waited = now.saturating_sub(q.queued_at);
        let result = if q.reply.is_closed() {
            // Nobody is waiting any more. Nothing was stored, and a retry settles it by its id.
            return;
        } else if self.admission.drop_waiting(waited) {
            let state = self.lock().queue_state(now);
            Err(Error::Overloaded {
                retry_after: self.admission.hint(state),
            })
        } else {
            match self.runner.run(&q.cmd).await {
                Ok(finished) => {
                    if let Some(took) = finished.commit_time {
                        let at = self.clock.monotonic();
                        self.lock().latency.record(at, took);
                    }
                    Ok(finished.outcome)
                }
                Err(e) => Err(e),
            }
        };
        let _ = q.reply.send(result);
    }
}

/// Counts a command as running until it is done, even if the runner panics.
struct RunGuard<'a, R: CommandRunner> {
    inner: &'a Inner<R>,
}

impl<R: CommandRunner> Drop for RunGuard<'_, R> {
    fn drop(&mut self) {
        let mut st = self.inner.lock();
        st.running = st.running.saturating_sub(1);
        if st.running == 0 {
            self.inner.idle.notify_waiters();
        }
    }
}

async fn writer<R: CommandRunner>(inner: Arc<Inner<R>>) {
    loop {
        match inner.take() {
            Take::Item(q) => inner.process(q).await,
            Take::Empty => inner.wake.notified().await,
            Take::Stop => {
                // Pass the wake-up on, so a writer that was about to wait does not sleep forever.
                inner.wake.notify_one();
                return;
            }
        }
    }
}

/// The queue and its writers. Cheap to clone; all clones are the same queue.
pub struct WriteQueue<R: CommandRunner> {
    inner: Arc<Inner<R>>,
}

impl<R: CommandRunner> Clone for WriteQueue<R> {
    fn clone(&self) -> Self {
        WriteQueue {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<R: CommandRunner> WriteQueue<R> {
    /// Builds the queue and starts `writers` tasks on the current tokio runtime (at least one).
    /// `writers` is the size of the recipe pool.
    #[must_use]
    pub fn start(
        runner: R,
        clock: Arc<dyn Clock>,
        cfg: &WriterConfig,
        writers: usize,
    ) -> WriteQueue<R> {
        let inner = Arc::new(Inner {
            runner,
            clock,
            admission: Admission {
                capacity: cfg.queue_capacity as usize,
                max_wait: cfg.max_queue_wait(),
            },
            state: Mutex::new(State {
                items: VecDeque::new(),
                latency: CommitLatency::new(Duration::from_millis(cfg.commit_latency_alarm_ms)),
                closed: false,
                running: 0,
            }),
            wake: Notify::new(),
            idle: Notify::new(),
        });
        for _ in 0..writers.max(1) {
            tokio::spawn(writer(Arc::clone(&inner)));
        }
        WriteQueue { inner }
    }

    /// A snapshot for `bhdatactl status`.
    #[must_use]
    pub fn status(&self) -> QueueStatus {
        let now = self.inner.clock.monotonic();
        let mut st = self.inner.lock();
        let q = st.queue_state(now);
        QueueStatus {
            len: q.len,
            oldest_wait: q.oldest_wait,
            running: st.running,
            commit_p99: st.latency.p99(now),
            alarm: st.latency.alarmed(now),
            closed: st.closed,
        }
    }

    /// Stops admitting (new commands get `overloaded`), answers the commands still waiting with
    /// `overloaded` too, and gives the running ones up to `drain` to finish. A command that did not
    /// finish was never committed, so its client retries safely.
    pub async fn shutdown(&self, drain: Duration) {
        let waiting: Vec<Queued> = {
            let mut st = self.inner.lock();
            st.closed = true;
            st.items.drain(..).collect()
        };
        for q in waiting {
            let _ = q.reply.send(Err(Error::Overloaded {
                retry_after: OVERLOAD_RETRY_AFTER,
            }));
        }
        self.inner.wake.notify_one();
        let _ = tokio::time::timeout(drain, async {
            loop {
                let changed = self.inner.idle.notified();
                if self.inner.lock().running == 0 {
                    return;
                }
                changed.await;
            }
        })
        .await;
    }
}

impl<R: CommandRunner> ShardBackend for WriteQueue<R> {
    async fn execute(&self, command: Command) -> Result<Outcome, Error> {
        let (reply, answer) = oneshot::channel();
        {
            let mut st = self.inner.lock();
            let now = self.inner.clock.monotonic();
            if st.closed {
                return Err(Error::Overloaded {
                    retry_after: OVERLOAD_RETRY_AFTER,
                });
            }
            let queue = st.queue_state(now);
            let alarm = st.latency.alarmed(now);
            match self.inner.admission.decide(queue, alarm) {
                Verdict::Reject { retry_after } => return Err(Error::Overloaded { retry_after }),
                Verdict::Admit => st.items.push_back(Queued {
                    cmd: command,
                    queued_at: now,
                    reply,
                }),
            }
        }
        self.inner.wake.notify_one();
        // The only way the reply is lost is a writer that died; the command may or may not have
        // run, and the retry settles it through the request id.
        answer.await.unwrap_or(Err(Error::Overloaded {
            retry_after: OVERLOAD_RETRY_AFTER,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use bhdata_api::recipe::eval::Value;
    use bhdata_api::recipe::{compile, parse_recipe, Limits, Plan};
    use bhdata_api::shard::{Retention, Status};
    use bhdata_api::types::{LoginId, ManualClock, RecipeVersion, RequestId, SubjectId, Timestamp};
    use bhdata_schema::parse_schema;

    use super::*;

    fn data(file: &str) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../bhdata-recipe/tests/data")
            .join(file);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    fn spend_plan() -> Arc<Plan> {
        let schema = parse_schema(&data("schema.toml")).expect("schema");
        let recipe = parse_recipe(&data("spend.toml"), &Limits::default()).expect("recipe");
        Arc::new(compile(&schema, &recipe, &[], &Limits::default()).expect("compiles"))
    }

    fn cmd(plan: &Arc<Plan>, request: &str) -> Command {
        Command {
            login_id: LoginId::new("l").expect("login id"),
            request_id: RequestId::new(request).expect("request id"),
            requested_at: Timestamp::from_unix_millis(1_700_000_000_000),
            plan: plan.clone(),
            version: RecipeVersion::new(1),
            version_named: false,
            subject: SubjectId::new("u1").expect("subject id"),
            counterparty: None,
            args: BTreeMap::from([("amount".to_string(), Value::I64(1))]),
            retention: Retention::Window,
        }
    }

    /// A runner that answers with the request id, and can be held shut until opened.
    struct Fake {
        gate: tokio::sync::Semaphore,
        started: AtomicUsize,
        commit: Option<Duration>,
    }

    impl Fake {
        fn open() -> Fake {
            Fake {
                gate: tokio::sync::Semaphore::new(usize::MAX >> 4),
                started: AtomicUsize::new(0),
                commit: None,
            }
        }

        fn shut() -> Fake {
            Fake {
                gate: tokio::sync::Semaphore::new(0),
                started: AtomicUsize::new(0),
                commit: None,
            }
        }
    }

    impl CommandRunner for Arc<Fake> {
        async fn run(&self, cmd: &Command) -> Result<Finished, Error> {
            self.started.fetch_add(1, Ordering::SeqCst);
            let permit = self.gate.acquire().await.expect("gate");
            permit.forget();
            Ok(Finished {
                outcome: Outcome {
                    status: Status::Done,
                    result: cmd.request_id.to_string(),
                    replayed: false,
                },
                commit_time: self.commit,
            })
        }
    }

    fn cfg(capacity: u32) -> WriterConfig {
        WriterConfig {
            queue_capacity: capacity,
            max_queue_wait_ms: 500,
            ..WriterConfig::default()
        }
    }

    fn clock() -> Arc<ManualClock> {
        Arc::new(ManualClock::new(Timestamp::from_unix_millis(
            1_700_000_000_000,
        )))
    }

    async fn until(mut ok: impl FnMut() -> bool) {
        for _ in 0..500 {
            if ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("condition not reached");
    }

    #[tokio::test]
    async fn commands_are_run_and_each_gets_its_own_outcome() {
        let plan = spend_plan();
        let q = WriteQueue::start(Arc::new(Fake::open()), clock(), &cfg(8), 4);
        let mut tasks = Vec::new();
        for i in 0..20 {
            let (q, plan) = (q.clone(), plan.clone());
            tasks.push(tokio::spawn(async move {
                let out = q.execute(cmd(&plan, &format!("r{i}"))).await;
                (i, out)
            }));
        }
        for t in tasks {
            let (i, out) = t.await.expect("task");
            // The queue holds 8, so some may be refused under a burst; whatever ran is right.
            match out {
                Ok(o) => assert_eq!(o.result, format!("r{i}")),
                Err(e) => assert!(matches!(e, Error::Overloaded { .. }), "{e}"),
            }
        }
    }

    #[tokio::test]
    async fn a_full_queue_refuses_and_the_hint_names_the_overload() {
        let plan = spend_plan();
        let fake = Arc::new(Fake::shut());
        let q = WriteQueue::start(fake.clone(), clock(), &cfg(2), 1);
        // One command is taken by the writer and held at the gate; two more fill the queue.
        let mut held = Vec::new();
        for i in 0..3 {
            let (q, plan) = (q.clone(), plan.clone());
            held.push(tokio::spawn(async move {
                q.execute(cmd(&plan, &format!("h{i}"))).await
            }));
            if i == 0 {
                until(|| fake.started.load(Ordering::SeqCst) == 1).await;
            }
        }
        until(|| q.status().len == 2).await;
        let refused = q.execute(cmd(&plan, "late")).await;
        assert!(
            matches!(refused, Err(Error::Overloaded { retry_after }) if retry_after >= OVERLOAD_RETRY_AFTER)
        );
        fake.gate.add_permits(10);
        for h in held {
            assert!(h.await.expect("task").is_ok());
        }
    }

    #[tokio::test]
    async fn a_queue_that_is_not_moving_refuses_even_when_half_empty() {
        let plan = spend_plan();
        let fake = Arc::new(Fake::shut());
        let clk = clock();
        let q = WriteQueue::start(fake.clone(), clk.clone(), &cfg(100), 1);
        let mut held = Vec::new();
        for i in 0..2 {
            let (q, plan) = (q.clone(), plan.clone());
            held.push(tokio::spawn(async move {
                q.execute(cmd(&plan, &format!("h{i}"))).await
            }));
            if i == 0 {
                until(|| fake.started.load(Ordering::SeqCst) == 1).await;
            }
        }
        until(|| q.status().len == 1).await;
        clk.advance(Duration::from_millis(600));
        let refused = q.execute(cmd(&plan, "late")).await;
        assert!(matches!(refused, Err(Error::Overloaded { .. })));
        fake.gate.add_permits(10);
        for h in held {
            // The one that waited 600 ms is under the 2000 ms drop limit, so it still runs.
            assert!(h.await.expect("task").is_ok());
        }
    }

    #[tokio::test]
    async fn a_command_that_waited_four_times_the_limit_is_dropped_at_the_front() {
        let plan = spend_plan();
        let fake = Arc::new(Fake::shut());
        let clk = clock();
        let q = WriteQueue::start(fake.clone(), clk.clone(), &cfg(100), 1);
        let first = {
            let (q, plan) = (q.clone(), plan.clone());
            tokio::spawn(async move { q.execute(cmd(&plan, "first")).await })
        };
        until(|| fake.started.load(Ordering::SeqCst) == 1).await;
        let second = {
            let (q, plan) = (q.clone(), plan.clone());
            tokio::spawn(async move { q.execute(cmd(&plan, "second")).await })
        };
        until(|| q.status().len == 1).await;
        clk.advance(Duration::from_millis(2_500));
        fake.gate.add_permits(10);
        assert!(first.await.expect("task").is_ok());
        assert!(matches!(
            second.await.expect("task"),
            Err(Error::Overloaded { .. })
        ));
        assert_eq!(fake.started.load(Ordering::SeqCst), 1, "never ran");
    }

    #[tokio::test]
    async fn slow_commits_raise_the_alarm_and_it_clears_as_they_age_out() {
        let plan = spend_plan();
        let fake = Arc::new(Fake {
            commit: Some(Duration::from_millis(400)),
            ..Fake::open()
        });
        let clk = clock();
        let q = WriteQueue::start(fake, clk.clone(), &cfg(8), 2);
        assert!(q.execute(cmd(&plan, "a")).await.is_ok());
        let s = q.status();
        assert!(s.alarm);
        assert_eq!(s.commit_p99, Some(Duration::from_millis(400)));
        clk.advance(Duration::from_secs(11));
        let s = q.status();
        assert!(!s.alarm);
        assert_eq!(s.commit_p99, None);
    }

    #[tokio::test]
    async fn shutdown_refuses_the_waiting_and_the_new_and_lets_the_running_finish() {
        let plan = spend_plan();
        let fake = Arc::new(Fake::shut());
        let q = WriteQueue::start(fake.clone(), clock(), &cfg(8), 1);
        let running = {
            let (q, plan) = (q.clone(), plan.clone());
            tokio::spawn(async move { q.execute(cmd(&plan, "running")).await })
        };
        until(|| fake.started.load(Ordering::SeqCst) == 1).await;
        let waiting = {
            let (q, plan) = (q.clone(), plan.clone());
            tokio::spawn(async move { q.execute(cmd(&plan, "waiting")).await })
        };
        until(|| q.status().len == 1).await;
        let stopper = {
            let q = q.clone();
            tokio::spawn(async move { q.shutdown(Duration::from_secs(5)).await })
        };
        assert!(matches!(
            waiting.await.expect("task"),
            Err(Error::Overloaded { .. })
        ));
        assert!(matches!(
            q.execute(cmd(&plan, "new")).await,
            Err(Error::Overloaded { .. })
        ));
        fake.gate.add_permits(1);
        assert!(running.await.expect("task").is_ok(), "finished");
        stopper.await.expect("shutdown returns once idle");
        assert!(q.status().closed);
    }

    #[tokio::test]
    async fn shutdown_gives_up_after_the_drain_time() {
        let plan = spend_plan();
        let fake = Arc::new(Fake::shut());
        let q = WriteQueue::start(fake.clone(), clock(), &cfg(8), 1);
        let stuck = {
            let (q, plan) = (q.clone(), plan.clone());
            tokio::spawn(async move { q.execute(cmd(&plan, "stuck")).await })
        };
        until(|| fake.started.load(Ordering::SeqCst) == 1).await;
        q.shutdown(Duration::from_millis(50)).await;
        assert_eq!(q.status().running, 1, "left running, never committed");
        fake.gate.add_permits(1);
        let _ = stuck.await;
    }
}
