//! Checks GitHub for a newer release. Only reads the public releases API; nothing is downloaded.

use anyhow::{Result, anyhow};

pub const REPO: &str = "aliforever/sonar-deck";

/// The latest published release: (version without the leading "v", release page URL).
pub fn latest() -> Result<(String, String)> {
    let v: serde_json::Value = ureq::get(&format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("User-Agent", concat!("sonar-deck/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/vnd.github+json")
        .call()?
        .body_mut()
        .read_json()?;
    let tag = v["tag_name"].as_str().ok_or_else(|| anyhow!("release has no tag"))?;
    let url = v["html_url"].as_str().unwrap_or_default();
    Ok((tag.trim_start_matches('v').to_string(), url.to_string()))
}

/// Compares dotted numeric versions ("0.2.0" > "0.1.10").
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| v.split(['.', '-']).map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    parse(candidate) > parse(current)
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn version_order() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
    }
}
