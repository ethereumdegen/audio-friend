use std::collections::VecDeque;

use chrono::{DateTime, Utc};

#[derive(Debug, Clone)]
pub struct ActivityEntry {
    pub timestamp: DateTime<Utc>,
    pub message: String,
}

impl ActivityEntry {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            timestamp: Utc::now(),
            message: message.into(),
        }
    }
}

pub fn push_activity(log: &mut VecDeque<ActivityEntry>, message: impl Into<String>) {
    const MAX_ACTIVITY: usize = 100;

    log.push_front(ActivityEntry::new(message));
    while log.len() > MAX_ACTIVITY {
        log.pop_back();
    }
}
