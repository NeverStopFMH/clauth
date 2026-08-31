//! Bearer token gating the embedded web dashboard's write endpoints. Read
//! endpoints (status, usage, incidents) need none of this — the dashboard
//! never carries a token/secret/key of its own (same invariant `status.json`
//! already holds), so the only thing worth gating is who can trigger a
//! mutation (switch account, edit config, delete a profile) from another
//! local process without you asking.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::profile::{atomic_write_600, clauth_dir};

const TOKEN_FILE: &str = "web_token";
const TOKEN_BYTES: usize = 32;

fn token_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join(TOKEN_FILE))
}

/// The dashboard's write-auth token, generating and persisting one on first
/// use. Stable across restarts (never regenerated on its own) so a bookmarked
/// `clauth web url` link keeps working indefinitely.
pub(crate) fn load_or_create_token() -> Result<String> {
    let path = token_path()?;
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let token = generate_token()?;
    atomic_write_600(&path, &token)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(token)
}

/// 32 random bytes, hex-encoded (64 chars) — long enough that guessing it is
/// not a realistic attack, short enough to paste into a URL query param.
fn generate_token() -> Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("CSPRNG failure: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Whether an incoming request's raw `Authorization` header value (if any)
/// carries the expected bearer token. Pure so the 401 gate is unit-testable
/// without a real HTTP round trip.
pub(crate) fn check_bearer(header: Option<&str>, expected: &str) -> bool {
    header
        .and_then(|h| h.strip_prefix("Bearer "))
        .is_some_and(|token| token == expected)
}

#[cfg(test)]
#[path = "../../tests/inline/web_auth.rs"]
mod tests;
