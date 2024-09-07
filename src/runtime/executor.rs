use std::collections::{BTreeSet, VecDeque};
use std::future::Future;
use std::intrinsics::unlikely;
use std::mem;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use crossbeam::utils::CachePadded;

use crate::atomic_task_queue::AtomicTaskList;
use crate::io::worker::{
    init_local_worker, IoWorker, local_worker as local_io_worker, LOCAL_WORKER,
};
use crate::runtime::call::Call;
use crate::runtime::config::{Config, ValidConfig};
use crate::runtime::{end_worker_by_id, get_core_id_for_executor, init_new_worker_end, is_ended_by_id};
use crate::runtime::task::Task;
use crate::runtime::task_pool::TaskPool;
use crate::runtime::waker::create_waker;
use crate::sleep::sleeping_task::SleepingTask;
use crate::utils::CoreId;

#[thread_local]
pub static mut LOCAL_EXECUTOR: Option<Executor> = None;

fn uninit_local_executor() {
    unsafe { LOCAL_EXECUTOR = None }
}

// TODO
pub(crate) const MSG_LOCAL_EXECUTOR_IS_NOT_INIT: &str = "\
    ------------------------------------------------------------------------------------------\n\
    |    Local executor is not initialized.                                                  |\n\
    |    Please initialize it first.                                                         |\n\
    |                                                                                        |\n\
    |    1 - use let executor = Executor::init();                                            |\n\
    |    2 - use executor.spawn_local(your_future)                                           |\n\
    |            or executor.spawn_global(your_future)                                       |\n\
    |                                                                                        |\n\
    |    ATTENTION: if you want the future to finish the local runtime,                      |\n\
    |               add orengine::end_local_thread() in the end of the future,               |\n\
    |               otherwise the local runtime will never be stopped.                       |\n\
    |                                                                                        |\n\
    |    3 - use executor.run()                                                              |\n\
    ------------------------------------------------------------------------------------------";

#[inline(always)]
pub fn local_executor() -> &'static mut Executor {
    unsafe {
        LOCAL_EXECUTOR
            .as_mut()
            .expect(MSG_LOCAL_EXECUTOR_IS_NOT_INIT)
    }
}

#[inline(always)]
pub unsafe fn local_executor_unchecked() -> &'static mut Executor {
    unsafe { LOCAL_EXECUTOR.as_mut().unwrap_unchecked() }
}

pub struct Executor {
    core_id: CoreId,
    worker_id: usize,

    config: ValidConfig,

    current_call: Call,

    exec_series: usize,
    tasks: VecDeque<Task>,
    sleeping_tasks: BTreeSet<SleepingTask>,
}

pub(crate) static FREE_WORKER_ID: AtomicUsize = AtomicUsize::new(0);

impl Executor {
    pub fn init_on_core_with_config(core_id: CoreId, config: Config) -> &'static mut Executor {
        let valid_config = config.validate();
        crate::utils::core::set_for_current(core_id);
        let worker_id = FREE_WORKER_ID.fetch_add(1, Ordering::Relaxed);
        init_new_worker_end(worker_id);
        TaskPool::init();

        unsafe {
            if let Some(io_config) = valid_config.io_worker_config {
                init_local_worker(io_config);
            }

            LOCAL_EXECUTOR = Some(Executor {
                config: valid_config,
                core_id,
                worker_id,
                current_call: Call::default(),
                tasks: VecDeque::new(),
                exec_series: 0,
                sleeping_tasks: BTreeSet::new(),
            });
            local_executor_unchecked()
        }
    }

    pub fn init_on_core(core_id: CoreId) -> &'static mut Executor {
        Self::init_on_core_with_config(core_id, Config::default())
    }

    pub fn init_with_config(config: Config) -> &'static mut Executor {
        Self::init_on_core_with_config(get_core_id_for_executor(), config)
    }

    pub fn init() -> &'static mut Executor {
        Self::init_on_core(get_core_id_for_executor())
    }

    pub fn worker_id(&self) -> usize {
        self.worker_id
    }

    pub fn core_id(&self) -> CoreId {
        self.core_id
    }

    pub fn config(&self) -> Config {
        Config::from(&self.config)
    }

    pub(crate) fn set_config_buffer_len(&mut self, buffer_len: usize) {
        self.config.buffer_len = buffer_len;
    }

    #[inline(always)]
    /// # Safety
    ///
    /// * send_to must be a valid pointer to [`AtomicTaskQueue`](AtomicTaskList)
    ///
    /// * the reference must live at least as long as this state of the task
    ///
    /// * task must return [`Poll::Pending`](Poll::Pending) immediately after calling this function
    pub unsafe fn push_current_task_to(&mut self, send_to: &AtomicTaskList) {
        debug_assert!(self.current_call.is_none());
        self.current_call = Call::PushCurrentTaskTo(send_to);
    }

    #[inline(always)]
    /// # Safety
    ///
    /// * send_to must be a valid pointer to [`AtomicTaskQueue`](AtomicTaskList)
    ///
    /// * task must return [`Poll::Pending`](Poll::Pending) immediately after calling this function
    ///
    /// * counter must be a valid pointer to [`AtomicUsize`](AtomicUsize)
    ///
    /// * the references must live at least as long as this state of the task
    pub unsafe fn push_current_task_to_and_remove_it_if_counter_is_zero(
        &mut self,
        send_to: &AtomicTaskList,
        counter: &AtomicUsize,
        order: Ordering,
    ) {
        debug_assert!(self.current_call.is_none());
        self.current_call =
            Call::PushCurrentTaskToAndRemoveItIfCounterIsZero(send_to, counter, order);
    }

    #[inline(always)]
    pub unsafe fn release_atomic_bool(&mut self, atomic_bool: *const CachePadded<AtomicBool>) {
        debug_assert!(self.current_call.is_none());
        self.current_call = Call::ReleaseAtomicBool(atomic_bool);
    }

    #[inline(always)]
    pub fn exec_task(&mut self, mut task: Task) {
        self.exec_series += 1;
        if unlikely(self.exec_series == 107) {
            self.exec_series = 0;
            self.spawn_local_task(task);
            return;
        }
        let task_ref = &mut task;
        let task_ptr = task_ref as *mut Task;
        let future = unsafe { &mut *task_ref.future_ptr };
        let waker = create_waker(task_ptr as *const ());
        let mut context = Context::from_waker(&waker);

        match unsafe { Pin::new_unchecked(future) }
            .as_mut()
            .poll(&mut context)
        {
            Poll::Ready(()) => {
                debug_assert_eq!(self.current_call, Call::None);
                unsafe { task.drop_future() };
            }
            Poll::Pending => {
                match mem::take(&mut self.current_call) {
                    Call::None => {}
                    Call::PushCurrentTaskTo(task_list) => unsafe { (&*task_list).push(task) },
                    Call::PushCurrentTaskToAndRemoveItIfCounterIsZero(
                        task_list,
                        counter,
                        order,
                    ) => {
                        unsafe {
                            let list = &*task_list;
                            list.push(task);
                            let counter = &*counter;

                            if counter.load(order) == 0 {
                                if let Some(task) = list.pop() {
                                    self.exec_task(task);
                                } // else other thread already executed the task
                            }
                        }
                    }
                    Call::ReleaseAtomicBool(atomic_ptr) => {
                        let atomic_ref = unsafe { &*atomic_ptr };
                        atomic_ref.store(false, Ordering::Release);
                    }
                }
            }
        }
    }

    #[inline(always)]
    pub fn exec_future<F>(&mut self, future: F)
    where
        F: Future<Output = ()>,
    {
        let task = Task::from_future(future);
        self.exec_task(task);
    }

    #[inline(always)]
    pub fn spawn_local<F>(&mut self, future: F)
    where
        F: Future<Output = ()>,
    {
        let task = Task::from_future(future);
        self.spawn_local_task(task);
    }

    #[inline(always)]
    pub fn spawn_local_task(&mut self, task: Task) {
        self.tasks.push_back(task);
    }

    #[inline(always)]
    // TODO r #[allow(unused)]
    #[allow(unused)]
    pub fn spawn_global<F>(&mut self, future: F)
    where
        F: Future<Output = ()> + Send,
    {
        todo!()
    }

    #[inline(always)]
    // TODO r #[allow(unused)]
    #[allow(unused)]
    pub fn spawn_global_task(&mut self, task: Task) {
        todo!()
    }

    #[inline(always)]
    pub fn local_queue(&mut self) -> &mut VecDeque<Task> {
        &mut self.tasks
    }

    #[inline(always)]
    pub fn sleeping_tasks(&mut self) -> &mut BTreeSet<SleepingTask> {
        &mut self.sleeping_tasks
    }

    #[inline(always)]
    /// Return true, if we need to stop ([`end_local_thread`](end_local_thread)
    /// was called or [`end`](crate::runtime::end::end)).
    fn background_task<W: IoWorker>(&mut self, io_worker: &mut W) -> bool {
        if unlikely(is_ended_by_id(self.worker_id())) {
            return true;
        }

        self.exec_series = 0;

        let has_no_work = io_worker.must_poll(Duration::ZERO);

        let instant = Instant::now();
        while let Some(sleeping_task) = self.sleeping_tasks.pop_first() {
            if sleeping_task.time_to_wake() <= instant {
                self.exec_task(sleeping_task.task());
            } else {
                let need_to_sleep = sleeping_task.time_to_wake() - instant;
                self.sleeping_tasks.insert(sleeping_task);
                if unlikely(has_no_work) {
                    const MAX_SLEEP: Duration = Duration::from_millis(10);

                    if need_to_sleep > MAX_SLEEP {
                        let _ = io_worker.must_poll(MAX_SLEEP);
                        break;
                    } else {
                        let _ = io_worker.must_poll(need_to_sleep);
                    }
                } else {
                    break;
                }
            }
        }

        if unlikely(self.tasks.capacity() > 512 && self.tasks.len() * 3 < self.tasks.capacity()) {
            self.tasks.shrink_to(self.tasks.len() * 2 + 1);
        }

        false
    }

    pub fn run(&mut self) {
        let mut task;
        // A round is a number of tasks that must be completed before the next background_work call.
        // It is needed to avoid case like:
        //   Task with yield -> repeat this task -> repeat this task -> ...
        //
        // So it works like:
        //   Round 1 -> background work -> round 2  -> ...
        let mut number_of_tasks_in_this_round = self.tasks.len();
        let io_worker = unsafe { local_io_worker() };

        loop {
            for _ in 0..number_of_tasks_in_this_round {
                task = unsafe { self.tasks.pop_back().unwrap_unchecked() };
                self.exec_task(task);
            }

            if unlikely(self.background_task(io_worker)) {
                break;
            }

            number_of_tasks_in_this_round = self.tasks.len();
        }

        uninit_local_executor();
        unsafe { LOCAL_WORKER = None; }
    }

    #[inline(always)]
    pub fn run_with_future<Fut: Future<Output = ()>>(
        &mut self,
        future: Fut,
    ) {
        self.spawn_local(future);
        self.run();
    }

    pub fn run_and_block_on<T, Fut: Future<Output = T>>(
        &mut self,
        future: Fut,
    ) -> Result<T, &'static str> {
        // Async block in async block allocates double memory.
        // But if we use async block in `Future::poll`, it allocates only one memory.
        // When I say "async block" I mean future that is represented by `async {}`.
        struct EndLocalThreadAndWriteIntoPtr<R, Fut: Future<Output = R>> {
            res_ptr: *mut Option<R>,
            future: Fut,
            worker_id: usize
        }

        impl<R, Fut: Future<Output = R>> Future for EndLocalThreadAndWriteIntoPtr<R, Fut> {
            type Output = ();

            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let this = unsafe { self.get_unchecked_mut() };
                let mut pinned_fut = unsafe { Pin::new_unchecked(&mut this.future) };
                match pinned_fut.as_mut().poll(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(res) => {
                        unsafe { this.res_ptr.write(Some(res)) };
                        unsafe { end_worker_by_id(this.worker_id) };
                        Poll::Ready(())
                    }
                }
            }
        }

        let mut res = None;
        let static_future = EndLocalThreadAndWriteIntoPtr {
            res_ptr: &mut res,
            future,
            worker_id: self.worker_id()
        };
        self.exec_future(static_future);
        self.run();
        res.ok_or(
            "The process has been ended by end() or end_local_thread() not in block_on future.",
        )
    }
}

#[inline(always)]
pub fn init_local_executor_and_run_it_for_block_on<T, Fut>(future: Fut) -> Result<T, &'static str>
where
    Fut: Future<Output = T>,
{
    Executor::init();
    local_executor().run_and_block_on(future)
}

#[cfg(test)]
mod tests {
    use crate::local::Local;
    use crate::yield_now;

    use super::*;

    #[test_macro::test]
    fn test_spawn_local_and_exec_future() {
        async fn insert(number: u16, arr: Local<Vec<u16>>) {
            arr.get_mut().push(number);
        }

        let executor = local_executor();
        let arr = Local::new(Vec::new());

        insert(10, arr.clone()).await;
        executor.spawn_local(insert(20, arr.clone()));
        executor.spawn_local(insert(30, arr.clone()));

        yield_now().await;

        assert_eq!(&vec![10, 30, 20], arr.get()); // 30, 20 because of LIFO

        let arr = Local::new(Vec::new());

        insert(10, arr.clone()).await;
        local_executor().exec_future(insert(20, arr.clone()));
        local_executor().exec_future(insert(30, arr.clone()));

        assert_eq!(&vec![10, 20, 30], arr.get()); // 20, 30 because we don't use the queue here
    }

    #[test]
    fn test_run_and_block_on() {
        async fn async_42() -> u32 {
            42
        }

        Executor::init();
        assert_eq!(Ok(42), local_executor().run_and_block_on(async_42()));
    }
}
