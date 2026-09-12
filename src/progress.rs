//! Progress of a sync, so that a long run (`sync --full`) shows where it is.
//!
//! The adapters report their progress through [`Progress`]. The binary draws it
//! as a progress bar in a terminal. Without a terminal (systemd, tests), the
//! progress goes nowhere: [`NoProgress`].

use std::{fmt, sync::Arc};

/// Receives the progress of one source.
pub trait Progress: Send + Sync + fmt::Debug {
    /// The number of steps, if known (collections, years, scrobbles...).
    /// `None`: the length is unknown, show only an activity indicator.
    fn set_length(&self, length: Option<u64>);

    /// The number of steps done.
    fn set_position(&self, position: u64);

    /// What happens now, for example the collection that the run reads.
    fn set_message(&self, message: String);
}

/// Ignores the progress.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoProgress;

impl Progress for NoProgress {
    fn set_length(&self, _length: Option<u64>) {}
    fn set_position(&self, _position: u64) {}
    fn set_message(&self, _message: String) {}
}

/// A [`NoProgress`], ready to put in a [`crate::sources::SyncContext`].
pub fn none() -> Arc<dyn Progress> {
    Arc::new(NoProgress)
}
