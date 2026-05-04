#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexStatus {
    Empty,
    Building,
    Ready,
    Failed,
    Stale,
}

impl IndexStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Building => "building",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Stale => "stale",
        }
    }
}
