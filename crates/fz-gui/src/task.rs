//! Long operations run on a background thread; the UI polls their progress.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Instant;

use fz_core::extract::Summary;
use fz_core::*;

pub enum Job {
    Open { path: PathBuf, opts: OpenOptions },
    Extract { archive: Arc<Archive>, opts: ExtractOptions, then_open: Option<PathBuf> },
    Create { out: PathBuf, inputs: Vec<PathBuf>, opts: CreateOptions },
    Test { archive: Arc<Archive>, password: Option<String> },
}

pub enum Outcome {
    Opened(Archive),
    Extracted { dest: PathBuf, summary: Summary, then_open: Option<PathBuf> },
    Created { out: PathBuf },
    Tested(Summary),
}

pub struct Task {
    pub title: String,
    pub progress: Arc<Progress>,
    pub started: Instant,
    rx: Receiver<Result<Outcome>>,
    /// The job is kept so it can be retried with a password.
    pub retry: Option<Job>,
}

impl Task {
    pub fn spawn(ctx: &eframe::egui::Context, title: impl Into<String>, job: Job) -> Task {
        let progress = Arc::new(Progress::new());
        let (tx, rx) = channel();
        let retry = job.clone_for_retry();
        let p = progress.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("fz-task".into())
            .spawn(move || {
                let result = run(job, &p);
                let _ = tx.send(result);
                ctx.request_repaint();
            })
            .expect("spawn task thread");
        Task { title: title.into(), progress, started: Instant::now(), rx, retry }
    }

    pub fn poll(&self) -> Option<Result<Outcome>> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(Error::Other("任务异常终止".into()))),
        }
    }
}

impl Job {
    fn clone_for_retry(&self) -> Option<Job> {
        Some(match self {
            Job::Open { path, opts } => Job::Open { path: path.clone(), opts: opts.clone() },
            Job::Extract { archive, opts, then_open } => {
                Job::Extract { archive: archive.clone(), opts: opts.clone(), then_open: then_open.clone() }
            }
            Job::Test { archive, password } => Job::Test { archive: archive.clone(), password: password.clone() },
            Job::Create { .. } => return None,
        })
    }

    pub fn with_password(mut self, pw: String) -> Job {
        match &mut self {
            Job::Open { opts, .. } => opts.password = Some(pw),
            Job::Extract { opts, .. } => opts.password = Some(pw),
            Job::Test { password, .. } => *password = Some(pw),
            Job::Create { .. } => {}
        }
        self
    }
}

fn run(job: Job, progress: &Arc<Progress>) -> Result<Outcome> {
    match job {
        Job::Open { path, opts } => Ok(Outcome::Opened(open_archive(&path, &opts, progress)?)),
        Job::Extract { archive, opts, then_open } => {
            let summary = extract(&archive, &opts, progress)?;
            Ok(Outcome::Extracted { dest: opts.dest, summary, then_open })
        }
        Job::Create { out, inputs, opts } => {
            create_archive(&out, &inputs, &opts, progress.clone())?;
            Ok(Outcome::Created { out })
        }
        Job::Test { archive, password } => Ok(Outcome::Tested(test_archive(&archive, password.as_deref(), 0, progress)?)),
    }
}
