//! Back and Forward through the folders a file pane showed.

#[derive(Default)]
pub struct FolderHistory {
    back: Vec<String>,
    forward: Vec<String>,
    /// Where a Back or Forward is on its way to: landing there is no new visit.
    going_to: Option<String>,
}

impl FolderHistory {
    /// The pane moved from `from` to `to`.
    pub fn landed(&mut self, from: String, to: &str) {
        if self.going_to.take().as_deref() == Some(to) {
            return;
        }
        if self.back.last() != Some(&from) {
            self.back.push(from);
        }
        self.forward.clear();
    }

    pub fn go_back(&mut self, current: &str) -> Option<String> {
        let to = self.back.pop()?;
        self.forward.push(current.to_string());
        self.going_to = Some(to.clone());
        Some(to)
    }

    pub fn go_forward(&mut self, current: &str) -> Option<String> {
        let to = self.forward.pop()?;
        self.back.push(current.to_string());
        self.going_to = Some(to.clone());
        Some(to)
    }

    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn back_and_forward_walk_the_visited_folders() {
        let mut history = FolderHistory::default();
        history.landed("/".into(), "/etc");
        history.landed("/etc".into(), "/etc/ssh");
        assert_eq!(history.go_back("/etc/ssh").as_deref(), Some("/etc"));
        history.landed("/etc/ssh".into(), "/etc");
        assert_eq!(history.go_back("/etc").as_deref(), Some("/"));
        history.landed("/etc".into(), "/");
        assert!(!history.can_go_back());
        assert_eq!(history.go_forward("/").as_deref(), Some("/etc"));
        history.landed("/".into(), "/etc");
        assert_eq!(history.go_forward("/etc").as_deref(), Some("/etc/ssh"));
        history.landed("/etc".into(), "/etc/ssh");
        assert!(!history.can_go_forward());
    }

    #[test]
    fn a_new_folder_after_back_drops_the_forward_list() {
        let mut history = FolderHistory::default();
        history.landed("/".into(), "/a");
        history.go_back("/a");
        history.landed("/a".into(), "/");
        history.landed("/".into(), "/b");
        assert!(!history.can_go_forward());
        assert_eq!(history.go_back("/b").as_deref(), Some("/"));
    }

    #[test]
    fn a_back_that_lands_elsewhere_counts_as_a_visit() {
        let mut history = FolderHistory::default();
        history.landed("/".into(), "/a");
        history.go_back("/a");
        // The listing of "/" failed and the user typed another folder.
        history.landed("/a".into(), "/c");
        assert_eq!(history.go_back("/c").as_deref(), Some("/a"));
    }
}
