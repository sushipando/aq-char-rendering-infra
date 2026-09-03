//! WebP frame encoding through the same pinned `cwebp` binary and flags the
//! Python worker uses. Keeping cwebp as a subprocess isolates the compositor
//! comparison from any codec change; direct libwebp linking is a separate,
//! later benchmark.

use std::path::Path;

use tokio::process::Command;

use crate::error::ComposeError;

/// Format a quality like Python's `f"{value:g}"`: 85.0 -> "85", 85.5 -> "85.5".
fn format_quality(quality: f64) -> String {
    if quality.fract() == 0.0 {
        format!("{}", quality as i64)
    } else {
        format!("{}", quality)
    }
}

pub async fn encode_webp(
    cwebp: &Path,
    quality: f64,
    method: i64,
    lossless: bool,
    input_png: &Path,
    output_webp: &Path,
) -> Result<(), ComposeError> {
    let mut command = Command::new(cwebp);
    command.arg("-quiet");
    if lossless {
        command.arg("-lossless").arg("1");
    }
    command
        .arg("-q")
        .arg(format_quality(quality))
        .arg("-alpha_q")
        .arg("100")
        .arg("-m")
        .arg(method.to_string())
        .arg(input_png)
        .arg("-o")
        .arg(output_webp);
    let output = command
        .output()
        .await
        .map_err(|error| ComposeError::Encode(format!("cannot launch {cwebp:?}: {error}")))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if detail.is_empty() {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        } else {
            detail
        };
        let truncated: String = detail.chars().take(2000).collect();
        return Err(ComposeError::Encode(truncated));
    }
    match output_webp.metadata() {
        Ok(metadata) if metadata.len() > 0 => Ok(()),
        _ => Err(ComposeError::Encode(
            "cwebp produced an empty frame".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_formatting_matches_python_g() {
        assert_eq!(format_quality(85.0), "85");
        assert_eq!(format_quality(85.5), "85.5");
        assert_eq!(format_quality(80.25), "80.25");
        assert_eq!(format_quality(100.0), "100");
        assert_eq!(format_quality(0.0), "0");
    }
}
