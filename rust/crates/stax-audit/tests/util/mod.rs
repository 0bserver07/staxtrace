//! A synthetic home directory for hermetic scanner tests — std only, no
//! tempfile dependency (the workspace's austerity holds in dev-deps too).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct TempHome {
    root: PathBuf,
}

impl TempHome {
    pub fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "stax-audit-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("create temp home");
        Self { root }
    }

    pub fn path(&self) -> PathBuf {
        self.root.clone()
    }

    pub fn mkdir(&self, rel: &str) {
        std::fs::create_dir_all(self.root.join(rel)).expect("mkdir");
    }

    pub fn write(&self, rel: &str, contents: &str) {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir -p");
        }
        std::fs::write(path, contents).expect("write fixture");
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[allow(dead_code)]
fn _path_is_used(p: &Path) -> bool {
    p.exists()
}
