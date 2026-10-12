use crate::{docker::Docker, github::RunnerRelease};
use anyhow::Result;
use serde_json::{Value, json};
use std::sync::LazyLock;

static MANIFEST: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../runner/version.json")).expect("valid runner manifest")
});
pub fn runner_version() -> &'static str {
    MANIFEST["version"].as_str().expect("runner version")
}
fn version(s: &str) -> Option<Vec<u64>> {
    let numbers: Option<Vec<u64>> = s
        .trim_start_matches('v')
        .split('.')
        .map(|s| s.parse().ok())
        .collect();
    numbers.filter(|v| v.len() == 3)
}
pub fn version_check(
    image: &str,
    installed: Option<&str>,
    latest: Option<&RunnerRelease>,
) -> Value {
    let mut check =
        json!({"check":format!("runner-version:{image}"),"ok":true,"installed":installed});
    match (installed.and_then(version), latest) {
        (Some(current), Some(latest)) => {
            check["latest"] = json!(latest.tag_name.trim_start_matches('v'));
            if let Some(new) = version(&latest.tag_name) {
                if current < new {
                    check["update_available"] = json!(true);
                    check["warning"] = json!(
                        "Rebuild or pull an updated runner image, then use runnerctl upgrade. Earlier releases may have an earlier deadline."
                    );
                    if let Ok(published) = humantime::parse_rfc3339(&latest.published_at) {
                        let deadline = published + std::time::Duration::from_secs(30 * 86400);
                        check["deadline"] = json!(humantime::format_rfc3339(deadline).to_string());
                        check["deadline_basis"] =
                            json!("latest upstream release; this is an upper bound");
                        if std::time::SystemTime::now() >= deadline {
                            check["ok"] = json!(false);
                            check["error"] = json!(
                                "Runner is past the upstream update deadline and may no longer receive jobs"
                            );
                        }
                    }
                }
            }
        }
        (None, _) => {
            check["warning"] =
                json!("Custom image must set io.runnerctl.runner.version to enable update checks");
        }
        (_, None) => {
            check["warning"] =
                json!("Upstream version unavailable; check runner update requirements manually");
        }
    }
    check
}
pub async fn pull(image: Option<String>) -> Result<Value> {
    let image = image.unwrap_or_else(crate::config::default_image);
    crate::config::validate_image(&image)?;
    Docker::pull(&image).await?;
    Ok(
        json!({"pulled":image,"runner_version":Docker::image_version(&image).await?,"bundled_runner_version":runner_version()}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn update_checks_distinguish_missing_current_and_expired_images() {
        let expired = RunnerRelease {
            tag_name: "v2.338.0".into(),
            published_at: "2020-01-01T00:00:00Z".into(),
        };
        assert_eq!(version_check("custom:1", None, Some(&expired))["ok"], true);
        assert_eq!(
            version_check("runner:1", Some("2.338.0"), Some(&expired))["ok"],
            true
        );
        assert_eq!(
            version_check("runner:1", Some("2.337.0"), Some(&expired))["ok"],
            false
        );
        assert_eq!(version_check("runner:1", Some("2.338.0"), None)["ok"], true);
        let fresh = RunnerRelease {
            published_at: humantime::format_rfc3339(std::time::SystemTime::now()).to_string(),
            ..expired
        };
        assert_eq!(
            version_check("runner:1", Some("2.337.0"), Some(&fresh))["update_available"],
            true
        );
        assert_eq!(
            version_check("runner:1", Some("2.337.0"), Some(&fresh))["ok"],
            true
        );
    }
}
