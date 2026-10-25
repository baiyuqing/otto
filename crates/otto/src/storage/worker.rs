//! Owned async I/O for adapters that must also provide synchronous compatibility ports.
//! Network futures run on a real runtime, never on a SQL driver thread.
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};

use super::{Error, Result};

pub type IoFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type Job<T> = Box<dyn for<'a> FnOnce(&'a mut T) -> IoFuture<'a, ()> + Send>;

pub struct Worker<T: Send + 'static> {
    sender: Mutex<Option<mpsc::Sender<Job<T>>>>,
    thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
}

impl<T: Send + 'static> Worker<T> {
    /// Capacity bounds submitted operations; initialization errors expose no SDK details.
    pub async fn open(
        capacity: usize,
        initialize: impl Future<Output = Result<T>> + Send + 'static,
    ) -> Result<Self> {
        if capacity == 0 {
            return Err(Error::Misuse("zero worker capacity".into()));
        }
        let (sender, mut receiver) = mpsc::channel::<Job<T>>(capacity);
        let (ready, opened) = oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("otto-storage".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(_) => {
                        let _ = ready.send(Err(Error::Unavailable));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let mut state = match initialize.await {
                        Ok(state) => state,
                        Err(error) => {
                            let _ = ready.send(Err(error));
                            return;
                        }
                    };
                    if ready.send(Ok(())).is_err() {
                        return;
                    }
                    // ponytail: one serial worker per adapter; a connection pool can
                    // use multiple workers when independent-operation throughput matters.
                    while let Some(job) = receiver.recv().await {
                        job(&mut state).await;
                    }
                });
            })
            .map_err(|_| Error::Unavailable)?;
        let worker = Self {
            sender: Mutex::new(Some(sender)),
            thread: Arc::new(Mutex::new(Some(thread))),
        };
        opened.await.map_err(|_| Error::Unavailable)??;
        Ok(worker)
    }

    fn submit<R: Send + 'static>(
        &self,
        operation: impl for<'a> FnOnce(&'a mut T) -> IoFuture<'a, R> + Send + 'static,
    ) -> Result<oneshot::Receiver<R>> {
        let (response, receiver) = oneshot::channel();
        let job: Job<T> = Box::new(move |state| {
            Box::pin(async move {
                let result = operation(state).await;
                // A canceled waiter does not cancel an accepted commit.
                let _ = response.send(result);
            })
        });
        self.sender
            .lock()
            .map_err(|_| Error::Unavailable)?
            .as_ref()
            .ok_or(Error::Closed)?
            .try_send(job)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => Error::Busy("storage worker capacity".into()),
                mpsc::error::TrySendError::Closed(_) => Error::Closed,
            })?;
        Ok(receiver)
    }

    pub async fn call<R: Send + 'static>(
        &self,
        operation: impl for<'a> FnOnce(&'a mut T) -> IoFuture<'a, R> + Send + 'static,
    ) -> Result<R> {
        self.submit(operation)?
            .await
            .map_err(|_| Error::CommitUnknown)
    }

    /// Compatibility calls yield the multi-thread executor while the owned I/O
    /// runtime works. Single-thread callers must use `call().await`.
    pub fn call_blocking<R: Send + 'static>(
        &self,
        operation: impl for<'a> FnOnce(&'a mut T) -> IoFuture<'a, R> + Send + 'static,
    ) -> Result<R> {
        if tokio::runtime::Handle::try_current().is_ok_and(|handle| {
            handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread
        }) {
            return Err(Error::Misuse(
                "synchronous storage requires a blocking thread or multi-thread executor".into(),
            ));
        }
        let receiver = self.submit(operation)?;
        blocking(move || receiver.blocking_recv().map_err(|_| Error::CommitUnknown))
    }

    /// Reject new work before draining every accepted job, even if this waiter drops.
    pub async fn close(&self) -> Result<()> {
        self.sender.lock().map_err(|_| Error::Unavailable)?.take();
        let thread = self.thread.clone();
        tokio::task::spawn_blocking(move || drain(&thread))
            .await
            .map_err(|_| Error::CommitUnknown)?
    }

    pub fn close_blocking(&self) -> Result<()> {
        self.sender.lock().map_err(|_| Error::Unavailable)?.take();
        blocking(|| drain(&self.thread))
    }
}

fn blocking<R>(operation: impl FnOnce() -> Result<R>) -> Result<R> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(operation)
        }
        Ok(_) => Err(Error::Misuse(
            "synchronous storage requires a blocking thread or multi-thread executor".into(),
        )),
        Err(_) => operation(),
    }
}

fn drain(thread: &Mutex<Option<std::thread::JoinHandle<()>>>) -> Result<()> {
    let mut guard = thread.lock().map_err(|_| Error::CommitUnknown)?;
    if let Some(thread) = guard.take() {
        thread.join().map_err(|_| Error::CommitUnknown)?;
    }
    Ok(())
}

impl<T: Send + 'static> Drop for Worker<T> {
    fn drop(&mut self) {
        if let Ok(mut sender) = self.sender.lock() {
            sender.take();
        }
        let _ = match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| drain(&self.thread))
            }
            _ => drain(&self.thread),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn synchronous_compatibility_yields_executor_to_async_network_runtime() {
        let worker = Worker::open(4, async { Ok(0usize) }).await.unwrap();
        let (release, waiting) = oneshot::channel();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            let _ = release.send(());
        });
        assert_eq!(
            worker
                .call_blocking(move |state| Box::pin(async move {
                    waiting.await.unwrap();
                    *state += 1;
                    *state
                }))
                .unwrap(),
            1
        );
        worker.close_blocking().unwrap();
    }

    #[tokio::test]
    async fn canceled_response_commits_once_and_close_drains_owned_work() {
        let committed = Arc::new(AtomicUsize::new(0));
        let state = committed.clone();
        let worker = Arc::new(Worker::open(4, async move { Ok(state) }).await.unwrap());
        let (accepted, submitted) = oneshot::channel();
        let (release, commit) = oneshot::channel();
        let owner = worker.clone();
        let waiter = tokio::spawn(async move {
            owner
                .call(move |state| {
                    Box::pin(async move {
                        let _ = accepted.send(());
                        let _ = commit.await;
                        state.fetch_add(1, Ordering::SeqCst);
                    })
                })
                .await
        });
        submitted.await.unwrap();
        waiter.abort();
        let closer = worker.clone();
        let closing = tokio::spawn(async move { closer.close().await });
        tokio::task::yield_now().await;
        assert!(!closing.is_finished());
        release.send(()).unwrap();
        closing.await.unwrap().unwrap();
        assert_eq!(committed.load(Ordering::SeqCst), 1);
        assert!(matches!(
            worker.call(|_| Box::pin(async {})).await,
            Err(Error::Closed)
        ));
    }
}
