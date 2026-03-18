// Path encoder: converts filesystem paths to Claude Code's encoding format

use std::path::Path;

/// Fehler beim Encoding von Pfaden.
#[derive(Debug, thiserror::Error)]
pub enum EncoderError {
    #[error("path must be absolute, got: {0}")]
    RelativePath(String),
}

/// Encodes a filesystem path to Claude Code's internal format.
///
/// Replaces all non-alphanumeric characters (except `-`) with `-`.
/// This matches Claude Code's actual encoding behavior (verified against
/// real `~/.claude/projects/` data: `/`, `.`, `_` and other special
/// characters all become `-`).
/// Rejects relative paths.
pub fn encode(path: &Path) -> Result<String, EncoderError> {
    let s = path.to_string_lossy();

    if !s.starts_with('/') {
        return Err(EncoderError::RelativePath(s.to_string()));
    }

    // Trailing Slash entfernen, aber "/" selbst beibehalten
    let s = if s.len() > 1 {
        s.strip_suffix('/').unwrap_or(&s)
    } else {
        &s
    };

    Ok(s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn encode_simple_path() {
        assert_eq!(
            encode(Path::new("/home/user/project")).unwrap(),
            "-home-user-project"
        );
    }

    #[test]
    fn encode_path_with_dots() {
        assert_eq!(
            encode(Path::new("/home/user/github.com/repo")).unwrap(),
            "-home-user-github-com-repo"
        );
    }

    #[test]
    fn encode_double_hyphen_from_dot_slash() {
        // /.config → --config (/ becomes -, . becomes -)
        assert_eq!(
            encode(Path::new("/home/user/dotfiles/.config/hypr")).unwrap(),
            "-home-user-dotfiles--config-hypr"
        );
    }

    #[test]
    fn encode_trailing_slash_stripped() {
        assert_eq!(
            encode(Path::new("/home/user/project/")).unwrap(),
            "-home-user-project"
        );
    }

    #[test]
    fn encode_rejects_relative_path() {
        assert!(encode(Path::new("relative/path")).is_err());
    }

    #[test]
    fn encode_root_path() {
        assert_eq!(encode(Path::new("/")).unwrap(), "-");
    }

    // Verified against real system data
    #[test]
    fn encode_real_path_dotfiles_atuin() {
        assert_eq!(
            encode(Path::new("/home/sebflu/dotfiles/atuin")).unwrap(),
            "-home-sebflu-dotfiles-atuin"
        );
    }

    #[test]
    fn encode_real_path_with_nested_dots() {
        // underscores also become hyphens (verified against real data)
        assert_eq!(
            encode(Path::new("/home/sebflu/dotfiles/hypr_env4/.config/hypr")).unwrap(),
            "-home-sebflu-dotfiles-hypr-env4--config-hypr"
        );
    }

    #[test]
    fn encode_underscores_replaced() {
        // Verified: ~/.claude/projects/ contains -home-sebflu-scratchpad-research-neovim-excalidraw-integration
        // NOT -home-sebflu-scratchpad-research-neovim_excalidraw_integration
        assert_eq!(
            encode(Path::new(
                "/home/sebflu/scratchpad/research/neovim_excalidraw_integration"
            ))
            .unwrap(),
            "-home-sebflu-scratchpad-research-neovim-excalidraw-integration"
        );
    }
}
