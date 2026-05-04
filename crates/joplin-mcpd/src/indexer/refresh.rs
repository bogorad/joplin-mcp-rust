pub fn requires_full_rebuild(gap_seconds: u64, lookback_cap_seconds: u64) -> bool {
    gap_seconds > lookback_cap_seconds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookback_cap_triggers_full_rebuild() {
        assert!(requires_full_rebuild(86_401, 86_400));
        assert!(!requires_full_rebuild(86_400, 86_400));
    }
}
