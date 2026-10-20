//! Output recorders, gates and counters shared by the checks.

use super::*;

/// A `Write` sink that keeps everything written and publishes complete lines.
///
/// Clones share one buffer, so a clone may be moved into an executing task
/// while the check keeps another to read from.
#[derive(Clone)]
pub(crate) struct Recorder {
    state: Arc<Mutex<Vec<u8>>>,
    reported: Arc<Mutex<usize>>,
    lines: UnboundedSender<String>,
}

impl Recorder {
    pub(super) fn new() -> (Self, UnboundedReceiver<String>) {
        let (lines, receiver) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                state: Arc::new(Mutex::new(Vec::new())),
                reported: Arc::new(Mutex::new(0)),
                lines,
            },
            receiver,
        )
    }

    pub(super) fn text(&self) -> String {
        String::from_utf8_lossy(&self.state.lock().expect("recorder")).into_owned()
    }
}

impl std::io::Write for Recorder {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let mut buffer = self.state.lock().expect("recorder");
        let mut reported = self.reported.lock().expect("recorder");
        buffer.extend_from_slice(data);
        while let Some(offset) = buffer[*reported..].iter().position(|byte| *byte == b'\n') {
            let line = String::from_utf8_lossy(&buffer[*reported..*reported + offset]).into_owned();
            *reported += offset + 1;
            let _ = self.lines.send(line);
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A one-way latch two threads use to hand off.
#[derive(Debug, Default)]
pub(super) struct Gate {
    opened: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    pub(super) fn release(&self) {
        *self.opened.lock().expect("gate") = true;
        self.changed.notify_all();
    }

    pub(super) fn released(&self) -> bool {
        *self.opened.lock().expect("gate")
    }

    /// Blocks the calling thread until released or [`AWAIT`] elapses.
    fn wait(&self) {
        let mut opened = self.opened.lock().expect("gate");
        let deadline = Instant::now() + AWAIT;
        while !*opened {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return;
            };
            let (guard, timeout) = self
                .changed
                .wait_timeout(opened, remaining)
                .expect("gate wait");
            opened = guard;
            if timeout.timed_out() {
                return;
            }
        }
    }
}

/// A [`Recorder`] whose first write blocks until `release` opens, holding the
/// driver inside its own stream work.
#[derive(Clone)]
pub(super) struct BarrierRecorder {
    pub(super) recorder: Recorder,
    pub(super) entered: Arc<Gate>,
    pub(super) release: Arc<Gate>,
}

impl std::io::Write for BarrierRecorder {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        // Open `entered` before the data reaches the recorder: a check that
        // sees the data must find the gate open without sleeping, because
        // this write blocks a runtime worker and a tokio timer may not fire
        // until the write returns.
        self.entered.release();
        let written = self.recorder.write(data)?;
        self.release.wait();
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Counts signals from several threads so a check can wait for all of them.
#[derive(Debug, Default)]
pub(super) struct Counter {
    count: Mutex<usize>,
    changed: Condvar,
}

impl Counter {
    fn add(&self) {
        *self.count.lock().expect("counter") += 1;
        self.changed.notify_all();
    }

    pub(super) fn wait_for(&self, wanted: usize, description: &str) {
        let mut count = self.count.lock().expect("counter");
        let deadline = Instant::now() + AWAIT;
        while *count < wanted {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| panic!("timed out waiting for {description}"));
            let (guard, timeout) = self
                .changed
                .wait_timeout(count, remaining)
                .expect("counter wait");
            count = guard;
            if timeout.timed_out() && *count < wanted {
                panic!("timed out waiting for {description}");
            }
        }
    }
}

/// What one `Close` caller observed.
#[derive(Debug)]
pub(super) struct Observation {
    pub(super) error: Option<Error>,
    pub(super) returned_while_blocked: bool,
}

/// Wraps a driver to prove `Close` crossed the boundary and only returned
/// after the driver's own stream work was released.
pub(super) struct CloseDrainObserver {
    pub(super) inner: Arc<dyn Driver>,
    pub(super) entered: Counter,
    pub(super) returned: Counter,
    pub(super) observations: Mutex<Vec<Observation>>,
    pub(super) driver_work_released: Arc<Gate>,
}

#[async_trait]
impl Driver for CloseDrainObserver {
    fn id(&self) -> DriverId {
        self.inner.id()
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn execute(
        &self,
        request: Request,
        streams: Streams<'_>,
        cancel: &CancellationToken,
    ) -> (ExitStatus, Result<(), Error>) {
        self.inner.execute(request, streams, cancel).await
    }

    fn close(&self) -> Result<(), Error> {
        self.entered.add();
        let result = self.inner.close();
        let observation = Observation {
            error: result.clone().err(),
            returned_while_blocked: !self.driver_work_released.released(),
        };
        self.observations
            .lock()
            .expect("observations")
            .push(observation);
        self.returned.add();
        result
    }
}
