# photocraft-xmp - independent read-only XMP core

Rust crate overlay for a PhotoCraft v0.7.0 local development branch.

This parser is a **bounded subset** of XMP/RDF/XML, not a full RDF graph equivalence implementation, an Adobe RAW renderer, or a sidecar writer.

## Public API

- `XmpPacket::parse(&[u8]) -> Result<XmpPacket, XmpError>` parses at most 8 MiB and preserves source bytes unchanged in memory.
- `XmpPacket::camera_raw("Exposure2012")` returns `Text("0.7853281")`, `Resource`, `Opaque` or `Absent`; no floating-point rounding.
- `XmpPacket::inspection()` produces a shared typed report for a future CLI/Session/UI adapter. All scalar controls are `NotApplied`, and complex values are `Unsupported`.
- `sidecar::plan_companions(&raw_path)` produces XMP/ACR candidates for both stem and full filename naming. It **does not read the filesystem**.
- `sidecar::select_authorized()` requires candidate records and resolved identities from the trusted PhotoCraft host, and returns an ambiguity error for distinct matches.
- `photocraft-xmp-inspect <explicit.xmp>` outputs typed JSON v2, as a stand-alone debugging tool (not the app file-open command).

## Safety boundaries

The metadata crate cannot authorize an arbitrary user-provided path or validate Windows reparse points. Opening companion files belongs to the trusted PhotoCraft file-open/automation boundary. The returned `XmpInspection<'_>` borrows values from `XmpPacket`; callers storing results in engine Session must create an owned representation or retain the packet.

No RAW/XMP/ACR write API is exposed and no parsed Adobe values are applied to PhotoCraft sliders. Absolute Kelvin must NOT be mapped to relative PhotoCraft temperature, even if both use the label `Temperature`.

## Run in the team's PhotoCraft checkout

```powershell
cargo test -p photocraft-xmp
cargo clippy -p photocraft-xmp --all-targets -- -D warnings
cargo run -p photocraft-xmp --bin photocraft-xmp-inspect -- "C:\path\to\sample_0004.xmp"
```

The feature-gated original private test takes explicit `PHOTOCRAFT_PRIVATE_NEF_PATH` and `PHOTOCRAFT_PRIVATE_XMP_PATH` environment variables. Because its expected values describe that private sidecar, they are supplied too: `PHOTOCRAFT_PRIVATE_XMP_EXPECT` (`Name=Value` pairs separated by `;`) and `PHOTOCRAFT_PRIVATE_XMP_SNAPSHOTS` (snapshot-name prefixes separated by `|`). Do not commit private assets or values taken from them.

`quick-xml = "=0.41.0"` aligns with the reviewed local fork's reported Cargo.lock entry, but the crate has **not been compiled** here. The local team must regenerate the root lockfile and run all gates before merging.

## Not implemented

- Camera Raw import into the PhotoCraft engine/Session or UI.
- Path authorization, trusted I/O, archive companion copying or file changes.
- RDF qualifiers, masks, curves, structured CRS values or comprehensive RDF-semantic comparison.
- XMP writing/merge, Adobe reopening, pixel equivalence, or Windows portable binary creation.
- Non-UTF-8 XML (e.g. UTF-16) parsing.

Read the root package `HANDOVER_GATE.md` and `LOCAL_FORK_INTEGRATION.md` before integrating.
