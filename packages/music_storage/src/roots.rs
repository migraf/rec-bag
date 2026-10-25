//! Where storages and catalog sources are rooted, and when two roots collide.
//!
//! Roots are compared only within one namespace: host paths with host paths,
//! and remote folders within the same provider account. Two roots overlap when
//! one is equal to or nested inside the other.

use std::fmt;
use std::path::{Component, Path, PathBuf};

use rec_core::{CatalogSource, CatalogSourceKind, StorageRole};

/// Where a storage or catalog source is rooted, independent of provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootLocator {
    /// A directory on the host, normalised by [`RootLocator::host_path`].
    HostPath(PathBuf),
    /// A folder inside a remote namespace such as one provider account.
    /// `path` runs from the namespace root to the folder.
    Remote {
        namespace: String,
        path: Vec<String>,
    },
}

impl RootLocator {
    /// A host directory, canonicalised when it exists and lexically
    /// normalised otherwise, so equal directories compare equal.
    pub fn host_path(path: impl AsRef<Path>) -> Self {
        Self::HostPath(normalize_path(path.as_ref()))
    }

    /// How `self` relates to `other`.
    pub(crate) fn relation(&self, other: &Self) -> RootRelation {
        match (self, other) {
            (Self::HostPath(left), Self::HostPath(right)) => {
                if left == right {
                    RootRelation::Equal
                } else if left.starts_with(right) || right.starts_with(left) {
                    RootRelation::Nested
                } else {
                    RootRelation::Disjoint
                }
            }
            (
                Self::Remote {
                    namespace: left_namespace,
                    path: left,
                },
                Self::Remote {
                    namespace: right_namespace,
                    path: right,
                },
            ) if left_namespace == right_namespace => {
                if left == right {
                    RootRelation::Equal
                } else if left.starts_with(right) || right.starts_with(left) {
                    RootRelation::Nested
                } else {
                    RootRelation::Disjoint
                }
            }
            _ => RootRelation::Disjoint,
        }
    }
}

impl fmt::Display for RootLocator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostPath(path) => write!(formatter, "{}", path.display()),
            Self::Remote { namespace, path } => {
                write!(formatter, "{namespace}:/{}", path.join("/"))
            }
        }
    }
}

/// What a root is about to be used for. The separation rules depend on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootPurpose {
    /// A catalog source rec_bag looks in.
    CatalogSource,
    /// A music storage with the given role.
    Storage(StorageRole),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootRelation {
    Disjoint,
    Equal,
    Nested,
}

/// Whether a storage root and a catalog source root may coexist.
///
/// A backup must never share files with a source. A primary or primary
/// candidate may sit exactly at a source root (in-place adoption keeps the
/// user's library where it is) but never inside or around one.
pub(crate) fn storage_and_source_may_coexist(role: StorageRole, relation: RootRelation) -> bool {
    match relation {
        RootRelation::Disjoint => true,
        RootRelation::Equal => role != StorageRole::Backup,
        RootRelation::Nested => false,
    }
}

/// The root a catalog source looks in, when it has one on the host.
pub fn source_root(source: &CatalogSource) -> Option<RootLocator> {
    match source.kind {
        CatalogSourceKind::LocalFolder => source.config.get("path").map(RootLocator::host_path),
        _ => None,
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_host_paths_are_normalised_lexically() {
        let base = std::env::temp_dir().join(format!("rec-bag-roots-{}", uuid::Uuid::new_v4()));
        assert_eq!(
            RootLocator::host_path(base.join("a/./b/../c")),
            RootLocator::HostPath(base.join("a/c"))
        );
    }

    #[test]
    fn host_paths_relate_by_prefix_components() {
        let root = RootLocator::HostPath("/music".into());
        assert_eq!(
            root.relation(&RootLocator::HostPath("/music".into())),
            RootRelation::Equal
        );
        assert_eq!(
            root.relation(&RootLocator::HostPath("/music/sub".into())),
            RootRelation::Nested
        );
        assert_eq!(
            root.relation(&RootLocator::HostPath("/musicals".into())),
            RootRelation::Disjoint
        );
    }
}
