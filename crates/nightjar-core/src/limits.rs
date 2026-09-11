pub const MAX_SLEEP: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Limits {
    pub memory: Option<u64>,
    pub cpu_time: Option<u64>,
    pub processes: Option<u64>,
    pub files: Option<u64>,
}

impl Limits {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}
