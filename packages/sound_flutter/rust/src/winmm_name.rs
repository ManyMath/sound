//! Prefix matching for fixed-width WinMM device names.

pub(crate) fn is_name_prefix(wanted: &str, enumerated: &str) -> bool {
    let wanted = wanted.trim().to_lowercase();
    let folded = enumerated.to_lowercase();
    if wanted.is_empty() || folded.is_empty() {
        return false;
    }
    folded.starts_with(&wanted)
        || (enumerated.encode_utf16().count() == 31 && wanted.starts_with(&folded))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prefix_queries_match_but_short_device_names_do_not_expand() {
        assert!(is_name_prefix(
            " cable input ",
            "CABLE Input (VB-Audio Virtual Cable)"
        ));
        assert!(!is_name_prefix("Speakers (USB)", "Speakers"));
        assert!(!is_name_prefix("", "Speakers"));
        assert!(!is_name_prefix("Speakers", ""));
    }
    #[test]
    fn driver_truncation_is_measured_in_utf16_units() {
        let full = "CABLE Input (VB-Audio Virtual Cable)";
        let truncated: String = full.chars().take(31).collect();
        assert!(is_name_prefix(full, &truncated));
        let truncated = format!("{}🎧", "a".repeat(29));
        assert!(is_name_prefix(
            &(truncated.clone() + " full name"),
            &truncated
        ));
        assert!(!is_name_prefix(
            &("a".repeat(29) + "🎧 full name"),
            &"a".repeat(29)
        ));
    }
}
