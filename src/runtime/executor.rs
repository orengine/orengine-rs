use std::collections::{BTreeSet, VecDeque};
use std::future::Future;
use std::intrinsics::unlikely;
use std::{mem, thread};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use crossbeam::utils::CachePadded;
use fastrand::Rng;
use crate::atomic_task_queue::AtomicTaskList;
use crate::check_task_local_safety;
use crate::io::sys::WorkerSys;
use crate::io::worker::{init_local_worker, IoWorker, LOCAL_WORKER, local_worker_option};
use crate::runtime::call::Call;
use crate::runtime::config::{Config, ValidConfig};
use crate::runtime::global_state::{register_local_executor, SubscribedState};
use crate::runtime::{get_core_id_for_executor, SharedExecutorTaskList};
use crate::runtime::end_local_thread_and_write_into_ptr::EndLocalThreadAndWriteIntoPtr;
use crate::runtime::local_thread_pool::LocalThreadWorkerPool;
use crate::runtime::task::{Task, TaskPool};
use crate::runtime::waker::create_waker;
use crate::sleep::sleeping_task::SleepingTask;
use crate::utils::CoreId;

/// Thread local [`Executor`]. So, it is lockless.
#[thread_local]
pub static mut LOCAL_EXECUTOR: Option<Executor> = None;

/// Change the state of local thread to pre-initialized.
fn uninit_local_executor() {
    unsafe { LOCAL_EXECUTOR = None }
    unsafe { LOCAL_WORKER = None; }
}

/// Message that prints out when local executor is not initialized
/// but [`local_executor()`](local_executor) is called.
#[cfg(debug_assertions)]
pub(crate) const MSG_LOCAL_EXECUTOR_IS_NOT_INIT: &str = "\
------------------------------------------------------------------------------------------
|    Local executor is not initialized.                                                  |
|    Please initialize it first.                                                         |
|                                                                                        |
|    First way:                                                                          |
|    1 - let executor = Executor::init();                                                |
|    2 - executor.run_with_global_future(your_future) or                                 |
|        executor.run_with_local_future(your_future)                                     |
|                                                                                        |
|    ATTENTION:                                                                          |
|    To stop the executor, save in the start of the future local_executor().id()         |
|    and call orengine::stop_executor(executor_id), or                                   |
|    call orengine::stop_all_executors to stop the entire runtime.                       |
|                                                                                        |
|    Second way:                                                                         |
|    1 - let executor = Executor::init();                                                |
|    2 - executor.spawn_local(your_future) or                                            |
|        executor.spawn_global(your_future)                                              |
|    3 - executor.run()                                                                  |
|                                                                                        |
|    ATTENTION:                                                                          |
|    To stop the executor, save in the start of the future local_executor().id()         |
|    and call orengine::stop_executor(executor_id), or                                   |
|    call orengine::stop_all_executors to stop the entire runtime.                       |
|                                                                                        |
|    Third way:                                                                          |
|    1 - let executor = Executor::init();                                                |
|    2 - executor.run_and_block_on_local(your_future) or                                 |
|        executor.run_and_block_on_global(your_future)                                   |
|                                                                                        |
|        This will block the current thread executor until the future completes.         |
|        And after the future completes, the executor will be stopped.                   |
------------------------------------------------------------------------------------------";

/// Returns the [`Executor`] that is running in the current thread.
///
/// # Panics
///
/// If the local executor is not initialized.
///
/// # Undefined Behavior
///
/// If the local executor is not initialized and the program is in `release` mode.
///
/// Read [`MSG_LOCAL_EXECUTOR_IS_NOT_INIT`] for more details.
#[inline(always)]
pub fn local_executor() -> &'static mut Executor {
    #[cfg(debug_assertions)]
    unsafe {
        LOCAL_EXECUTOR
            .as_mut()
            .expect(MSG_LOCAL_EXECUTOR_IS_NOT_INIT)
    }

    #[cfg(not(debug_assertions))]
    unsafe {
        LOCAL_EXECUTOR
            .as_mut()
            .unwrap_unchecked()
    }
}

/// Returns the [`Executor`] that is running in the current thread.
///
/// # Undefined Behavior
///
/// If the local executor is not initialized.
#[inline(always)]
pub unsafe fn local_executor_unchecked() -> &'static mut Executor {
    unsafe { LOCAL_EXECUTOR.as_mut().unwrap_unchecked() }
}

/// The executor that runs futures in the current thread.
pub struct Executor {
    core_id: CoreId,
    executor_id: usize,
    config: ValidConfig,
    subscribed_state: SubscribedState,
    rng: Rng,

    local_tasks: VecDeque<Task>,
    global_tasks: VecDeque<Task>,
    shared_tasks_list: Option<Arc<SharedExecutorTaskList>>,

    exec_series: usize,
    local_worker: &'static mut Option<WorkerSys>,
    thread_pool: LocalThreadWorkerPool,
    current_call: Call,
    sleeping_tasks: BTreeSet<SleepingTask>,
}

pub(crate) static FREE_EXECUTOR_ID: AtomicUsize = AtomicUsize::new(0);

const MAX_NUMBER_OF_TASKS_TAKEN: usize = 16;

impl Executor {
    pub fn init_on_core_with_config(core_id: CoreId, config: Config) -> &'static mut Executor {
        let valid_config = config.validate();
        crate::utils::core::set_for_current(core_id);
        let executor_id = FREE_EXECUTOR_ID.fetch_add(1, Ordering::Relaxed);
        TaskPool::init();
        let (
            shared_tasks,
            global_tasks_list_cap
        ) = match valid_config.is_work_sharing_enabled() {
            true => (Some(Arc::new(SharedExecutorTaskList::new(executor_id))), MAX_NUMBER_OF_TASKS_TAKEN),
            false => (None, 0)
        };
        let number_of_thread_workers = valid_config.number_of_thread_workers;

        unsafe {
            if let Some(io_config) = valid_config.io_worker_config {
                init_local_worker(io_config);
            }

            LOCAL_EXECUTOR = Some(Executor {
                core_id,
                executor_id,
                config: valid_config,
                subscribed_state: SubscribedState::new(),
                rng: Rng::new(),

                local_tasks: VecDeque::new(),
                global_tasks: VecDeque::with_capacity(global_tasks_list_cap),
                shared_tasks_list: shared_tasks,

                current_call: Call::default(),
                exec_series: 0,
                local_worker: local_worker_option(),
                thread_pool: LocalThreadWorkerPool::new(number_of_thread_workers),
                sleeping_tasks: BTreeSet::new(),
            });

            register_local_executor();

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

    pub fn id(&self) -> usize {
        self.executor_id
    }

    pub(crate) fn subscribed_state(&self) -> &SubscribedState {
        &self.subscribed_state
    }

    pub(crate) fn subscribed_state_mut(&mut self) -> &mut SubscribedState {
        &mut self.subscribed_state
    }

    pub fn core_id(&self) -> CoreId {
        self.core_id
    }

    pub fn config(&self) -> Config {
        Config::from(&self.config)
    }

    pub(crate) fn shared_task_list(&self) -> Option<&Arc<SharedExecutorTaskList>> {
        self.shared_tasks_list.as_ref()
    }

    pub(crate) fn set_config_buffer_cap(&mut self, buffer_len: usize) {
        self.config.buffer_cap = buffer_len;
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
    /// * the reference must live at least as long as this state of the task
    ///
    /// * task must return [`Poll::Pending`](Poll::Pending) immediately after calling this function
    pub unsafe fn yield_current_global_task(&mut self) {
        debug_assert!(self.current_call.is_none());
        self.current_call = Call::YieldCurrentGlobalTask;
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
    pub unsafe fn push_fn_to_thread_pool(&mut self, f: &'static mut dyn Fn()) {
        debug_assert!(self.current_call.is_none());
        self.current_call = Call::PushFnToThreadPool(f)
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
        let future = unsafe { &mut *task_ref.future_ptr() };
        check_task_local_safety!(task);
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
                    Call::YieldCurrentGlobalTask => {
                        self.global_tasks.push_front(task);
                    },
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
                    Call::PushFnToThreadPool(f) => {
                        debug_assert_ne!(
                            self.config.number_of_thread_workers,
                            0,
                            "try to use thread pool with 0 workers"
                        );

                        self.thread_pool.push(task, f);
                    },
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
        self.local_tasks.push_back(task);
    }

    #[inline(always)]
    pub fn spawn_global<F>(&mut self, future: F)
    where
        F: Future<Output = ()> + Send,
    {
        let task = Task::from_future(future);
        self.spawn_global_task(task);
    }

    #[inline(always)]
    #[allow(unused_mut)] // because #[cfg(debug_assertions)]
    pub fn spawn_global_task(&mut self, mut task: Task) {
        #[cfg(debug_assertions)]
        { task.is_local = false; }
        match self.config.is_work_sharing_enabled() {
            true => {
                if unlikely(self.global_tasks.len() > self.config.work_sharing_level) {
                    let mut shared_tasks_list = unsafe {
                        self.shared_tasks_list.as_ref().unwrap_unchecked().as_vec()
                    };
                    let number_of_shared = (self.config.work_sharing_level >> 1).min(1);
                    for task in self.global_tasks.drain(..number_of_shared) {
                        shared_tasks_list.push(task);
                    }
                } else {
                    self.global_tasks.push_back(task);
                }
            }
            false => {
                self.global_tasks.push_back(task);
            }
        }
    }

    #[inline(always)]
    pub fn local_queue(&mut self) -> &mut VecDeque<Task> {
        &mut self.local_tasks
    }

    #[inline(always)]
    pub fn sleeping_tasks(&mut self) -> &mut BTreeSet<SleepingTask> {
        &mut self.sleeping_tasks
    }

    #[inline(always)]
    fn take_work_if_needed(&mut self) {
        if self.global_tasks.len() >= MAX_NUMBER_OF_TASKS_TAKEN {
            return;
        }
        if let Some(shared_task_list) = self.shared_tasks_list.as_mut() {
            if !shared_task_list.is_empty() {
                return;
            }

            let lists = unsafe { self.subscribed_state.tasks_lists() };
            if lists.is_empty() {
                return;
            }

            let max_number_of_tries = self.rng.usize(0..lists.len()) + 1;

            for i in 0..max_number_of_tries {
                let list = unsafe { lists.get_unchecked(i) };
                let limit = MAX_NUMBER_OF_TASKS_TAKEN - self.global_tasks.len();
                if limit == 0 {
                   return;
                }

                 list.take_batch(&mut self.global_tasks, limit);
            }
        }
    }

    #[inline(always)]
    /// Return true, if we need to stop ([`end_local_thread`](end_local_thread)
    /// was called or [`end`](crate::runtime::end::end)).
    fn background_task(&mut self) -> bool {
        self.subscribed_state.check_subscription(self.executor_id);
        if unlikely(self.subscribed_state.is_stopped()) {
            return true;
        }

        self.exec_series = 0;
        self.take_work_if_needed();
        self.thread_pool.poll(&mut self.local_tasks);
        let has_no_io_work = match self.local_worker {
            Some(io_worker) => io_worker.must_poll(Duration::ZERO),
            None => true,
        };

        let instant = Instant::now();
        while let Some(sleeping_task) = self.sleeping_tasks.pop_first() {
            if sleeping_task.time_to_wake() <= instant {
                self.exec_task(sleeping_task.task());
            } else {
                let need_to_sleep = sleeping_task.time_to_wake() - instant;
                self.sleeping_tasks.insert(sleeping_task);
                if unlikely(has_no_io_work) {
                    const MAX_SLEEP: Duration = Duration::from_millis(1);

                    if need_to_sleep > MAX_SLEEP {
                        let _ = thread::sleep(MAX_SLEEP);
                        break;
                    } else {
                        let _ = thread::sleep(need_to_sleep);
                    }
                } else {
                    break;
                }
            }
        }

        macro_rules! shrink {
            ($list:expr) => {
                if unlikely($list.capacity() > 512 && $list.len() * 3 < $list.capacity()) {
                    $list.shrink_to(self.local_tasks.len() * 2 + 1);
                }
            };
        }

        shrink!(self.local_tasks);
        shrink!(self.global_tasks);
        if self.shared_tasks_list.is_some() {
            let mut shared_tasks_list = unsafe {
                self.shared_tasks_list.as_ref().unwrap_unchecked().as_vec()
            };

            shrink!(shared_tasks_list);
        }

        false
    }
}

macro_rules! generate_run_and_block_on_function {
    ($func:expr, $future:expr, $executor:expr) => {
        {
            let mut res = None;
            let static_future = EndLocalThreadAndWriteIntoPtr::new(&mut res, $future);
            $func($executor, static_future);
            $executor.run();
            res.ok_or(
                "The process has been stopped by stop_all_executors \
                or stop_executor not in block_on future."
            )
        }
    };
}

// region run

impl Executor {
    pub fn run(&mut self) {
        let mut task;
        // A round is a number of tasks that must be completed before the next background_work call.
        // It is needed to avoid case like:
        //   Task with yield -> repeat this task -> repeat this task -> ...
        //
        // So it works like:
        //   Round 1 -> background work -> round 2  -> ...
        let mut number_of_local_tasks_in_this_round = self.local_tasks.len();
        let mut number_of_global_tasks_in_this_round = self.global_tasks.len();

        loop {
            for _ in 0..number_of_local_tasks_in_this_round {
                task = unsafe { self.local_tasks.pop_back().unwrap_unchecked() };
                self.exec_task(task);
            }

            for _ in 0..number_of_global_tasks_in_this_round {
                task = unsafe { self.global_tasks.pop_back().unwrap_unchecked() };
                self.exec_task(task);
            }

            if let Some(shared_tasks_list) = self.shared_tasks_list.as_ref() {
                if self.global_tasks.len() < self.config.work_sharing_level {
                    let prev_len = self.global_tasks.len();
                    let to_take = (self.config.work_sharing_level - prev_len)
                        .min(MAX_NUMBER_OF_TASKS_TAKEN);
                    shared_tasks_list.take_batch(&mut self.global_tasks, to_take);

                    let taken = self.global_tasks.len() - prev_len;
                    for _ in 0..taken {
                        let task = unsafe { self.global_tasks.pop_back().unwrap_unchecked() };
                        self.exec_task(task);
                    }
                }
            }

            if unlikely(self.background_task()) {
                break;
            }

            number_of_local_tasks_in_this_round = self.local_tasks.len();
            number_of_global_tasks_in_this_round = self.global_tasks.len();
        }

        uninit_local_executor();
    }

    #[inline(always)]
    pub fn run_with_local_future<Fut: Future<Output = ()>>(
        &mut self,
        future: Fut,
    ) {
        self.spawn_local(future);
        self.run();
    }

    #[inline(always)]
    pub fn run_with_global_future<Fut: Future<Output = ()> + Send>(
        &mut self,
        future: Fut,
    ) {
        self.spawn_global(future);
        self.run();
    }

    pub fn run_and_block_on_local<T, Fut: Future<Output = T>>(
        &'static mut self,
        future: Fut,
    ) -> Result<T, &'static str> {
        generate_run_and_block_on_function!(Executor::spawn_local, future, self)
    }

    pub fn run_and_block_on_global<T, Fut: Future<Output = T> + Send>(
        &'static mut self,
        future: Fut,
    ) -> Result<T, &'static str> {
        generate_run_and_block_on_function!(Executor::spawn_global, future, self)
    }
}

// endregion

#[inline(always)]
pub fn init_local_executor_and_run_it_for_block_on<T, Fut>(future: Fut) -> Result<T, &'static str>
where
    Fut: Future<Output = T>,
{
    Executor::init();
    local_executor().run_and_block_on_local(future)
}

#[cfg(test)]
mod tests {
    use crate::local::Local;
    use crate::utils::global_test_lock::GLOBAL_TEST_LOCK;
    use crate::yield_now::local_yield_now;
    use super::*;

    #[orengine_macros::test]
    fn test_spawn_local_and_exec_future() {
        async fn insert(number: u16, arr: Local<Vec<u16>>) {
            arr.get_mut().push(number);
        }

        let executor = local_executor();
        let arr = Local::new(Vec::new());

        insert(10, arr.clone()).await;
        executor.spawn_local(insert(20, arr.clone()));
        executor.spawn_local(insert(30, arr.clone()));

        local_yield_now().await;

        assert_eq!(&vec![10, 30, 20], arr.get()); // 30, 20 because of LIFO

        let arr = Local::new(Vec::new());

        insert(10, arr.clone()).await;
        local_executor().exec_future(insert(20, arr.clone()));
        local_executor().exec_future(insert(30, arr.clone()));

        assert_eq!(&vec![10, 20, 30], arr.get()); // 20, 30 because we don't use the list here
    }

    #[test]
    fn test_run_and_block_on() {
        let lock = GLOBAL_TEST_LOCK.lock();
        async fn async_42() -> u32 {
            42
        }

        Executor::init();
        assert_eq!(Ok(42), local_executor().run_and_block_on_local(async_42()));
        drop(lock);
    }

    // #[test]
    // TODO
    // fn work_sharing() {
    // }
}
