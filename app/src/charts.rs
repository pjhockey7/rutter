//! Chart decoding, on a thread of its own.
//!
//! A change of visible tile set — every zoom, and every pan across a tile
//! boundary — means gunzipping and decoding protobuf for each newly visible
//! tile and triangulating its polygons. On a regional chart that is 110 to
//! 290 ms, and doing it between two frames is exactly as bad as it sounds:
//! the window simply stops for a third of a second.
//!
//! No amount of optimising the work fixes that, because the work is real.
//! Triangulation is only about 40% of it and decoding is the rest, so even
//! making the triangulator free would leave a visible stall. What fixes it
//! is not doing it where the frame can see it.
//!
//! So the [`ChartSource`] lives here, on a worker thread, and the main
//! thread asks for a viewport and carries on drawing whatever it already
//! has until an answer arrives. A pan stays smooth and the new tiles appear
//! a frame or two late, which is the right trade: slightly stale chart beats
//! a frozen one.
//!
//! Two details that matter more than they look:
//!
//! * **Requests coalesce.** A drag crosses many tile sets in a second. The
//!   worker drains its queue and serves only the newest request, so it never
//!   works through a backlog of viewports nobody is looking at any more.
//! * **Answers are matched to requests.** An answer that arrives after a
//!   newer request was sent is dropped rather than uploaded, or the chart
//!   would flick back to where the drag used to be.

use std::sync::mpsc;
use std::thread;

use rutter_charts::mbtiles::Metadata;
use rutter_charts::{ChartSource, Collected};
use rutter_core::View;

/// What the main thread asks for.
#[derive(Debug)]
struct Job {
    id: u64,
    view: View,
    /// The layer selection to collect under. Carried with the job because
    /// the source lives on the worker, so this is the only way to tell it.
    layers: Vec<String>,
}

/// What comes back.
#[derive(Debug)]
struct Answer {
    id: u64,
    got: Box<Collected>,
}

/// A chart being decoded somewhere else.
#[derive(Debug)]
pub struct ChartWorker {
    to_worker: mpsc::Sender<Job>,
    from_worker: mpsc::Receiver<Answer>,
    /// The file's own description, kept on this side: the main thread needs
    /// the zoom range to pick tiles and the bounds to open on the chart,
    /// and asking the worker for them would mean waiting.
    metadata: Metadata,
    /// Id of the most recent job sent. Answers older than this are stale.
    sent: u64,
    /// Whether a job is outstanding, so the same viewport is not asked for
    /// twice while the first answer is still in flight.
    waiting: bool,
}

impl ChartWorker {
    /// Hand a chart to a worker thread.
    ///
    /// The source is moved, not shared: nothing else needs it, and a mutex
    /// around it would put the decode back on whichever thread happened to
    /// take the lock.
    ///
    /// `None` if the thread could not be started.
    ///
    /// Not a panic: on Android a process under pressure can be refused a
    /// thread, and the project's rule is that an app which opens with no
    /// chart is still a compass while one that will not open is nothing.
    /// The reason is logged.
    pub fn spawn(source: ChartSource) -> Option<Self> {
        let metadata = source.metadata().clone();
        let (to_worker, jobs) = mpsc::channel::<Job>();
        let (answers, from_worker) = mpsc::channel::<Answer>();

        if let Err(e) = thread::Builder::new()
            .name("rutter-charts".to_owned())
            .spawn(move || worker(source, &jobs, &answers))
        {
            log::error!("could not start the chart worker: {e}");
            return None;
        }

        Some(Self {
            to_worker,
            from_worker,
            metadata,
            sent: 0,
            waiting: false,
        })
    }

    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// The zoom the file can actually serve for a wanted zoom.
    pub fn clamp_zoom(&self, zoom: u8) -> u8 {
        zoom.clamp(self.metadata.min_zoom, self.metadata.max_zoom)
    }

    pub fn waiting(&self) -> bool {
        self.waiting
    }

    /// Ask for a viewport. Returns false if the worker has gone away.
    pub fn request(&mut self, view: &View, layers: &[String]) -> bool {
        self.sent += 1;
        self.waiting = true;
        self.to_worker
            .send(Job {
                id: self.sent,
                view: *view,
                layers: layers.to_vec(),
            })
            .is_ok()
    }

    /// The newest answer, if one has arrived. Never blocks.
    ///
    /// Drains the channel rather than taking one: during a drag several
    /// answers can queue up, and only the last describes where the view is
    /// now. Anything older than the latest request is discarded — uploading
    /// it would snap the chart back to a viewport already left behind.
    pub fn take_answer(&mut self) -> Option<Box<Collected>> {
        let mut newest: Option<Answer> = None;
        while let Ok(answer) = self.from_worker.try_recv() {
            if answer.id >= self.sent {
                self.waiting = false;
            }
            match &newest {
                Some(held) if held.id >= answer.id => {}
                _ => newest = Some(answer),
            }
        }
        // Still only useful if it is the answer to the latest question.
        newest.filter(|a| a.id >= self.sent).map(|a| a.got)
    }
}

/// The worker's loop.
fn worker(mut source: ChartSource, jobs: &mpsc::Receiver<Job>, answers: &mpsc::Sender<Answer>) {
    while let Ok(mut job) = jobs.recv() {
        // Skip to the newest request. A drag can queue a dozen viewports
        // while one is being decoded, and the older ones are already wrong.
        while let Ok(newer) = jobs.try_recv() {
            job = newer;
        }

        source.set_layers(&job.layers);
        let got = source.collect(&job.view);

        if answers
            .send(Answer {
                id: job.id,
                got: Box::new(got),
            })
            .is_err()
        {
            // The app has gone. Nothing left to decode for.
            break;
        }
    }
    log::debug!("chart worker finished");
}
