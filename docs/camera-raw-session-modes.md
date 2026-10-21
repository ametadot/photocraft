# Camera Raw session modes

Updated 21 October 2026 for the integrated local lifecycle contract.

Opening a batch shows one centred loading dialog. Opening placeholders and staged RAW documents are hidden from the main workspace, and Camera Raw controls are shown only after decoding and native recipe admission finish. Cancel during loading cancels the pending imports; existing edited work still requires a discard decision.

A required colour-profile question takes precedence over the loading dialog, since opening cannot finish until it is answered. Ordinary editor-only imports retain their existing opening UI.

Camera Raw Preferences → Long-session recovery contains the three saving choices:

- **PhotoCraft default — recovery off:** the default for new and pre-existing preferences that lack the new field. Edits remain in memory; admission and editing create no recovery checkpoint or XMP sidecar. Done and Open Images explicitly save changed retained photographs through the validated transaction service.
- **Long-session recovery — cache saving:** explicitly opt in to background recovery. Show recovery progress is independently configurable and off by default. Turning this mode off stops automatic checkpoints while retaining all live recipes. A fresh session's draft is retired; a resumed session's earlier draft remains protected until an explicit finish or Discard recovered copy. Cancel on a resumed visit restores its entry draft even after opt-out. An unresolved XMP operation prevents that transition.
- **Live saving — planned:** disabled. No live publication is implemented, and `prefs.set` rejects an unsupported live mode.

Recovery settings and details live on that preferences page. When recovery is enabled, Keep in recovery and close is also available inside the Done and close decision dialogs. It is not an extra footer button. General and File Handling do not repeat them. An actionable failure remains visible even if routine progress is hidden.

The ordinary footer contains Open Image(s), Cancel and Done. Cancel and Done use the same themed secondary button size. Load/Save Settings and Before are in the Settings menu. Camera Raw Filter retains OK and Cancel.

Done prepares the exact XMP publication plan and then offers Save XMP and close (the default) or Back to editing. It closes only after the publication result is acknowledged, without creating editor documents. Recovery being off does not disable Done: explicit XMP publication still uses a private transaction journal. Preparing a plan never writes an image's sidecar. The journal is distinct from opting into automatic session recovery.

When recovery is off, ordinary editing and admission acknowledgements remain in memory even if an unresolved explicit XMP operation requires its journal to be retained. Clean returns from an explicit save retire that temporary journal. A cleanup failure remains visible without changing a confirmed save/removal result. Failed opt-in does not adopt durable ownership. Opt-out carries authoritative removals forward when recovery is later enabled again.

Cancel offers Discard this visit's changes or Back to editing, plus Keep in recovery and close when opted in. Keep must acknowledge the complete private checkpoint before closing and never writes XMP. A fresh discard retires its draft. A resumed discard restores the immutable visit-entry draft, overlays acknowledged XMP successes and durable removals, and publishes a new fenced recovery checkpoint. Earlier XMP files are not rewritten. Explicit Discard recovered copy is a separate terminal deletion. Unknown outcomes prevent destructive disposition; failures remain visible. Escape returns to editing.

Enter activates the focused dialog choice. Only when no choice handles Enter does Done use its default Save XMP and close action; a removal dialog has no implicit Enter-to-save fallback. RAW-only preference commands are permitted during RAW review without reapplying global history limits. Mixed preference requests still obey the existing held-document protection.

The previous RAW/JPEG/TIFF routing, source protection, group-editing, metadata, filtering, layout and JPEG export policies remain in force. JPEG/TIFF persistence still uses adjacent XMP; embedded metadata rewriting is not introduced. Native visual acceptance and the measured long-session test require their separate execution evidence.

Open Images uses the same save-all retained-member plan as Done, including hidden and unselected changes, then hands off the whole retained batch in admission order. A failed save retains review; an incomplete editor handoff retains its acknowledged progress. Private transaction journals with recovery off do not enable the Keep action.
