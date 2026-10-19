//! Portable workspace text foundation; the core binary orchestrates analysis providers.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_ENTRIES: usize = 20_000;
pub const MAX_DOCUMENT_BYTES: u64 = 2 * 1024 * 1024;
const EXCLUDED: &[&str] = &[
    ".git",
    ".build",
    ".swiftpm",
    "target",
    "node_modules",
    "build",
    "dist",
    ".venv",
    "venv",
    "__pycache__",
    ".gradle",
    ".idea",
    ".DS_Store",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Directory,
    File,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// Relative UTF-8 path using '/' separators. Never an absolute file path.
    pub path: String,
    pub name: String,
    pub kind: EntryKind,
    pub language: Option<String>,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub root: String,
    pub name: String,
    pub entries: Vec<FileEntry>,
    pub file_count: usize,
    pub languages: BTreeMap<String, usize>,
    pub skipped_entries: usize,
    pub truncated: bool,
    pub analysis_level: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub path: String,
    pub language: Option<String>,
    pub content: String,
    pub size_bytes: u64,
    pub line_count: usize,
    pub encoding: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoreError {
    pub code: &'static str,
    pub message: String,
}

impl CoreError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for CoreError {}

pub fn language_for(path: &Path) -> Option<&'static str> {
    Some(
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "py" | "pyi" => "Python",
            "c" | "h" => "C",
            "cc" | "cpp" | "cxx" | "hpp" | "hxx" | "hh" => "C++",
            "rs" => "Rust",
            "java" => "Java",
            // These are text-reading labels, not semantic support promises.
            "swift" => "Swift",
            "toml" => "TOML",
            "json" => "JSON",
            "md" => "Markdown",
            "yaml" | "yml" => "YAML",
            "sh" => "Shell",
            "html" => "HTML",
            "css" => "CSS",
            "js" | "mjs" => "JavaScript",
            "ts" | "tsx" => "TypeScript",
            _ => return None,
        },
    )
}

pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub fn open(root: &Path) -> Result<Self, CoreError> {
        let root = fs::canonicalize(root)
            .map_err(|e| CoreError::new("workspace_unavailable", e.to_string()))?;
        if !root.is_dir() {
            return Err(CoreError::new(
                "not_a_directory",
                "Workspace root must be a directory.",
            ));
        }
        if root.to_str().is_none() {
            return Err(CoreError::new(
                "unsupported_path",
                "Workspace root is not valid UTF-8.",
            ));
        }
        Ok(Self { root })
    }

    pub fn scan(&self) -> Result<WorkspaceSnapshot, CoreError> {
        let mut entries = Vec::new();
        let mut languages = BTreeMap::new();
        let mut file_count = 0;
        let mut skipped_entries = 0;
        let mut truncated = false;
        let walker = WalkDir::new(&self.root)
            .follow_links(false)
            .max_depth(32)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|entry| {
                entry.depth() == 0 || !EXCLUDED.iter().any(|name| entry.file_name() == *name)
            });

        for entry in walker {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    skipped_entries += 1;
                    continue;
                }
            };
            if entry.depth() == 0 {
                continue;
            }
            if entry.file_type().is_symlink()
                || !(entry.file_type().is_file() || entry.file_type().is_dir())
            {
                skipped_entries += 1;
                continue;
            }
            if entries.len() == MAX_ENTRIES {
                truncated = true;
                break;
            }
            if entry.depth() == 32 && entry.file_type().is_dir() {
                // Its contents may be omitted by the depth guard.
                truncated = true;
            }
            let relative = entry
                .path()
                .strip_prefix(&self.root)
                .expect("walk rooted under workspace");
            let Some(path) = relative.to_str() else {
                skipped_entries += 1;
                continue;
            };
            let Some(name) = entry.file_name().to_str() else {
                skipped_entries += 1;
                continue;
            };
            let is_file = entry.file_type().is_file();
            let language = is_file
                .then(|| language_for(entry.path()))
                .flatten()
                .map(str::to_owned);
            if is_file {
                file_count += 1;
                if let Some(language) = &language {
                    *languages.entry(language.clone()).or_insert(0) += 1;
                }
            }
            entries.push(FileEntry {
                path: path.replace(std::path::MAIN_SEPARATOR, "/"),
                name: name.to_owned(),
                kind: if is_file {
                    EntryKind::File
                } else {
                    EntryKind::Directory
                },
                language,
                size_bytes: if is_file {
                    match entry.metadata() {
                        Ok(metadata) => metadata.len(),
                        Err(_) => {
                            skipped_entries += 1;
                            0
                        }
                    }
                } else {
                    0
                },
            });
        }
        Ok(WorkspaceSnapshot {
            root: self.root.to_str().expect("validated UTF-8 root").to_owned(),
            name: self
                .root
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("Workspace")
                .to_owned(),
            entries,
            file_count,
            languages,
            skipped_entries,
            truncated,
            analysis_level: "text_only".into(),
        })
    }

    pub fn read_document(&self, relative: &str) -> Result<Document, CoreError> {
        // Backslashes and parent components are rejected uniformly across platforms.
        if relative.is_empty()
            || relative.contains('\\')
            || Path::new(relative).is_absolute()
            || relative
                .split('/')
                .any(|part| part == ".." || part == "." || part.is_empty())
        {
            return Err(CoreError::new(
                "invalid_path",
                "Expected a workspace-relative file path.",
            ));
        }
        let canonical = fs::canonicalize(self.root.join(relative))
            .map_err(|e| CoreError::new("file_unavailable", e.to_string()))?;
        if !canonical.starts_with(&self.root) {
            return Err(CoreError::new(
                "outside_workspace",
                "File resolves outside the workspace.",
            ));
        }
        let metadata = fs::metadata(&canonical)
            .map_err(|e| CoreError::new("file_unavailable", e.to_string()))?;
        if !metadata.is_file() {
            return Err(CoreError::new(
                "not_a_file",
                "Selection is not a regular file.",
            ));
        }
        if metadata.len() > MAX_DOCUMENT_BYTES {
            return Err(CoreError::new(
                "file_too_large",
                "This first reader supports files up to 2 MiB.",
            ));
        }
        // Bound the actual read too: the file could grow after metadata was checked.
        use std::io::Read;
        let file = fs::File::open(&canonical)
            .map_err(|e| CoreError::new("file_unavailable", e.to_string()))?;
        let mut bytes = Vec::new();
        file.take(MAX_DOCUMENT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| CoreError::new("file_unavailable", e.to_string()))?;
        if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
            return Err(CoreError::new(
                "file_too_large",
                "File grew beyond the 2 MiB reading limit.",
            ));
        }
        if bytes.contains(&0) {
            return Err(CoreError::new(
                "binary_file",
                "Binary files are not supported by the source reader.",
            ));
        }
        let size_bytes = bytes.len() as u64;
        let content = String::from_utf8(bytes).map_err(|_| {
            CoreError::new(
                "unsupported_encoding",
                "Only UTF-8 text is supported in this milestone.",
            )
        })?;
        let line_count = content.bytes().filter(|b| *b == b'\n').count() + 1;
        Ok(Document {
            path: relative.into(),
            language: language_for(&canonical).map(str::to_owned),
            content,
            size_bytes,
            line_count,
            encoding: "UTF-8".into(),
        })
    }

    pub fn save_document(&self, relative: &str, content: &str) -> Result<[u8; 32], CoreError> {
        if relative.is_empty()
            || relative.contains('\\')
            || Path::new(relative).is_absolute()
            || relative
                .split('/')
                .any(|part| part == ".." || part == "." || part.is_empty())
        {
            return Err(CoreError::new(
                "invalid_path",
                "Expected a workspace-relative file path.",
            ));
        }
        let full = self.root.join(relative);
        let canonical = if full.exists() {
            fs::canonicalize(&full).map_err(|e| CoreError::new("file_unavailable", e.to_string()))?
        } else {
            full.clone()
        };
        if !canonical.starts_with(&self.root) {
            return Err(CoreError::new(
                "outside_workspace",
                "File resolves outside the workspace.",
            ));
        }
        let bytes = content.as_bytes();
        if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
            return Err(CoreError::new(
                "file_too_large",
                "File content exceeds maximum document size.",
            ));
        }
        fs::write(&canonical, bytes)
            .map_err(|e| CoreError::new("io_error", format!("Failed to write document to disk: {e}")))?;
        let read_back = fs::read(&canonical)
            .map_err(|e| CoreError::new("io_error", format!("Failed to read back document from disk: {e}")))?;
        if read_back != bytes {
            return Err(CoreError::new("digest_mismatch", "Disk content does not match buffer."));
        }
        Ok(symweft_index::hash(&read_back))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "symweft-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn scan_excludes_build_trees_and_labels_languages() {
        let f = Fixture::new();
        fs::create_dir_all(f.0.join("src")).unwrap();
        fs::create_dir_all(f.0.join("target/debug")).unwrap();
        fs::write(f.0.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(f.0.join("target/debug/generated.rs"), "hidden").unwrap();
        let snapshot = Workspace::open(&f.0).unwrap().scan().unwrap();
        assert_eq!(snapshot.file_count, 1);
        assert_eq!(snapshot.languages.get("Rust"), Some(&1));
        assert!(
            !snapshot
                .entries
                .iter()
                .any(|e| e.path.starts_with("target"))
        );
        assert_eq!(snapshot.analysis_level, "text_only");
    }

    #[test]
    fn reads_unicode_and_preserves_content() {
        let f = Fixture::new();
        fs::write(f.0.join("你好.py"), "print('织源')\n").unwrap();
        let doc = Workspace::open(&f.0)
            .unwrap()
            .read_document("你好.py")
            .unwrap();
        assert_eq!(doc.content, "print('织源')\n");
        assert_eq!(doc.line_count, 2);
        assert_eq!(doc.language.as_deref(), Some("Python"));
    }

    #[test]
    fn rejects_traversal_binary_and_oversized_input() {
        let f = Fixture::new();
        fs::write(f.0.join("binary"), [0, 1, 2]).unwrap();
        fs::File::create(f.0.join("large.rs"))
            .unwrap()
            .set_len(MAX_DOCUMENT_BYTES + 1)
            .unwrap();
        let workspace = Workspace::open(&f.0).unwrap();
        for path in ["../outside", "/etc/passwd", "src/../file", "a\\b"] {
            assert_eq!(
                workspace.read_document(path).unwrap_err().code,
                "invalid_path"
            );
        }
        assert_eq!(
            workspace.read_document("binary").unwrap_err().code,
            "binary_file"
        );
        assert_eq!(
            workspace.read_document("large.rs").unwrap_err().code,
            "file_too_large"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape_and_does_not_traverse_links() {
        let f = Fixture::new();
        let outside = Fixture::new();
        fs::write(outside.0.join("secret.txt"), "outside").unwrap();
        std::os::unix::fs::symlink(outside.0.join("secret.txt"), f.0.join("link.txt")).unwrap();
        let workspace = Workspace::open(&f.0).unwrap();
        assert_eq!(
            workspace.read_document("link.txt").unwrap_err().code,
            "outside_workspace"
        );
        let snapshot = workspace.scan().unwrap();
        assert_eq!(snapshot.file_count, 0);
        assert_eq!(snapshot.skipped_entries, 1);
    }
}
