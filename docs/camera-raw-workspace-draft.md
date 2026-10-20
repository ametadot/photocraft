# Camera Raw retained review workspace (draft subset)

This draft starts from upstream release 0.11.1 at `46e538b3a2bacb533e89c7a30a99bbf2d91c3381`. It is a small adaptation of the retained-review and independent-selection design in PhotoCraft candidate-source-02 (`9a0edf25deb64a9c879d50b519816620f9827981`), preserved by Candidate F. It does not import that candidate stack or PR #7215.

When more than one RAW finishes decoding, each photograph remains available in the review strip. Clicking a name activates its preview; Ctrl/Cmd-click toggles its selection. Switching previews preserves each photograph's adjustments. Thumbnails appear for photographs whose previews have been prepared. Cancel discards the decoded review batch and preserves unrelated editor documents.

This patch retains upstream's **Open** behavior for the active photograph. Other retained photographs remain available for review. It does not introduce Done, XMP publication, recovery or batch Open Images. Those require the separately reviewed saving lifecycle.

Cancellation refuses while any file-open or RAW redevelopment job is pending. The refusal is visible and leaves the review intact. Full interruption of pending admissions needs a separate, verified generation/ownership foundation; this draft does not pretend that cancelling a job token proves it cannot complete.

Control transport uses the existing `ui.menu.invoke` route with `id: "filter.cameraRaw"` and a standalone `ui.workspace` object. `{}` inspects retained photo/document identities and selection. `{"activate": 42, "toggle": false}` activates a retained document, with optional selection toggle. Unknown identities and malformed values fail before changing the review.

## Remaining intended workspace scope

Bottom orientation, grid, range selection, relative group editing, rating/colour labels and sort/filter controls are not implemented by this narrow draft. Selection is currently a navigation state, not a promise of group editing. Native visual acceptance, public fixture captures, wasm, full suite and clippy remain required before promotion. The current strip is transient shell state and has no saved workspace preference.

## Regression coverage

Synthetic DNG tests exercise two-file admission, stable order, per-photo adjustment retention, independent selection, stale activation rejection, whole decoded-batch cancellation and unrelated-document preservation. The upstream importer/developer and original Camera Raw dialog are reused. Original photographs and sidecars are not test inputs.

The read-only `camera_raw_ui::retained_reviews` seam exposes retained sources and current recipes, including parked and queued photographs in stable review order. `workspace_snapshot` and `ui.workspace {}` expose the same membership plus pending-import/redevelopment readiness. No saving callback is implemented in this patch. Existing upstream open-admission identity is reused; asynchronous source-binding acceptance and the independent #7215 fix remain separate prerequisites.
