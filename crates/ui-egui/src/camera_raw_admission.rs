//! Requested-input accounting established before reads and retained through decode and prompts.
use crate::PhotocraftApp;
use photocraft_doc::DocId;
use photocraft_engine::jobs::JobId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Outcome {
    Pending,
    Ready,
    OutsideRaw,
    Failed(String),
    Cancelled,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Request {
    pub key: String,
    pub sequence: u64,
    pub name: String,
    pub path: Option<String>,
    pub outcome: Outcome,
    pub document: Option<u64>,
}
#[derive(Clone, Default)]
pub(crate) struct AdmissionState {
    next: u64,
    current: Option<u64>,
    requests: BTreeMap<u64, Request>,
    jobs: BTreeMap<u64, u64>,
    documents: BTreeMap<u64, u64>,
}
impl AdmissionState {
    fn register(&mut self, name: &str, path: Option<&str>) -> Result<u64, String> {
        if self.requests.len() >= photocraft_io::raw_session::MAX_PHOTOS {
            return Err("Camera Raw requested-input capacity reached; finish this session before opening another batch".into());
        }
        if name.len() > 32_768 || path.is_some_and(|path| path.len() > 32_768) {
            return Err("Requested input name or path exceeds the supported limit".into());
        }
        self.next = self.next.checked_add(1).ok_or("Admission sequence exhausted; restart after saving recovery")?;
        let sequence = self.next;
        self.requests.insert(
            sequence,
            Request {
                key: format!("input-{sequence}"),
                sequence,
                name: name.to_owned(),
                path: path.map(str::to_owned),
                outcome: Outcome::Pending,
                document: None,
            },
        );
        Ok(sequence)
    }
    fn settle(&mut self, id: u64, outcome: Outcome) -> bool {
        let Some(request) = self.requests.get_mut(&id) else { return false };
        if request.outcome != Outcome::Pending {
            return false;
        }
        request.outcome = outcome;
        true
    }
}
pub(crate) fn register(app: &mut PhotocraftApp, name: &str, path: Option<&str>) -> Result<u64, String> {
    let id = app.raw_filmstrip.admission.register(name, path)?;
    crate::camera_raw_ui::admission_changed(app);
    Ok(id)
}
/// Atomically register a routed batch. No partial batch survives a capacity error.
pub(crate) fn register_many(app: &mut PhotocraftApp, inputs: &[Option<(String, Option<String>)>]) -> Result<Vec<Option<u64>>, String> {
    let mut candidate = app.raw_filmstrip.admission.clone();
    let ids =
        inputs.iter().map(|input| input.as_ref().map(|(name, path)| candidate.register(name, path.as_deref())).transpose()).collect::<Result<Vec<_>, _>>()?;
    app.raw_filmstrip.admission = candidate;
    crate::camera_raw_ui::admission_changed(app);
    Ok(ids)
}
/// Explain why an input can or cannot be explicitly skipped from recovery.
pub(crate) fn can_cancel_request(app: &PhotocraftApp, key: &str) -> Result<(), String> {
    crate::camera_raw_ui::admission_allowed(app)?;
    let state = &app.raw_filmstrip.admission;
    let row = state.requests.values().find(|row| row.key == key).ok_or("Requested input no longer exists")?;
    if row.outcome != Outcome::Pending {
        return Err("Requested input has already settled".into());
    }
    let id = row.sequence;
    if state.current == Some(id)
        || state.jobs.values().any(|request| *request == id)
        || app.held_opens.iter().any(|held| state.documents.get(&held.doc.id.0) == Some(&id))
    {
        return Err("This input is still opening; cancel its import or profile prompt and wait for completion".into());
    }
    Ok(())
}
/// Explicitly skip a recovered request which has no live decoder or profile prompt.
pub(crate) fn cancel_request(app: &mut PhotocraftApp, key: &str) -> Result<(), String> {
    can_cancel_request(app, key)?;
    let state = &mut app.raw_filmstrip.admission;
    let id = state.requests.values().find(|row| row.key == key).map(|row| row.sequence).ok_or("Requested input no longer exists")?;
    if !state.settle(id, Outcome::Cancelled) {
        return Err("Requested input has already settled".into());
    }
    crate::camera_raw_ui::admission_changed(app);
    Ok(())
}
/// A photograph that opened in Camera Raw but whose admission was refused (for example an
/// over-budget sidecar): skipping it settles its ticket so the others can still be saved.
pub(crate) fn can_skip_unadmitted(app: &PhotocraftApp, key: &str) -> Result<(), String> {
    crate::camera_raw_ui::admission_allowed(app)?;
    let row = app.raw_filmstrip.admission.requests.values().find(|row| row.key == key).ok_or("Requested input no longer exists")?;
    if row.outcome != Outcome::Ready {
        return Err("Only an opened photograph that was not admitted can be skipped here".into());
    }
    if !crate::camera_raw_ui::unadmitted(app, key) {
        return Err("This photograph is part of the session; remove it from the session instead".into());
    }
    Ok(())
}
/// Skip such a photograph: close its review and settle its ticket as cancelled. Nothing is
/// written next to the photograph; it can be opened again later.
pub(crate) fn skip_unadmitted(app: &mut PhotocraftApp, key: &str) -> Result<(), String> {
    can_skip_unadmitted(app, key)?;
    crate::camera_raw_ui::forget_admission(app, key);
    if let Some(row) = app.raw_filmstrip.admission.requests.values_mut().find(|row| row.key == key) {
        row.outcome = Outcome::Cancelled;
    }
    crate::camera_raw_ui::admission_changed(app);
    Ok(())
}
/// "Skip this unfinished import", shared by the recovery details and the control channel:
/// a pending request, or one that opened but was never admitted.
pub(crate) fn skip(app: &mut PhotocraftApp, key: &str) -> Result<(), String> {
    let ready = app.raw_filmstrip.admission.requests.values().any(|row| row.key == key && row.outcome == Outcome::Ready);
    if ready { skip_unadmitted(app, key) } else { cancel_request(app, key) }
}
/// End a completed visit without reusing request keys in this process.
pub(crate) fn clear(app: &mut PhotocraftApp) {
    let next = app.raw_filmstrip.admission.next;
    app.raw_filmstrip.admission = AdmissionState { next, ..Default::default() };
}
/// Restore filename outcomes; old runtime document/job IDs are deliberately not rebound.
pub(crate) fn restore(app: &mut PhotocraftApp, rows: Vec<Request>) {
    clear(app);
    let state = &mut app.raw_filmstrip.admission;
    for mut row in rows {
        state.next = state.next.max(row.sequence);
        row.document = None;
        state.requests.insert(row.sequence, row);
    }
}
pub(crate) fn bind_restored(app: &mut PhotocraftApp, document: DocId, key: &str) {
    let state = &mut app.raw_filmstrip.admission;
    if let Some(row) = state.requests.values_mut().find(|row| row.key == key) {
        row.document = Some(document.0);
        state.documents.insert(document.0, row.sequence);
    }
}
pub(crate) fn snapshot(app: &PhotocraftApp) -> Vec<Request> {
    app.raw_filmstrip.admission.requests.values().cloned().collect()
}
pub(crate) fn settled(app: &PhotocraftApp) -> bool {
    app.raw_filmstrip.admission.requests.values().all(|r| r.outcome != Outcome::Pending)
}
pub(crate) fn request_for_doc(app: &PhotocraftApp, document: DocId) -> Option<Request> {
    let state = &app.raw_filmstrip.admission;
    state.documents.get(&document.0).and_then(|id| state.requests.get(id)).cloned()
}
pub(crate) fn order_for_doc(app: &PhotocraftApp, document: DocId) -> Option<u64> {
    request_for_doc(app, document).map(|r| r.sequence)
}
/// Preserve one ticket through nested synchronous wrappers, and settle any error once.
pub(crate) fn input<T>(app: &mut PhotocraftApp, name: &str, path: Option<&str>, f: impl FnOnce(&mut PhotocraftApp) -> Result<T, String>) -> Result<T, String> {
    // An Open Document script can start a fresh import while its parent's scope
    // still exists. Only an unfinished nested wrapper inherits that parent's ticket.
    let inherited =
        app.raw_filmstrip.admission.current.filter(|id| app.raw_filmstrip.admission.requests.get(id).is_some_and(|row| row.outcome == Outcome::Pending));
    if inherited.is_none() {
        crate::camera_raw_ui::admission_allowed(app)?;
    }
    let id = match inherited {
        Some(id) => id,
        None => register(app, name, path)?,
    };
    scoped(app, Some(id), f)
}
pub(crate) fn scoped<T>(app: &mut PhotocraftApp, id: Option<u64>, f: impl FnOnce(&mut PhotocraftApp) -> Result<T, String>) -> Result<T, String> {
    let previous = app.raw_filmstrip.admission.current;
    app.raw_filmstrip.admission.current = id;
    let result = f(app);
    if let Err(error) = &result {
        outcome(app, Outcome::Failed(error.clone()));
    }
    app.raw_filmstrip.admission.current = previous;
    result
}
pub(crate) fn bind_job(app: &mut PhotocraftApp, job: JobId) {
    let state = &mut app.raw_filmstrip.admission;
    if let Some(id) = state.current {
        state.jobs.insert(job.0, id);
    }
}
pub(crate) fn take_job(app: &mut PhotocraftApp, job: JobId) -> Option<u64> {
    app.raw_filmstrip.admission.jobs.remove(&job.0)
}
pub(crate) fn for_document(app: &PhotocraftApp, doc: DocId) -> Option<u64> {
    app.raw_filmstrip.admission.documents.get(&doc.0).copied()
}
pub(crate) fn bind_document(app: &mut PhotocraftApp, doc: DocId) {
    let state = &mut app.raw_filmstrip.admission;
    if let Some(id) = state.current {
        state.documents.insert(doc.0, id);
        if let Some(request) = state.requests.get_mut(&id) {
            request.document = Some(doc.0);
        }
    }
}
pub(crate) fn outcome(app: &mut PhotocraftApp, outcome: Outcome) {
    if let Some(id) = app.raw_filmstrip.admission.current
        && app.raw_filmstrip.admission.settle(id, outcome)
    {
        crate::camera_raw_ui::admission_changed(app);
    }
}
pub(crate) fn cancel_document(app: &mut PhotocraftApp, doc: DocId) {
    if let Some(id) = for_document(app, doc)
        && app.raw_filmstrip.admission.settle(id, Outcome::Cancelled)
    {
        crate::camera_raw_ui::admission_changed(app);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn twenty_two_requests_exist_before_any_decode_and_keep_original_order() {
        let mut state = AdmissionState::default();
        let ids: Vec<_> = (0..22).map(|i| state.register(&format!("photo-{i}.nef"), None).unwrap()).collect();
        assert_eq!(state.requests.len(), 22);
        assert!(state.requests.values().all(|r| r.outcome == Outcome::Pending));
        for &id in ids.iter().rev().take(20) {
            state.settle(id, Outcome::Ready);
        }
        state.settle(ids[0], Outcome::Failed("decode failed".into()));
        state.settle(ids[1], Outcome::Cancelled);
        assert_eq!(state.requests.values().map(|r| r.sequence).collect::<Vec<_>>(), ids);
        assert!(state.requests.values().all(|r| r.outcome != Outcome::Pending));
        assert_eq!(state.requests[&ids[0]].name, "photo-0.nef");
    }
    #[test]
    fn delayed_prompt_stays_pending_and_terminal_outcome_is_not_overwritten() {
        let mut state = AdmissionState::default();
        let delayed = state.register("delayed.nef", Some("delayed.nef")).unwrap();
        let failed = state.register("failed.nef", None).unwrap();
        state.settle(failed, Outcome::Failed("missing".into()));
        assert_eq!(state.requests[&delayed].outcome, Outcome::Pending);
        assert!(!state.settle(failed, Outcome::Ready));
        state.settle(delayed, Outcome::Cancelled);
        let bytes = serde_json::to_vec(&state.requests.values().collect::<Vec<_>>()).unwrap();
        let restored: Vec<Request> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored[0].name, "delayed.nef");
        assert_eq!(restored[1].outcome, Outcome::Failed("missing".into()));
    }
    fn app() -> PhotocraftApp {
        PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default())
    }
    #[test]
    fn synchronous_failures_are_named_and_nested_wrappers_do_not_duplicate_requests() {
        let mut app = app();
        assert!(app.open_file("bad.nef", b"invalid").is_err());
        let rows = snapshot(&app);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "bad.nef");
        assert!(matches!(rows[0].outcome, Outcome::Failed(_)));
        assert!(settled(&app));
        assert!(app.open_bytes("bytes.nef", b"invalid").is_err());
        assert_eq!(snapshot(&app).len(), 2);
    }
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn missing_batch_members_remain_in_filename_receipt() {
        let mut app = app();
        let root = std::env::temp_dir().join(format!("photocraft-admission-missing-{}", std::process::id()));
        let paths = (0..22).rev().map(|i| root.join(format!("photo-{i}.nef")).to_string_lossy().into_owned()).collect::<Vec<_>>();
        assert_eq!(app.open_paths(&paths), 0);
        let rows = snapshot(&app);
        assert_eq!(rows.len(), 22);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.name, format!("photo-{i}.nef"));
            assert!(matches!(row.outcome, Outcome::Failed(_)));
        }
        assert!(settled(&app));
    }
    #[test]
    fn restore_advances_keys_and_does_not_rebind_stale_runtime_documents() {
        let mut app = app();
        let id = register(&mut app, "pending.nef", None).unwrap();
        let _: Result<(), String> = scoped(&mut app, Some(id), |app| {
            bind_document(app, DocId(123));
            Ok(())
        });
        let rows = snapshot(&app);
        restore(&mut app, rows);
        assert!(request_for_doc(&app, DocId(123)).is_none());
        assert!(!settled(&app));
        let next = register(&mut app, "next.nef", None).unwrap();
        assert!(next > id);
    }
    #[test]
    fn reentrant_open_after_completion_gets_its_own_ticket() {
        let mut app = app();
        let _: Result<(), String> = input(&mut app, "parent.nef", None, |app| {
            outcome(app, Outcome::OutsideRaw);
            assert!(app.open_bytes("script-child.nef", b"invalid").is_err());
            Ok(())
        });
        let rows = snapshot(&app);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].outcome, Outcome::OutsideRaw);
        assert_eq!(rows[1].name, "script-child.nef");
        assert!(matches!(rows[1].outcome, Outcome::Failed(_)));
    }

    #[test]
    fn held_profile_requests_block_finish_until_explicit_cancel() {
        use photocraft_color::{ColorMode, SampleType};
        use photocraft_doc::Document;
        use photocraft_geom::Size;
        let mut app = app();
        for name in ["held-1.nef", "held-2.nef"] {
            let _: Result<(), String> = input(&mut app, name, None, |app| {
                let doc = Document::new(name, Size::new(1, 1), ColorMode::Rgb, SampleType::U8);
                bind_document(app, doc.id);
                crate::open_prompt::arrive(
                    app,
                    crate::open_prompt::Held {
                        doc,
                        read_only: false,
                        name: name.into(),
                        path: None,
                        warnings: Vec::new(),
                        bytes: None,
                        slot: None,
                        question: Some(serde_json::json!({"missing": true})),
                        policy_applied: false,
                    },
                );
                Ok(())
            });
        }
        assert!(app.jobs.opens.is_empty());
        assert_eq!(app.held_opens.len(), 2);
        assert!(!settled(&app));
        let key = snapshot(&app)[0].key.clone();
        assert!(cancel_request(&mut app, &key).is_err());
        assert!(snapshot(&app).iter().all(|row| row.outcome == Outcome::Pending));
        crate::open_prompt::answer(&mut app, None).unwrap();
        assert!(settled(&app));
        assert!(snapshot(&app).iter().all(|r| r.outcome == Outcome::Cancelled));
    }

    #[test]
    fn exhausted_sequence_and_oversized_batch_fail_without_partial_registration() {
        let mut app = app();
        app.raw_filmstrip.admission.next = u64::MAX - 1;
        let inputs = vec![Some(("a.nef".into(), None)), Some(("b.nef".into(), None))];
        assert!(register_many(&mut app, &inputs).is_err());
        assert!(snapshot(&app).is_empty());
        assert_eq!(app.raw_filmstrip.admission.next, u64::MAX - 1);
        app.raw_filmstrip.admission.next = 0;
        let inputs = vec![Some(("a.nef".into(), None)); photocraft_io::raw_session::MAX_PHOTOS + 1];
        assert!(register_many(&mut app, &inputs).is_err());
        assert!(snapshot(&app).is_empty());
        assert_eq!(app.raw_filmstrip.admission.next, 0);
    }
    #[test]
    fn recovered_skip_refuses_live_job_but_settles_jobless_pending_request() {
        let mut app = app();
        let id = register(&mut app, "pending.nef", None).unwrap();
        let _: Result<(), String> = scoped(&mut app, Some(id), |app| {
            bind_job(app, JobId(42));
            Ok(())
        });
        let key = snapshot(&app)[0].key.clone();
        assert!(cancel_request(&mut app, &key).is_err());
        assert!(!settled(&app));
        let rows = snapshot(&app);
        restore(&mut app, rows);
        cancel_request(&mut app, &key).unwrap();
        assert_eq!(snapshot(&app)[0].outcome, Outcome::Cancelled);
        assert!(settled(&app));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn persistence_control_skip_is_exact_and_survives_another_restore() {
        let mut app = app();
        register(&mut app, "missing-a.nef", Some("C:/missing-a.nef")).unwrap();
        register(&mut app, "missing-b.nef", Some("C:/missing-b.nef")).unwrap();
        let rows = snapshot(&app);
        restore(&mut app, rows);
        let key = snapshot(&app)[0].key.clone();
        let ctx = egui::Context::default();
        let invoke = |app: &mut PhotocraftApp, value: serde_json::Value| {
            crate::camera_raw_ui::menu(app, &ctx, "filter.cameraRaw", &serde_json::json!({"ui": {"persistence": value}})).unwrap()
        };
        assert!(invoke(&mut app, serde_json::json!({"action":"skipRequest"})).is_err());
        assert!(invoke(&mut app, serde_json::json!({"action":"skipRequest","requestKey":"unknown"})).is_err());
        invoke(&mut app, serde_json::json!({"action":"skipRequest","requestKey":key})).unwrap();
        assert!(invoke(&mut app, serde_json::json!({"action":"skipRequest","requestKey":key})).is_err());
        let rows = serde_json::from_slice::<Vec<Request>>(&serde_json::to_vec(&snapshot(&app)).unwrap()).unwrap();
        restore(&mut app, rows);
        assert_eq!(snapshot(&app)[0].name, "missing-a.nef");
        assert_eq!(snapshot(&app)[0].outcome, Outcome::Cancelled);
        assert_eq!(snapshot(&app)[1].outcome, Outcome::Pending);
        assert!(!settled(&app));
    }
}
