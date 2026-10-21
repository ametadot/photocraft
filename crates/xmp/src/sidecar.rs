//! Pure sidecar candidate planning. No filesystem access, no path authorisation,
//! no file writes, and no hidden sidecar opening in this crate.
//!
//! The trusted PhotoCraft host must enumerate candidates under its authorised
//! read roots, safely open the actual files (including Windows reparse-point
//! policy), and pass approved bytes to `XmpPacket::parse`.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CompanionKind {
    Xmp,
    Acr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamingConvention {
    Stem,
    FullFilename,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub kind: CompanionKind,
    pub naming: NamingConvention,
}

/// A record supplied by the host AFTER filesystem identity, authorisation,
/// and file type checks. The XMP parser cannot verify these assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedCandidate {
    pub candidate: Candidate,
    /// Opaque host-provided identity of the resolved filesystem object.
    /// On Windows the host must account for case-insensitive aliases,
    /// symlinks and reparse points. A path string is not a sufficient proof.
    pub resolved_identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarError {
    MissingFileName,
    MissingRawExtension,
    Ambiguous { first: PathBuf, second: PathBuf },
    UnplannedCandidate(PathBuf),
    MissingResolvedIdentity(PathBuf),
}

impl fmt::Display for SidecarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingFileName => write!(f, "RAW path has no filename"),
            Self::MissingRawExtension => write!(f, "RAW path has no extension"),
            Self::Ambiguous { .. } => write!(f, "more than one distinct sidecar matches the RAW"),
            Self::UnplannedCandidate(_) => write!(f, "host supplied a companion path outside the candidate plan"),
            Self::MissingResolvedIdentity(_) => write!(f, "host must supply a nonempty resolved file identity"),
        }
    }
}

impl Error for SidecarError {}

/// Generate all potential sidecar locations without touching the filesystem.
/// Both `image.xmp` and `image.NEF.xmp` (and case variants) are considered.
/// ACR files are inventory-only: we do not interpret them.
///
/// Conflict policy: do not silently choose between different existing files.
/// The host should call `select_authorized` after filesystem authorisation.
pub fn plan_companions(raw_path: &Path) -> Result<Vec<Candidate>, SidecarError> {
    raw_path.file_name().ok_or(SidecarError::MissingFileName)?;
    if raw_path.extension().is_none() {
        return Err(SidecarError::MissingRawExtension);
    }
    let mut plan = Vec::with_capacity(8);
    for kind in [CompanionKind::Xmp, CompanionKind::Acr] {
        let suffixes: [&str; 2] = match kind {
            CompanionKind::Xmp => ["xmp", "XMP"],
            CompanionKind::Acr => ["acr", "ACR"],
        };
        for suffix in suffixes {
            add_distinct(&mut plan, Candidate { path: raw_path.with_extension(suffix), kind, naming: NamingConvention::Stem });
            add_distinct(&mut plan, Candidate { path: raw_path.with_added_extension(suffix), kind, naming: NamingConvention::FullFilename });
        }
    }
    Ok(plan)
}

fn add_distinct(plan: &mut Vec<Candidate>, candidate: Candidate) {
    if !plan.iter().any(|c| c.path == candidate.path && c.kind == candidate.kind) {
        plan.push(candidate);
    }
}

/// Resolve host-confirmed candidates to one file or an explicit ambiguity.
/// Identical resolved object identities are aliases and are not conflicts.
/// `allowed_plan` is generated from the already-authorised original RAW path.
/// Identity and containment checks are performed by the host, not this function.
pub fn select_authorized<'a>(
    allowed_plan: &[Candidate],
    existing: &'a [AuthorizedCandidate],
    kind: CompanionKind,
) -> Result<Option<&'a AuthorizedCandidate>, SidecarError> {
    let mut chosen: Option<&AuthorizedCandidate> = None;
    for found in existing.iter().filter(|c| c.candidate.kind == kind) {
        if found.resolved_identity.is_empty() {
            return Err(SidecarError::MissingResolvedIdentity(found.candidate.path.clone()));
        }
        if !allowed_plan.contains(&found.candidate) {
            return Err(SidecarError::UnplannedCandidate(found.candidate.path.clone()));
        }
        match chosen {
            None => chosen = Some(found),
            Some(previous) if previous.resolved_identity == found.resolved_identity => {}
            Some(previous) => return Err(SidecarError::Ambiguous { first: previous.candidate.path.clone(), second: found.candidate.path.clone() }),
        }
    }
    Ok(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn Error>>;

    #[test]
    fn stem_full_and_acr_candidates_are_included() -> TestResult {
        let plan = plan_companions(Path::new("C:/photos/sample_0004.NEF"))?;
        let paths: Vec<_> = plan.iter().map(|c| c.path.to_string_lossy().into_owned()).collect();
        for expected in [
            "C:/photos/sample_0004.xmp",
            "C:/photos/sample_0004.NEF.xmp",
            "C:/photos/sample_0004.XMP",
            "C:/photos/sample_0004.NEF.XMP",
            "C:/photos/sample_0004.acr",
            "C:/photos/sample_0004.NEF.acr",
            "C:/photos/sample_0004.ACR",
            "C:/photos/sample_0004.NEF.ACR",
        ] {
            assert!(paths.iter().any(|p| p == expected), "missing candidate: {expected}");
        }
        assert_eq!(plan.len(), 8);
        Ok(())
    }

    #[test]
    fn conflicts_are_never_resolved_silently() -> TestResult {
        let plan = plan_companions(Path::new("/photos/a.NEF"))?;
        let matches: Vec<_> = plan.iter().filter(|c| c.kind == CompanionKind::Xmp).take(2).cloned().collect();
        let found = vec![
            AuthorizedCandidate { candidate: matches[0].clone(), resolved_identity: "inode-A".into() },
            AuthorizedCandidate { candidate: matches[1].clone(), resolved_identity: "inode-B".into() },
        ];
        assert!(matches!(select_authorized(&plan, &found, CompanionKind::Xmp), Err(SidecarError::Ambiguous { .. })));
        Ok(())
    }

    #[test]
    fn case_aliases_of_same_object_are_not_conflicts() -> TestResult {
        let plan = plan_companions(Path::new("C:/photos/a.NEF"))?;
        let matches: Vec<_> = plan.iter().filter(|c| c.kind == CompanionKind::Xmp).take(3).cloned().collect();
        let found = vec![
            AuthorizedCandidate { candidate: matches[0].clone(), resolved_identity: "win-fileid-123".into() },
            AuthorizedCandidate { candidate: matches[2].clone(), resolved_identity: "win-fileid-123".into() },
        ];
        assert!(select_authorized(&plan, &found, CompanionKind::Xmp)?.is_some());
        Ok(())
    }

    #[test]
    fn no_candidates_is_not_an_error() -> TestResult {
        let plan = plan_companions(Path::new("/photos/a.NEF"))?;
        assert!(select_authorized(&plan, &[], CompanionKind::Xmp)?.is_none());
        Ok(())
    }

    #[test]
    fn unplanned_or_missing_identity_is_rejected() -> TestResult {
        let plan = plan_companions(Path::new("/photos/a.NEF"))?;
        let mut extra = plan[0].clone();
        extra.path = PathBuf::from("/not-authorized/photo.xmp");
        assert!(matches!(
            select_authorized(&plan, &[AuthorizedCandidate { candidate: extra, resolved_identity: "x".into() }], CompanionKind::Xmp),
            Err(SidecarError::UnplannedCandidate(_))
        ));
        assert!(matches!(
            select_authorized(&plan, &[AuthorizedCandidate { candidate: plan[0].clone(), resolved_identity: String::new() }], CompanionKind::Xmp),
            Err(SidecarError::MissingResolvedIdentity(_))
        ));
        Ok(())
    }

    #[test]
    fn planning_does_not_require_an_existing_filesystem() -> TestResult {
        let plan = plan_companions(Path::new("/__missing__/a.NEF"))?;
        assert_eq!(plan.len(), 8);
        assert!(matches!(plan_companions(Path::new("/photos/noextension")), Err(SidecarError::MissingRawExtension)));
        Ok(())
    }
}
