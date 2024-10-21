// TODO docs

use crate::bug_message::BUG_MESSAGE;
use crate::runtime::{Config, Locality, Task};
use crate::sync::Channel;
use crate::{local_executor, Executor};
use crossbeam::queue::SegQueue;
use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex as STDMutex};
use std::task::Poll;
use std::{panic, thread};

struct Job {
    task: Task,
    sender: Option<Arc<Channel<Job>>>,
    result_sender: Arc<Channel<(thread::Result<()>, Arc<Channel<Job>>)>>,
}

impl Job {
    pub(crate) fn new(
        task: Task,
        channel: Arc<Channel<Job>>,
        result_channel: Arc<Channel<(thread::Result<()>, Arc<Channel<Job>>)>>,
    ) -> Self {
        Self {
            task,
            sender: Some(channel),
            result_sender: result_channel,
        }
    }
}

impl Future for Job {
    type Output = ();

    fn poll(self: Pin<&mut Self>, mut cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };

        let mut future_ptr = panic::AssertUnwindSafe(this.task.future_ptr());
        let mut unwind_safe_cx = panic::AssertUnwindSafe(&mut cx);
        let handle = panic::catch_unwind(move || unsafe {
            let pinned_future = Pin::new_unchecked(&mut **future_ptr);
            pinned_future.poll(*unwind_safe_cx)
        });

        if let Ok(poll_res) = handle {
            if poll_res.is_ready() {
                local_executor().exec_global_future(async move {
                    let send_res = this
                        .result_sender
                        .send((Ok(()), this.sender.take().unwrap()))
                        .await;
                    if send_res.is_err() {
                        panic!("{BUG_MESSAGE}");
                    }
                });

                return Poll::Ready(());
            }

            Poll::Pending
        } else {
            local_executor().exec_global_future(async move {
                let send_res = this
                    .result_sender
                    .send((
                        Err(Box::new(handle.unwrap_err())),
                        this.sender.take().unwrap(),
                    ))
                    .await;
                if send_res.is_err() {
                    panic!("{BUG_MESSAGE}");
                }
            });

            Poll::Ready(())
        }
    }
}

unsafe impl Send for Job {}

pub struct ExecutorPoolJoinHandle {
    was_joined: bool,
    channel: Arc<Channel<(thread::Result<()>, Arc<Channel<Job>>)>>,
    pool: &'static ExecutorPool,
}

impl ExecutorPoolJoinHandle {
    fn new(
        channel: Arc<Channel<(thread::Result<()>, Arc<Channel<Job>>)>>,
        pool: &'static ExecutorPool,
    ) -> Self {
        Self {
            was_joined: false,
            channel,
            pool,
        }
    }

    pub(crate) async fn join(mut self) {
        self.was_joined = true;
        let (res, sender) = self.channel.recv().await.expect(BUG_MESSAGE);
        self.pool.senders_to_executors.push(sender);

        if let Err(err) = res {
            panic::resume_unwind(err);
        }
    }
}

impl Drop for ExecutorPoolJoinHandle {
    fn drop(&mut self) {
        assert!(
            self.was_joined,
            "ExecutorPoolJoinHandle::join() must be called! \
        If you don't want to wait result immediately, put it somewhere and join it later."
        );
    }
}

static EXECUTORS_FROM_POOL_IDS: STDMutex<BTreeSet<usize>> = STDMutex::new(BTreeSet::new());

pub(crate) fn is_executor_id_in_pool(id: usize) -> bool {
    EXECUTORS_FROM_POOL_IDS.lock().unwrap().contains(&id)
}

struct ExecutorPool {
    senders_to_executors: SegQueue<Arc<Channel<Job>>>,
}

fn executor_pool_cfg() -> Config {
    Config::default().disable_work_sharing()
}

impl ExecutorPool {
    pub(crate) const fn new() -> Self {
        Self {
            senders_to_executors: SegQueue::new(),
        }
    }

    pub(crate) fn new_executor(&self) -> Arc<Channel<Job>> {
        let channel = Arc::new(Channel::bounded(1));
        let channel_clone = channel.clone();
        thread::spawn(move || {
            let ex = Executor::init_with_config(executor_pool_cfg());
            EXECUTORS_FROM_POOL_IDS.lock().unwrap().insert(ex.id());
            ex.run_and_block_on_global(async move {
                loop {
                    match channel_clone.recv().await {
                        Ok(job) => {
                            job.await;
                        }
                        Err(_) => {
                            // closed, it is fine
                            break;
                        }
                    }
                }
            })
            .expect(BUG_MESSAGE);
        });

        channel
    }

    pub(crate) async fn sched_future<Fut>(&'static self, future: Fut) -> ExecutorPoolJoinHandle
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        let task = Task::from_future(future, Locality::global());
        let result_channel = Arc::new(Channel::bounded(1));
        let sender = self
            .senders_to_executors
            .pop()
            .unwrap_or(self.new_executor());

        let send_res = sender
            .send(Job::new(task, sender.clone(), result_channel.clone()))
            .await;
        if send_res.is_err() {
            panic!("{BUG_MESSAGE}");
        }

        ExecutorPoolJoinHandle::new(result_channel, self)
    }
}

static EXECUTOR_POOL: ExecutorPool = ExecutorPool::new();

pub fn sched_future_to_another_thread<Fut>(future: Fut)
where
    Fut: Future<Output = ()> + Send + 'static,
{
    local_executor().exec_global_future(async move {
        let handle = EXECUTOR_POOL.sched_future(future).await;

        handle.join().await;
    });
}
