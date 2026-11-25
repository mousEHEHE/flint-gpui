//! The tabs the user closed, newest last, for "Reopen closed tab".

use uuid::Uuid;

const MAX_CLOSED_TABS: usize = 20;

/// `None` is a local terminal.
#[derive(Default)]
pub struct ClosedTabs(Vec<Option<Uuid>>);

impl ClosedTabs {
    /// A host closed twice is reopened once, at its last place.
    pub fn push(&mut self, host: Option<Uuid>) {
        if host.is_some() {
            self.0.retain(|closed| *closed != host);
        }
        self.0.push(host);
        if self.0.len() > MAX_CLOSED_TABS {
            self.0.remove(0);
        }
    }

    pub fn pop(&mut self) -> Option<Option<Uuid>> {
        self.0.pop()
    }

    /// A deleted host has nothing to reopen.
    pub fn forget(&mut self, host: Uuid) {
        self.0.retain(|closed| *closed != Some(host));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_closed_tab_comes_back_first_and_once() {
        let [a, b] = [(); 2].map(|_| Uuid::new_v4());
        let mut closed = ClosedTabs::default();
        closed.push(Some(a));
        closed.push(None);
        closed.push(Some(b));
        closed.push(Some(a));
        assert_eq!(closed.pop(), Some(Some(a)));
        assert_eq!(closed.pop(), Some(Some(b)));
        assert_eq!(closed.pop(), Some(None));
        assert_eq!(closed.pop(), None);
    }

    #[test]
    fn the_stack_keeps_the_newest_and_drops_deleted_hosts() {
        let mut closed = ClosedTabs::default();
        let first = Uuid::new_v4();
        closed.push(Some(first));
        for _ in 0..MAX_CLOSED_TABS {
            closed.push(None);
        }
        assert!(!closed.0.contains(&Some(first)));
        let host = Uuid::new_v4();
        closed.push(Some(host));
        closed.forget(host);
        assert!(!closed.0.contains(&Some(host)));
    }
}
