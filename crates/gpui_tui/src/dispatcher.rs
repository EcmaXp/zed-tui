use std::{
    collections::BinaryHeap,
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc},
    thread,
    time::{Duration, Instant},
};

use gpui::{PlatformDispatcher, Priority, RunnableVariant, profiler};
use gpui_util::{ResultExt as _, post_inc};

const MIN_THREADS: usize = 2;

pub(crate) struct TuiDispatcher {
    main_sender: mpsc::Sender<RunnableVariant>,
    background: flume::Sender<RunnableVariant>,
    timers: Arc<TimerQueue<RunnableVariant>>,
    main_thread_id: thread::ThreadId,
}

struct TimerEntry<T> {
    due: Instant,
    sequence: u64,
    payload: T,
}

impl<T> PartialEq for TimerEntry<T> {
    fn eq(&self, other: &Self) -> bool {
        self.due == other.due && self.sequence == other.sequence
    }
}

impl<T> Eq for TimerEntry<T> {}

impl<T> PartialOrd for TimerEntry<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for TimerEntry<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (other.due, other.sequence).cmp(&(self.due, self.sequence))
    }
}

struct TimerQueueState<T> {
    heap: BinaryHeap<TimerEntry<T>>,
    next_sequence: u64,
}

struct TimerQueue<T> {
    state: Mutex<TimerQueueState<T>>,
    condvar: Condvar,
}

impl<T> TimerQueue<T> {
    fn new() -> Self {
        Self {
            state: Mutex::new(TimerQueueState {
                heap: BinaryHeap::new(),
                next_sequence: 0,
            }),
            condvar: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, TimerQueueState<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, duration: Duration, payload: T) {
        self.push_at(Instant::now() + duration, payload);
    }

    fn push_at(&self, due: Instant, payload: T) {
        let mut state = self.lock();
        let sequence = post_inc(&mut state.next_sequence);
        state.heap.push(TimerEntry {
            due,
            sequence,
            payload,
        });
        let is_earliest = state
            .heap
            .peek()
            .is_some_and(|entry| entry.sequence == sequence);
        drop(state);
        if is_earliest {
            self.condvar.notify_one();
        }
    }

    fn pop_due(&self) -> T {
        let mut state = self.lock();
        loop {
            let now = Instant::now();
            match state.heap.peek().map(|entry| entry.due) {
                Some(due) if due <= now => {
                    if let Some(entry) = state.heap.pop() {
                        return entry.payload;
                    }
                }
                Some(due) => {
                    state = match self.condvar.wait_timeout(state, due - now) {
                        Ok((state, _)) => state,
                        Err(poisoned) => poisoned.into_inner().0,
                    };
                }
                None => {
                    state = self
                        .condvar
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
        }
    }
}

pub(crate) fn run_runnable(runnable: RunnableVariant) {
    let location = runnable.metadata().location;
    let spawned = runnable.metadata().spawned;
    profiler::update_running_task(spawned, location);
    runnable.run();
    profiler::save_task_timing();
}

impl TuiDispatcher {
    pub(crate) fn new(main_sender: mpsc::Sender<RunnableVariant>) -> Self {
        let (background, receiver) = flume::unbounded();
        let thread_count = thread::available_parallelism()
            .map_or(MIN_THREADS, |count| count.get().max(MIN_THREADS));

        for index in 0..thread_count {
            let receiver = receiver.clone();
            thread::Builder::new()
                .name(format!("Worker-{index}"))
                .spawn(move || receiver.iter().for_each(run_runnable))
                .log_err();
        }

        let timers = Arc::new(TimerQueue::new());
        thread::Builder::new()
            .name("Timer".to_owned())
            .spawn({
                let timers = timers.clone();
                move || run_timers(&timers)
            })
            .log_err();

        Self {
            main_sender,
            background,
            timers,
            main_thread_id: thread::current().id(),
        }
    }
}

fn run_timers(timers: &TimerQueue<RunnableVariant>) {
    loop {
        run_runnable(timers.pop_due());
    }
}

impl PlatformDispatcher for TuiDispatcher {
    fn is_main_thread(&self) -> bool {
        thread::current().id() == self.main_thread_id
    }

    fn dispatch(&self, runnable: RunnableVariant, _priority: Priority) {
        if let Err(error) = self.background.send(runnable) {
            log::debug!("dropped background work after shutdown: {error:?}");
        }
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, _priority: Priority) {
        if let Err(mpsc::SendError(runnable)) = self.main_sender.send(runnable) {
            std::mem::forget(runnable);
        }
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        self.timers.push(duration, runnable);
    }

    fn spawn_realtime(&self, function: Box<dyn FnOnce() + Send>) {
        thread::spawn(function);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timers_fire_in_deadline_order_and_never_early() {
        let queue = TimerQueue::new();
        let started = Instant::now();
        let durations = [
            (1, Duration::from_millis(30)),
            (2, Duration::from_millis(10)),
            (3, Duration::from_millis(20)),
        ];
        for (payload, duration) in durations {
            queue.push(duration, payload);
        }

        let mut fired = Vec::new();
        for _ in 0..durations.len() {
            let payload = queue.pop_due();
            fired.push((payload, started.elapsed()));
        }

        assert_eq!(
            fired
                .iter()
                .map(|(payload, _)| *payload)
                .collect::<Vec<_>>(),
            [2, 3, 1]
        );
        for (payload, elapsed) in fired {
            let (_, duration) = durations
                .iter()
                .find(|(candidate, _)| *candidate == payload)
                .expect("every fired payload was pushed");
            assert!(
                elapsed >= *duration,
                "timer {payload} fired after {elapsed:?}, before its {duration:?} deadline"
            );
        }
    }

    #[test]
    fn an_earlier_timer_wakes_a_thread_waiting_on_a_later_one() {
        let queue = Arc::new(TimerQueue::new());
        queue.push(Duration::from_secs(5), "late");
        let pusher = thread::spawn({
            let queue = queue.clone();
            move || {
                thread::sleep(Duration::from_millis(20));
                queue.push(Duration::from_millis(30), "early");
            }
        });

        let started = Instant::now();
        let payload = queue.pop_due();
        let elapsed = started.elapsed();
        pusher.join().expect("pusher thread panicked");

        assert_eq!(payload, "early");
        assert!(
            elapsed < Duration::from_secs(2),
            "the earlier timer fired only after {elapsed:?}"
        );
    }

    #[test]
    fn timers_with_the_same_deadline_fire_in_push_order() {
        let queue = TimerQueue::new();
        let due = Instant::now() + Duration::from_millis(5);
        for payload in 0..20 {
            queue.push_at(due, payload);
        }

        let fired = (0..20).map(|_| queue.pop_due()).collect::<Vec<_>>();

        assert_eq!(fired, (0..20).collect::<Vec<_>>());
    }
}
