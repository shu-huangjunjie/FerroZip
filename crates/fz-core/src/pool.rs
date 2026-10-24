//! A worker pool that runs jobs in parallel and hands results back either
//! strictly in submission order or (`unordered`) as soon as each finishes,
//! with a cap on how many jobs are in flight (and thus on memory). This is the
//! backbone of every parallel compressor here.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};

use crate::error::{Error, Result};

type Job<T> = Box<dyn FnOnce() -> Result<T> + Send>;

pub struct OrderedPool<T: Send + 'static> {
    job_tx: Option<Sender<(u64, Job<T>)>>,
    res_rx: Receiver<(u64, Result<T>)>,
    workers: Vec<JoinHandle<()>>,
    submitted: u64,
    taken: u64,
    max_inflight: u64,
    pending: HashMap<u64, Result<T>>,
    ordered: bool,
}

impl<T: Send + 'static> OrderedPool<T> {
    pub fn new(threads: usize, max_inflight: usize) -> Self {
        let threads = threads.max(1);
        let max_inflight = max_inflight.max(threads) as u64;
        let (job_tx, job_rx) = bounded::<(u64, Job<T>)>(max_inflight as usize);
        let (res_tx, res_rx) = unbounded();
        let workers = (0..threads)
            .map(|i| {
                let job_rx = job_rx.clone();
                let res_tx = res_tx.clone();
                std::thread::Builder::new()
                    .name(format!("fz-worker-{i}"))
                    .spawn(move || {
                        for (seq, job) in job_rx {
                            let result = catch_unwind(AssertUnwindSafe(job))
                                .unwrap_or_else(|_| Err(Error::Other("工作线程崩溃".into())));
                            if res_tx.send((seq, result)).is_err() {
                                break;
                            }
                        }
                    })
                    .expect("spawn worker thread")
            })
            .collect();
        Self {
            job_tx: Some(job_tx),
            res_rx,
            workers,
            submitted: 0,
            taken: 0,
            max_inflight,
            pending: HashMap::new(),
            ordered: true,
        }
    }

    /// Like `new`, but results are handed out in completion order, so one
    /// slow job never holds back the others.
    pub fn unordered(threads: usize, max_inflight: usize) -> Self {
        let mut pool = Self::new(threads, max_inflight);
        pool.ordered = false;
        pool
    }

    /// Queue `job`. If the in-flight limit is reached, first hands the oldest
    /// finished result(s) to `sink` (blocking until the oldest one is ready).
    pub fn push<F, S>(&mut self, job: F, sink: &mut S) -> Result<()>
    where
        F: FnOnce() -> Result<T> + Send + 'static,
        S: FnMut(T) -> Result<()>,
    {
        while self.submitted - self.taken >= self.max_inflight {
            let r = self.take_next()?;
            sink(r)?;
        }
        // Opportunistically drain whatever is already done so the writer keeps
        // pace with the workers.
        while self.taken < self.submitted {
            match self.try_take_next() {
                Some(r) => sink(r?)?,
                None => break,
            }
        }
        let seq = self.submitted;
        self.submitted += 1;
        self.job_tx
            .as_ref()
            .expect("pool open")
            .send((seq, Box::new(job)))
            .map_err(|_| Error::Other("工作线程已退出".into()))
    }

    /// Wait for every queued job and hand the results to `sink` in order.
    pub fn finish<S>(mut self, sink: &mut S) -> Result<()>
    where
        S: FnMut(T) -> Result<()>,
    {
        while self.taken < self.submitted {
            let r = self.take_next()?;
            sink(r)?;
        }
        Ok(())
    }

    fn ready_key(&self) -> Option<u64> {
        if self.ordered {
            self.pending.contains_key(&self.taken).then_some(self.taken)
        } else {
            self.pending.keys().next().copied()
        }
    }

    fn try_take_next(&mut self) -> Option<Result<T>> {
        while let Ok((seq, r)) = self.res_rx.try_recv() {
            self.pending.insert(seq, r);
        }
        let r = self.pending.remove(&self.ready_key()?)?;
        self.taken += 1;
        Some(r)
    }

    fn take_next(&mut self) -> Result<T> {
        loop {
            if let Some(k) = self.ready_key() {
                self.taken += 1;
                return self.pending.remove(&k).expect("key present");
            }
            let (seq, r) = self
                .res_rx
                .recv()
                .map_err(|_| Error::Other("工作线程已退出".into()))?;
            self.pending.insert(seq, r);
        }
    }
}

impl<T: Send + 'static> Drop for OrderedPool<T> {
    fn drop(&mut self) {
        self.job_tx.take();
        // Drain results so workers blocked on nothing can exit; they finish
        // the (bounded) queue and stop when the job channel closes.
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}
