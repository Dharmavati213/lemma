//! At most one build runs; at most one more waits.

use crate::BuildRequest;

/// One running build plus at most one waiting; a running build is never killed by a new request.
///
/// A request made while a build runs replaces the waiting one, so a burst of saves costs at most
/// one extra build, and that build sees the newest files.
#[derive(Debug, Default)]
pub struct BuildQueue {
    running: bool,
    waiting: Option<BuildRequest>,
}

impl BuildQueue {
    /// Returns `Some(req)` if it should start now; otherwise it replaces the waiting request.
    pub fn request(&mut self, req: BuildRequest) -> Option<BuildRequest> {
        if self.running {
            self.waiting = Some(req);
            None
        } else {
            self.running = true;
            Some(req)
        }
    }

    /// Call when the running build finished; returns the next request to start, if any.
    pub fn finished(&mut self) -> Option<BuildRequest> {
        let next = self.waiting.take();
        self.running = next.is_some();
        next
    }

    /// Whether a build is running.
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// The request that starts when the running build finishes.
    pub fn waiting(&self) -> Option<&BuildRequest> {
        self.waiting.as_ref()
    }

    /// Drops the waiting request (Stop also discards the queued build). Returns it, if any.
    pub fn clear_waiting(&mut self) -> Option<BuildRequest> {
        self.waiting.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Engine;
    use std::path::PathBuf;
    use std::time::Duration;

    fn req(name: &str) -> BuildRequest {
        BuildRequest {
            root: PathBuf::from(format!("/p/{name}.tex")),
            engine: Engine::PdfLatex,
            outdir: ".build".into(),
            mirror_dirs: Vec::new(),
            timeout: Duration::from_secs(180),
            latexmk: PathBuf::from("latexmk"),
        }
    }

    #[test]
    fn idle_request_starts_immediately() {
        let mut q = BuildQueue::default();
        assert!(!q.is_running());
        assert_eq!(q.request(req("a")), Some(req("a")));
        assert!(q.is_running());
        assert_eq!(q.waiting(), None);
    }

    #[test]
    fn newer_request_replaces_the_waiting_one() {
        let mut q = BuildQueue::default();
        assert_eq!(q.request(req("a")), Some(req("a")));
        assert_eq!(q.request(req("b")), None);
        assert_eq!(q.request(req("c")), None);
        assert_eq!(q.waiting(), Some(&req("c")));
        assert!(q.is_running());
        // The waiting request starts when the running one finishes; nothing waits behind it.
        assert_eq!(q.finished(), Some(req("c")));
        assert!(q.is_running());
        assert_eq!(q.waiting(), None);
        assert_eq!(q.finished(), None);
        assert!(!q.is_running());
    }

    #[test]
    fn finished_without_waiting_goes_idle() {
        let mut q = BuildQueue::default();
        q.request(req("a"));
        assert_eq!(q.finished(), None);
        assert!(!q.is_running());
        assert_eq!(q.request(req("b")), Some(req("b")));
    }

    #[test]
    fn clear_waiting_discards_the_queued_build() {
        let mut q = BuildQueue::default();
        q.request(req("a"));
        q.request(req("b"));
        assert_eq!(q.clear_waiting(), Some(req("b")));
        assert_eq!(q.finished(), None);
        assert!(!q.is_running());
        // A spurious finished() while idle stays idle.
        assert_eq!(q.finished(), None);
        assert!(!q.is_running());
    }
}
