//! File checkpoints: what files looked like before a turn changed them.
//!
//! Every turn that edits files gets a checkpoint, named after the sequence
//! number of the user message that started it. The first time a turn touches a
//! file, the file's content at that moment is stored. Rewinding to a checkpoint
//! restores every file recorded in it and in all later ones, so the workspace
//! is back where it was before that message.
//!
//! Only edits made through the file tools are recorded. What a shell command
//! changes is not, which is why a rewind lists exactly what it restored.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Larger files are not copied; an edit to one cannot be rewound.
const MAX_SNAPSHOT_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Entry {
    path: PathBuf,
    /// File name of the stored content inside the checkpoint directory;
    /// `None` when the file did not exist yet.
    blob: Option<String>,
}

/// A point the files of a session can be rewound to.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Checkpoint {
    /// Sequence number of the user message that started the turn.
    pub seq: u64,
    /// Files the turn changed, as absolute paths.
    pub files: Vec<PathBuf>,
}

pub(crate) struct Checkpoints {
    dir: PathBuf,
    /// The checkpoint of the turn that is running.
    current: Mutex<Option<u64>>,
}

impl Checkpoints {
    pub(crate) fn new(session_dir: &Path) -> Self {
        Checkpoints {
            dir: session_dir.join("checkpoints"),
            current: Mutex::new(None),
        }
    }

    /// Start recording for the turn begun by the user message `seq`.
    pub(crate) fn begin(&self, seq: u64) {
        *self.current.lock().unwrap() = Some(seq);
    }

    fn manifest(&self, seq: u64) -> Vec<Entry> {
        std::fs::read(self.dir.join(seq.to_string()).join("manifest.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Remember what `path` contains now, unless this turn already did.
    /// Call it before a tool changes the file.
    pub(crate) fn record(&self, path: &Path) {
        // Held throughout: two edits of one step must not both think they are first.
        let current = self.current.lock().unwrap();
        let Some(seq) = *current else { return };
        let mut entries = self.manifest(seq);
        if entries.iter().any(|entry| entry.path == path) {
            return;
        }
        let dir = self.dir.join(seq.to_string());
        let blob = match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_SNAPSHOT_BYTES => {
                let name = format!("{}.bin", entries.len());
                if std::fs::create_dir_all(&dir).is_err() || std::fs::copy(path, dir.join(&name)).is_err() {
                    return;
                }
                Some(name)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            // A directory, a file too large to copy, or one we cannot read.
            _ => return,
        };
        entries.push(Entry {
            path: path.to_path_buf(),
            blob,
        });
        if std::fs::create_dir_all(&dir).is_ok()
            && let Ok(bytes) = serde_json::to_vec(&entries)
        {
            let _ = std::fs::write(dir.join("manifest.json"), bytes);
        }
    }

    /// Checkpoints that recorded at least one file, oldest first.
    pub(crate) fn list(&self) -> Vec<Checkpoint> {
        let mut seqs: Vec<u64> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .collect();
        seqs.sort_unstable();
        seqs.into_iter()
            .map(|seq| Checkpoint {
                seq,
                files: self.manifest(seq).into_iter().map(|entry| entry.path).collect(),
            })
            .filter(|checkpoint| !checkpoint.files.is_empty())
            .collect()
    }

    /// Put every file back to what it was before the turn of `seq`, undoing
    /// that turn and all later ones. Returns the files that were restored.
    pub(crate) fn rewind(&self, seq: u64) -> std::io::Result<Vec<PathBuf>> {
        let mut restored: Vec<PathBuf> = Vec::new();
        // Newest first, so a file changed in several turns ends at its oldest state.
        for checkpoint in self.list().into_iter().rev().filter(|c| c.seq >= seq) {
            let dir = self.dir.join(checkpoint.seq.to_string());
            for entry in self.manifest(checkpoint.seq) {
                match &entry.blob {
                    Some(blob) => {
                        if let Some(parent) = entry.path.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::copy(dir.join(blob), &entry.path)?;
                    }
                    None => match std::fs::remove_file(&entry.path) {
                        Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err),
                        _ => {}
                    },
                }
                if !restored.contains(&entry.path) {
                    restored.push(entry.path);
                }
            }
            // A checkpoint is spent once it has been applied.
            std::fs::remove_dir_all(&dir)?;
        }
        restored.sort();
        Ok(restored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewinding_restores_changed_files_and_removes_created_ones() {
        let session = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let (a, b, new) = (
            work.path().join("a.txt"),
            work.path().join("b.txt"),
            work.path().join("new.txt"),
        );
        std::fs::write(&a, "a0").unwrap();
        std::fs::write(&b, "b0").unwrap();
        let checkpoints = Checkpoints::new(session.path());

        // Nothing is recorded outside a turn.
        checkpoints.record(&a);
        assert!(checkpoints.list().is_empty());

        // Turn 5 changes a twice and creates a file.
        checkpoints.begin(5);
        checkpoints.record(&a);
        std::fs::write(&a, "a1").unwrap();
        checkpoints.record(&a);
        std::fs::write(&a, "a2").unwrap();
        checkpoints.record(&new);
        std::fs::write(&new, "created").unwrap();

        // Turn 9 changes a again, and b.
        checkpoints.begin(9);
        checkpoints.record(&a);
        std::fs::write(&a, "a3").unwrap();
        checkpoints.record(&b);
        std::fs::write(&b, "b1").unwrap();
        assert_eq!(checkpoints.list().iter().map(|c| c.seq).collect::<Vec<_>>(), [5, 9]);

        // Rewinding the last turn keeps what turn 5 did.
        assert_eq!(checkpoints.rewind(9).unwrap(), [a.clone(), b.clone()]);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "a2");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "b0");
        assert!(new.exists());

        // Rewinding turn 5 brings back the original and removes the new file.
        assert_eq!(checkpoints.rewind(5).unwrap(), [a.clone(), new.clone()]);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "a0");
        assert!(!new.exists());
        assert!(checkpoints.list().is_empty());
        assert!(checkpoints.rewind(5).unwrap().is_empty());
    }
}
