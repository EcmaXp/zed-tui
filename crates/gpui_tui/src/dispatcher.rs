use std::{sync::mpsc, thread, time::Duration};

use gpui::{PlatformDispatcher, Priority, RunnableVariant, profiler};
use gpui_util::ResultExt as _;

const MIN_THREADS: usize = 2;

pub(crate) struct TuiDispatcher {
    main_sender: mpsc::Sender<RunnableVariant>,
    background: flume::Sender<RunnableVariant>,
    main_thread_id: thread::ThreadId,
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

        Self {
            main_sender,
            background,
            main_thread_id: thread::current().id(),
        }
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
        thread::spawn(move || {
            thread::sleep(duration);
            run_runnable(runnable);
        });
    }

    fn spawn_realtime(&self, function: Box<dyn FnOnce() + Send>) {
        thread::spawn(function);
    }
}
