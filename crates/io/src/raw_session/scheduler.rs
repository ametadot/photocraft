//! Bounded automatic recovery scheduling. The caller polls with a monotonic
//! clock; serialization and publication run on one private native worker.
//! Enqueueing never acknowledges recovery, and failures require explicit retry.

use super::Session;
use super::store::{CheckpointAck, RecoveryStore, StoreError};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::time::{Duration, Instant};

enum Request {
    Checkpoint(Session),
    Release,
}

pub enum Event {
    Checkpoint(Result<CheckpointAck, StoreError>),
    Released(RecoveryStore),
    Unavailable,
}

/// One worker request and one pending snapshot are the entire queue. A controller
/// must retain its live model until the matching acknowledgement arrives.
pub struct Scheduler {
    requests: SyncSender<Request>,
    events: Receiver<Event>,
    session: String,
    epoch: u64,
    newest: Option<u64>,
    pending: Option<Session>,
    first_dirty: Option<Instant>,
    last_edit: Option<Instant>,
    busy: bool,
    blocked: bool,
    releasing: bool,
    released: bool,
}

impl Scheduler {
    pub const IDLE: Duration = Duration::from_secs(1);
    pub const MAX_DELAY: Duration = Duration::from_secs(5);

    pub fn start(store: RecoveryStore) -> std::io::Result<Self> {
        let session = store.session_id().to_owned();
        let epoch = store.epoch();
        let (requests, input) = mpsc::sync_channel(1);
        let (output, events) = mpsc::sync_channel(1);
        std::thread::Builder::new().name("raw-recovery".into()).spawn(move || {
            let mut store = store;
            while let Ok(request) = input.recv() {
                match request {
                    Request::Checkpoint(snapshot) => {
                        if output.send(Event::Checkpoint(store.checkpoint(&snapshot))).is_err() {
                            break;
                        }
                    }
                    Request::Release => {
                        let _ = output.send(Event::Released(store));
                        break;
                    }
                }
            }
        })?;
        Ok(Self {
            requests,
            events,
            session,
            epoch,
            newest: None,
            pending: None,
            first_dirty: None,
            last_edit: None,
            busy: false,
            blocked: false,
            releasing: false,
            released: false,
        })
    }

    /// Supply an immutable coherent snapshot after a durable-state mutation.
    /// Replacing pending work does not reset the sustained-edit deadline.
    pub fn submit(&mut self, snapshot: Session, now: Instant) -> Result<(), String> {
        if self.releasing || snapshot.id != self.session || snapshot.epoch != self.epoch {
            return Err("Recovery scheduler ownership mismatch or releasing".into());
        }
        if self.newest.is_some_and(|generation| snapshot.generation <= generation) {
            return Err("Recovery snapshots must advance their generation".into());
        }
        if snapshot.photos.len() > super::MAX_PHOTOS {
            return Err("Too many recovery photographs".into());
        }
        self.newest = Some(snapshot.generation);
        self.pending = Some(snapshot);
        self.first_dirty.get_or_insert(now);
        self.last_edit = Some(now);
        Ok(())
    }

    /// Invoke on the UI/controller timer, or force at a frozen lifecycle barrier.
    /// A force request still cannot skip an in-flight publication.
    pub fn tick(&mut self, now: Instant, force: bool) -> Result<bool, String> {
        if self.blocked || self.busy || self.releasing || self.pending.is_none() {
            return Ok(false);
        }
        let due = force
            || self.first_dirty.is_some_and(|first| now.saturating_duration_since(first) >= Self::MAX_DELAY)
            || self.last_edit.is_some_and(|last| now.saturating_duration_since(last) >= Self::IDLE);
        if !due {
            return Ok(false);
        }
        if let Some(snapshot) = self.pending.take() {
            if self.requests.try_send(Request::Checkpoint(snapshot)).is_err() {
                self.blocked = true;
                return Err("Recovery worker unavailable; current edits remain unacknowledged".into());
            }
            self.busy = true;
            self.first_dirty = None;
            self.last_edit = None;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn poll(&mut self) -> Option<Event> {
        match self.events.try_recv() {
            Ok(event) => {
                self.busy = false;
                if matches!(&event, Event::Released(_)) {
                    self.released = true;
                }
                if matches!(&event, Event::Checkpoint(Err(_)) | Event::Unavailable) {
                    self.blocked = true;
                }
                Some(event)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) if !self.released && (!self.blocked || self.releasing) => {
                self.blocked = true;
                self.busy = false;
                self.releasing = false;
                Some(Event::Unavailable)
            }
            Err(TryRecvError::Disconnected) => None,
        }
    }

    /// Transfer exclusive store ownership back for reconciliation, pinning and
    /// journalled Done. Poll until Released; this never blocks the UI thread.
    /// Pending work must first be acknowledged, or explicitly abandoned by the
    /// caller while retaining the live model for reconciliation.
    pub fn release(&mut self, abandon_pending: bool) -> Result<(), String> {
        if self.busy || self.releasing || (self.pending.is_some() && !abandon_pending) {
            return Err("Recovery publication has not settled".into());
        }
        self.requests.try_send(Request::Release).map_err(|_| "Recovery worker unavailable")?;
        self.releasing = true;
        self.pending = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lost_release_response_is_reported_once() {
        let (requests, _input) = mpsc::sync_channel(1);
        let (output, events) = mpsc::sync_channel(1);
        let mut scheduler = Scheduler {
            requests,
            events,
            session: "test".into(),
            epoch: 1,
            newest: None,
            pending: None,
            first_dirty: None,
            last_edit: None,
            busy: false,
            blocked: false,
            releasing: false,
            released: false,
        };
        scheduler.release(false).unwrap();
        drop(output);
        assert!(matches!(scheduler.poll(), Some(Event::Unavailable)));
        assert!(scheduler.poll().is_none());
    }

    fn event(scheduler: &mut Scheduler) -> Event {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(event) = scheduler.poll() {
                return event;
            }
            assert!(Instant::now() < deadline, "worker did not respond");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn native_scheduler_coalesces_and_old_ack_cannot_save_new_edits() {
        let root = std::env::temp_dir().join(format!(
            "raw-scheduler-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let mut session = super::super::tests::session();
        let store = RecoveryStore::claim(&root, &session.id).unwrap();
        session.epoch = store.epoch();
        let mut scheduler = Scheduler::start(store).unwrap();
        let now = Instant::now();
        scheduler.submit(session.clone(), now).unwrap();
        assert!(!scheduler.tick(now + Duration::from_millis(999), false).unwrap());
        // Sustained edits replace the sole queued snapshot but preserve deadline.
        for second in 1..=5 {
            session.generation += 1;
            scheduler.submit(session.clone(), now + Duration::from_secs(second)).unwrap();
        }
        assert!(scheduler.tick(now + Duration::from_secs(5), false).unwrap());
        session.generation += 1;
        scheduler.submit(session.clone(), now + Duration::from_secs(5)).unwrap();
        assert!(!scheduler.tick(now + Duration::from_secs(5), true).unwrap());
        let Event::Checkpoint(Ok(ack)) = event(&mut scheduler) else { panic!("checkpoint failed") };
        assert_eq!(ack.generation, 6);
        assert!(session.acknowledge_cache(&ack.session, ack.epoch, ack.generation));
        assert!(!session.current_cached());
        assert!(scheduler.release(false).is_err());
        assert!(scheduler.tick(now + Duration::from_secs(5), true).unwrap());
        let Event::Checkpoint(Ok(ack)) = event(&mut scheduler) else { panic!("checkpoint failed") };
        assert!(session.acknowledge_cache(&ack.session, ack.epoch, ack.generation));
        assert!(session.current_cached());
        scheduler.release(false).unwrap();
        let Event::Released(store) = event(&mut scheduler) else { panic!("store not released") };
        assert_eq!(store.load().unwrap().unwrap().session.generation, 7);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_checkpoint_blocks_auto_retry_and_returns_owned_store() {
        let root = std::env::temp_dir().join(format!(
            "raw-scheduler-failure-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let mut session = super::super::tests::session();
        let mut store = RecoveryStore::claim(&root, &session.id).unwrap();
        session.epoch = store.epoch();
        store.checkpoint(&session).unwrap();
        let mut scheduler = Scheduler::start(store).unwrap();
        let now = Instant::now();
        let mut wrong_owner = session.clone();
        wrong_owner.epoch += 1;
        assert!(scheduler.submit(wrong_owner, now).is_err());
        session.generation += 1;
        session.schema = u32::MAX;
        scheduler.submit(session.clone(), now).unwrap();
        assert!(scheduler.submit(session.clone(), now).is_err());
        assert!(scheduler.tick(now, true).unwrap());
        assert!(matches!(event(&mut scheduler), Event::Checkpoint(Err(_))));
        session.schema = 1;
        session.generation += 1;
        scheduler.submit(session.clone(), now).unwrap();
        assert!(!scheduler.tick(now + Duration::from_secs(10), true).unwrap());
        assert!(matches!(RecoveryStore::claim(&root, &session.id), Err(StoreError::Owned)));
        scheduler.release(true).unwrap();
        let Event::Released(store) = event(&mut scheduler) else { panic!("store not released") };
        assert_eq!(store.load().unwrap().unwrap().session.generation, 1);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
