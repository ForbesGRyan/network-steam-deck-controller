//! Pure helper: turn a count of consecutive `usbip bind` failures into the
//! optional message the daemon writes into `Status::bind_error`. Threshold
//! is 3 — one or two transient failures stay silent so the kiosk doesn't
//! flicker, but a sustained failure surfaces the diagnostic.

#[must_use]
pub fn from_failure_count(consecutive_failures: u32) -> Option<String> {
    if consecutive_failures >= 3 {
        Some(format!(
            "usbip bind failed {consecutive_failures} times — is the usbip-host module loaded?"
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_threshold_yields_none() {
        assert_eq!(from_failure_count(0), None);
        assert_eq!(from_failure_count(1), None);
        assert_eq!(from_failure_count(2), None);
    }

    #[test]
    fn at_threshold_yields_message() {
        let msg = from_failure_count(3).expect("threshold = 3 should yield message");
        assert!(msg.starts_with("usbip bind failed 3 times"));
        assert!(msg.contains("usbip-host module loaded"));
    }

    #[test]
    fn above_threshold_uses_actual_count() {
        let msg = from_failure_count(7).expect("count above threshold should yield message");
        assert!(msg.starts_with("usbip bind failed 7 times"), "got: {msg}");
    }
}
