//! Restart policy logic and name generation.

use super::BoxRecord;

/// Check if a process is alive by sending signal 0.
pub(super) fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Determine if a box should be automatically restarted based on its restart policy.
///
/// Policies:
/// - `"no"` — never restart
/// - `"always"` — always restart (even if stopped by user, but we only call this for dead boxes)
/// - `"on-failure"` — restart only if the box died unexpectedly (not stopped by user)
/// - `"on-failure:N"` — like on-failure, but at most N times
/// - `"unless-stopped"` — restart unless the user explicitly stopped it
pub(crate) fn should_restart(record: &BoxRecord) -> bool {
    let policy = record.restart_policy.as_str();

    // Parse "on-failure:N" format
    if let Some(max_str) = policy.strip_prefix("on-failure:") {
        if let Ok(max) = max_str.parse::<u32>() {
            return !record.stopped_by_user && record.restart_count < max;
        }
        // Malformed "on-failure:..." — treat as no restart
        return false;
    }

    match policy {
        "always" => true,
        "on-failure" => {
            if record.stopped_by_user {
                return false;
            }
            if record.max_restart_count > 0 {
                record.restart_count < record.max_restart_count
            } else {
                true
            }
        }
        "unless-stopped" => !record.stopped_by_user,
        _ => false, // "no" or unknown
    }
}

/// Validate a restart policy string.
///
/// Returns `Ok(())` if valid, or an error message describing the problem.
/// Valid values: "no", "always", "on-failure", "on-failure:N", "unless-stopped".
pub fn validate_restart_policy(policy: &str) -> Result<(), String> {
    match policy {
        "no" | "always" | "on-failure" | "unless-stopped" => Ok(()),
        _ if policy.starts_with("on-failure:") => {
            let max_str = &policy["on-failure:".len()..];
            max_str
                .parse::<u32>()
                .map(|_| ())
                .map_err(|_| format!(
                    "Invalid restart policy '{policy}': expected 'on-failure:N' where N is a positive integer"
                ))
        }
        _ => Err(format!(
            "Invalid restart policy '{policy}': must be one of: no, always, on-failure, on-failure:N, unless-stopped"
        )),
    }
}

/// Parse a restart policy string and return (base_policy, max_restart_count).
///
/// - `"on-failure:5"` → `("on-failure", 5)`
/// - `"always"` → `("always", 0)`
/// - `"no"` → `("no", 0)`
pub fn parse_restart_policy(policy: &str) -> Result<(String, u32), String> {
    validate_restart_policy(policy)?;

    if let Some(max_str) = policy.strip_prefix("on-failure:") {
        let max = max_str.parse::<u32>().unwrap(); // safe: validated above
        Ok(("on-failure".to_string(), max))
    } else {
        Ok((policy.to_string(), 0))
    }
}

/// Adjectives for random name generation.
pub(super) const ADJECTIVES: &[&str] = &[
    "bold", "calm", "cool", "dark", "fast", "glad", "keen", "kind", "loud", "mild", "neat", "pale",
    "pure", "rare", "safe", "slim", "soft", "tall", "tiny", "vast", "warm", "wise", "zen", "agile",
    "brave", "eager", "happy", "lucid", "noble", "quick", "sharp", "vivid",
];

/// Nouns (notable computer scientists) for random name generation.
pub(super) const NOUNS: &[&str] = &[
    "turing",
    "hopper",
    "lovelace",
    "dijkstra",
    "knuth",
    "ritchie",
    "thompson",
    "torvalds",
    "wozniak",
    "cerf",
    "berners",
    "mccarthy",
    "backus",
    "kay",
    "lamport",
    "hoare",
    "church",
    "neumann",
    "shannon",
    "boole",
    "babbage",
    "hamilton",
    "liskov",
    "wing",
    "rivest",
    "shamir",
    "diffie",
    "hellman",
    "stallman",
    "pike",
    "kernighan",
    "stroustrup",
];

/// Generate a random Docker-style name (adjective_noun).
pub fn generate_name() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let adj = ADJECTIVES[rng.gen_range(0..ADJECTIVES.len())];
    let noun = NOUNS[rng.gen_range(0..NOUNS.len())];
    format!("{adj}_{noun}")
}
