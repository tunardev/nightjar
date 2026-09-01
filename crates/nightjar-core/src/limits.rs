/// Upper bound on daemon sleep between ticks. Keeps a config change or
/// `run --now` from waiting past one tick.
pub const MAX_SLEEP: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Limits {
    /// Address space in bytes, not resident set. See the README's
    /// limits section for why that matters on macOS.
    pub memory: Option<u64>,
    /// CPU time in seconds, not wall clock. `timeout` bounds wall clock.
    pub cpu_time: Option<u64>,
    pub processes: Option<u64>,
    pub files: Option<u64>,
}

impl Limits {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}
