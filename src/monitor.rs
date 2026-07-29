//! Heartbeat timeout / debounce evaluation.
//!
//! Tracks **last heartbeat** and **last alert** independently so that sending an
//! outage notification never pretends a heartbeat arrived. That keeps
//! still-down vs recovered semantics honest across the timeout/debounce matrix.

/// Decision produced by the periodic staleness check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckDecision {
    /// Last heartbeat is within the timeout window.
    Healthy {
        secs_since_heartbeat: u64,
    },
    /// Past timeout, but still inside the post-alert debounce window — do not re-alert.
    StillDown {
        secs_since_heartbeat: u64,
        secs_since_alert: u64,
        secs_until_next_alert: u64,
    },
    /// Past timeout and debounce allows another outage alert.
    AlertOutage {
        secs_since_heartbeat: u64,
    },
}

/// Decision produced when a heartbeat is received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatDecision {
    /// Heartbeat while the monitor considers the service healthy.
    Recorded,
    /// Heartbeat after an outage (alert was sent and/or timeout already exceeded).
    Recovered {
        outage_duration_secs: u64,
    },
}

/// Evaluate whether to alert, stay silent (still down / debouncing), or treat as healthy.
///
/// Times are opaque monotonic seconds (e.g. seconds since an arbitrary epoch).
/// Only relative differences matter.
///
/// - `last_heartbeat_secs`: when the last real heartbeat was recorded
/// - `last_alert_secs`: when the last outage alert was sent (`None` if never)
/// - `timeout_secs`: how long without a heartbeat before the service is considered down
/// - `debounce_secs`: minimum gap between successive outage alerts while still down
pub fn evaluate_check(
    now_secs: u64,
    last_heartbeat_secs: u64,
    last_alert_secs: Option<u64>,
    timeout_secs: u64,
    debounce_secs: u64,
) -> CheckDecision {
    let secs_since_heartbeat = now_secs.saturating_sub(last_heartbeat_secs);

    if secs_since_heartbeat <= timeout_secs {
        return CheckDecision::Healthy {
            secs_since_heartbeat,
        };
    }

    match last_alert_secs {
        None => CheckDecision::AlertOutage {
            secs_since_heartbeat,
        },
        Some(alert_at) => {
            let secs_since_alert = now_secs.saturating_sub(alert_at);
            if secs_since_alert >= debounce_secs {
                CheckDecision::AlertOutage {
                    secs_since_heartbeat,
                }
            } else {
                CheckDecision::StillDown {
                    secs_since_heartbeat,
                    secs_since_alert,
                    secs_until_next_alert: debounce_secs.saturating_sub(secs_since_alert),
                }
            }
        }
    }
}

/// Evaluate a heartbeat receipt against current outage state.
///
/// Recovery is detected when:
/// - we have already entered an outage (`in_outage`), or
/// - the previous heartbeat is already past `timeout_secs` (missed the flag, still recovered).
pub fn evaluate_heartbeat(
    now_secs: u64,
    previous_heartbeat_secs: u64,
    in_outage: bool,
    timeout_secs: u64,
) -> HeartbeatDecision {
    let outage_duration_secs = now_secs.saturating_sub(previous_heartbeat_secs);
    if in_outage || outage_duration_secs > timeout_secs {
        HeartbeatDecision::Recovered {
            outage_duration_secs,
        }
    } else {
        HeartbeatDecision::Recorded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- evaluate_check: timeout / debounce matrix ---

    #[test]
    fn check_healthy_when_within_timeout() {
        // heartbeat at t=0, now=60, timeout=90
        let d = evaluate_check(60, 0, None, 90, 300);
        assert_eq!(
            d,
            CheckDecision::Healthy {
                secs_since_heartbeat: 60
            }
        );
    }

    #[test]
    fn check_healthy_at_exact_timeout_boundary() {
        // elapsed == timeout is still healthy (alert only when elapsed > timeout)
        let d = evaluate_check(90, 0, None, 90, 300);
        assert_eq!(
            d,
            CheckDecision::Healthy {
                secs_since_heartbeat: 90
            }
        );
    }

    #[test]
    fn check_alert_when_past_timeout_and_never_alerted() {
        let d = evaluate_check(91, 0, None, 90, 300);
        assert_eq!(
            d,
            CheckDecision::AlertOutage {
                secs_since_heartbeat: 91
            }
        );
    }

    #[test]
    fn check_still_down_inside_debounce_after_alert() {
        // heartbeat at 0, alerted at 100, now=200, debounce=300
        // still down: 200s without heartbeat, only 100s since alert
        let d = evaluate_check(200, 0, Some(100), 90, 300);
        assert_eq!(
            d,
            CheckDecision::StillDown {
                secs_since_heartbeat: 200,
                secs_since_alert: 100,
                secs_until_next_alert: 200,
            }
        );
    }

    #[test]
    fn check_realert_when_debounce_elapsed_and_still_no_heartbeat() {
        // heartbeat at 0, alerted at 100, now=400, debounce=300
        // secs_since_alert = 300 >= debounce → re-alert; secs_since_heartbeat remains 400
        // (never reset by the previous alert)
        let d = evaluate_check(400, 0, Some(100), 90, 300);
        assert_eq!(
            d,
            CheckDecision::AlertOutage {
                secs_since_heartbeat: 400
            }
        );
    }

    #[test]
    fn check_realert_just_after_debounce_boundary() {
        // alerted at 100, debounce 300 → next alert at now >= 400
        let at_boundary = evaluate_check(400, 0, Some(100), 90, 300);
        assert!(matches!(at_boundary, CheckDecision::AlertOutage { .. }));

        let just_before = evaluate_check(399, 0, Some(100), 90, 300);
        assert!(matches!(
            just_before,
            CheckDecision::StillDown {
                secs_until_next_alert: 1,
                ..
            }
        ));
    }

    #[test]
    fn check_alert_does_not_mask_true_outage_duration() {
        // Regression for issue #4: previously LAST_SEEN was reset on alert, so after
        // debounce the elapsed time looked like ~debounce instead of true downtime.
        let last_heartbeat = 0_u64;
        let first_alert_at = 100_u64;
        let after_debounce = first_alert_at + 300; // 400
        let d = evaluate_check(after_debounce, last_heartbeat, Some(first_alert_at), 90, 300);
        match d {
            CheckDecision::AlertOutage {
                secs_since_heartbeat,
            } => {
                assert_eq!(
                    secs_since_heartbeat, 400,
                    "outage duration must use last heartbeat, not last alert"
                );
            }
            other => panic!("expected AlertOutage, got {other:?}"),
        }
    }

    #[test]
    fn check_healthy_after_new_heartbeat_even_if_last_alert_recent() {
        // recovered: heartbeat at 350, previous alert at 100, now=360
        let d = evaluate_check(360, 350, Some(100), 90, 300);
        assert_eq!(
            d,
            CheckDecision::Healthy {
                secs_since_heartbeat: 10
            }
        );
    }

    #[test]
    fn check_zero_debounce_allows_immediate_realert() {
        let d = evaluate_check(200, 0, Some(199), 90, 0);
        assert_eq!(
            d,
            CheckDecision::AlertOutage {
                secs_since_heartbeat: 200
            }
        );
    }

    // --- evaluate_heartbeat: recovery semantics ---

    #[test]
    fn heartbeat_recorded_when_healthy() {
        let d = evaluate_heartbeat(50, 40, false, 90);
        assert_eq!(d, HeartbeatDecision::Recorded);
    }

    #[test]
    fn heartbeat_recovered_when_in_outage_flag_set() {
        let d = evaluate_heartbeat(200, 0, true, 90);
        assert_eq!(
            d,
            HeartbeatDecision::Recovered {
                outage_duration_secs: 200
            }
        );
    }

    #[test]
    fn heartbeat_recovered_when_timeout_already_exceeded_without_flag() {
        // Missed in_outage flag but previous heartbeat is stale
        let d = evaluate_heartbeat(200, 0, false, 90);
        assert_eq!(
            d,
            HeartbeatDecision::Recovered {
                outage_duration_secs: 200
            }
        );
    }

    #[test]
    fn heartbeat_not_recovered_at_exact_timeout_without_flag() {
        let d = evaluate_heartbeat(90, 0, false, 90);
        assert_eq!(d, HeartbeatDecision::Recorded);
    }

    // --- matrix-style scenarios (timeline) ---

    #[test]
    fn timeline_outage_debounce_realert_then_recover() {
        let timeout = 90;
        let debounce = 300;
        let mut last_hb = 0_u64;
        let mut last_alert: Option<u64> = None;
        #[allow(unused_assignments)]
        let mut in_outage = false;

        // t=50: still healthy
        assert!(matches!(
            evaluate_check(50, last_hb, last_alert, timeout, debounce),
            CheckDecision::Healthy { .. }
        ));

        // t=100: first outage alert
        match evaluate_check(100, last_hb, last_alert, timeout, debounce) {
            CheckDecision::AlertOutage {
                secs_since_heartbeat,
            } => {
                assert_eq!(secs_since_heartbeat, 100);
                last_alert = Some(100);
                in_outage = true;
            }
            other => panic!("expected first alert, got {other:?}"),
        }

        // t=250: still down, debouncing — last_hb unchanged (no false reset)
        match evaluate_check(250, last_hb, last_alert, timeout, debounce) {
            CheckDecision::StillDown {
                secs_since_heartbeat,
                secs_since_alert,
                secs_until_next_alert,
            } => {
                assert_eq!(secs_since_heartbeat, 250);
                assert_eq!(secs_since_alert, 150);
                assert_eq!(secs_until_next_alert, 150);
            }
            other => panic!("expected StillDown, got {other:?}"),
        }

        // t=400: re-alert; true downtime is 400s, not ~300s
        match evaluate_check(400, last_hb, last_alert, timeout, debounce) {
            CheckDecision::AlertOutage {
                secs_since_heartbeat,
            } => {
                assert_eq!(secs_since_heartbeat, 400);
                last_alert = Some(400);
            }
            other => panic!("expected re-alert, got {other:?}"),
        }

        // t=420: heartbeat returns → recovered
        match evaluate_heartbeat(420, last_hb, in_outage, timeout) {
            HeartbeatDecision::Recovered {
                outage_duration_secs,
            } => {
                assert_eq!(outage_duration_secs, 420);
                last_hb = 420;
                in_outage = false;
            }
            other => panic!("expected Recovered, got {other:?}"),
        }

        // t=430: healthy again
        assert!(matches!(
            evaluate_check(430, last_hb, last_alert, timeout, debounce),
            CheckDecision::Healthy {
                secs_since_heartbeat: 10
            }
        ));
        assert!(!in_outage);
    }
}
