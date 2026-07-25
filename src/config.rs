//! File-based config (`$XDG_CONFIG_HOME/record-ui/config.toml`) with SPEC defaults.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ports::{absolutize_path, default_videos_dir, Paths, PortError};

/// User-facing configuration with SPEC v1 defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Where recording files are written (absolute after load/normalize).
    pub output_dir: PathBuf,
    /// System audio off unless toggled / CLI override.
    pub audio_default: bool,
    /// Copy absolute path via clipboard on success.
    pub copy_path: bool,
    /// Desktop notifications.
    pub notify: bool,
    /// “Recording started” when start has no GUI client.
    pub notify_on_start_cli: bool,
    /// Wait after SIGINT before SIGTERM escalation.
    pub stop_timeout_ms: u64,
    /// Wait after SIGTERM before hard failure / nuclear.
    pub stop_term_timeout_ms: u64,
    /// Wayland output for one-monitor fullscreen (`wf-recorder -o`).
    /// Required when more than one output is present (no multi-head focus auto-pick).
    /// Empty/None: sole output if inventory length is 1; else start fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fullscreen_output: Option<String>,
    /// One-monitor FPS for `wf-recorder -r`.
    ///
    /// - `None` / absent: Auto for CLI resolve (no `-r`); GUI first-run defaults to **native**.
    /// - `Some(0)`: remembered **Auto** (GUI sticky; resolve treats 0 as Auto).
    /// - `Some(n)` with `n > 0`: fixed rate.
    ///
    /// CLI/IPC `--fps` / `fps` overrides this for a single start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub one_fps: Option<u32>,
}

impl Default for Config {
    fn default() -> Self {
        // Prefer absolute `~/Videos` when HOME is set; otherwise a documented relative
        // placeholder. Prefer `Config::with_defaults` / `with_home` / `load` in production.
        let output_dir = env::var_os("HOME")
            .map(PathBuf::from)
            .map(|h| h.join("Videos"))
            .unwrap_or_else(|| PathBuf::from("Videos"));
        Self {
            output_dir,
            audio_default: false,
            copy_path: true,
            notify: true,
            notify_on_start_cli: true,
            stop_timeout_ms: 5000,
            stop_term_timeout_ms: 2000,
            fullscreen_output: None,
            one_fps: None,
        }
    }
}

impl Config {
    /// Defaults using the Paths port for XDG Videos (or `~/Videos`), then absolutize.
    pub fn with_defaults(paths: &dyn Paths) -> Self {
        let mut cfg = Self {
            output_dir: paths.output_dir(),
            ..Self::default_flags()
        };
        cfg.normalize_paths(None);
        cfg
    }

    /// Defaults from home + optional `xdg-user-dir VIDEOS` result.
    pub fn with_home(home: &Path, xdg_videos: Option<&Path>) -> Self {
        let mut cfg = Self {
            output_dir: default_videos_dir(home, xdg_videos),
            ..Self::default_flags()
        };
        cfg.normalize_paths(Some(home));
        cfg
    }

    /// Boolean/timeout defaults without inventing an output path twice.
    fn default_flags() -> Self {
        Self {
            output_dir: PathBuf::new(),
            audio_default: false,
            copy_path: true,
            notify: true,
            notify_on_start_cli: true,
            stop_timeout_ms: 5000,
            stop_term_timeout_ms: 2000,
            fullscreen_output: None,
            one_fps: None,
        }
    }

    /// Effective fullscreen `-o` override, if configured non-empty.
    pub fn fullscreen_output_override(&self) -> Option<&str> {
        self.fullscreen_output
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// Raw config `one_fps` value (`None` / `Some(0)` / `Some(n > 0)`).
    ///
    /// Does not interpret Auto: callers should pass this to [`crate::recorder::resolve_one_fps`],
    /// which treats both `None` and `0` as Auto (omit `-r`). GUI load maps `None` → native
    /// first-run and `Some(0)` → sticky Auto (see field docs / DUAL-MONITOR §5.3).
    pub fn one_fps_override(&self) -> Option<u32> {
        self.one_fps
    }

    /// Write this config as TOML to `path` (creates parent dirs).
    ///
    /// Used by the GUI to persist `fullscreen_output` / `one_fps` on picker change
    /// (DUAL-MONITOR §5.3). Other keys are rewritten from the in-memory snapshot.
    ///
    /// Writes via a sibling temp file then `rename` so a crash mid-write cannot
    /// leave a truncated `config.toml`.
    pub fn save_to_path(&self, path: &Path) -> Result<(), PortError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                PortError::Io(format!("create config dir {}: {e}", parent.display()))
            })?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| PortError::Io(format!("serialize config: {e}")))?;
        // Sibling temp in the same directory so rename is atomic on the same FS.
        let tmp = {
            let mut t = path.as_os_str().to_os_string();
            t.push(".tmp");
            PathBuf::from(t)
        };
        fs::write(&tmp, &text)
            .map_err(|e| PortError::Io(format!("write config temp {}: {e}", tmp.display())))?;
        fs::rename(&tmp, path).map_err(|e| {
            // Best-effort cleanup of the temp on rename failure.
            let _ = fs::remove_file(&tmp);
            PortError::Io(format!(
                "rename config temp {} → {}: {e}",
                tmp.display(),
                path.display()
            ))
        })?;
        Ok(())
    }

    /// Save to Paths port config location.
    pub fn save(&self, paths: &dyn Paths) -> Result<(), PortError> {
        self.save_to_path(&paths.config_path())
    }

    /// Load TOML from `path`. Missing file → defaults. Partial TOML merges over defaults.
    pub fn load_from_path(path: &Path, defaults: Config) -> Result<Config, PortError> {
        if !path.exists() {
            return Ok(defaults);
        }
        let text = fs::read_to_string(path)
            .map_err(|e| PortError::Io(format!("read config {}: {e}", path.display())))?;
        Self::parse_toml(&text, defaults)
    }

    /// Load from Paths port config location.
    pub fn load(paths: &dyn Paths) -> Result<Config, PortError> {
        let defaults = Config::with_defaults(paths);
        Self::load_from_path(&paths.config_path(), defaults)
    }

    /// Parse TOML text, filling missing keys from `defaults`. Absolutizes `output_dir`.
    pub fn parse_toml(text: &str, defaults: Config) -> Result<Config, PortError> {
        let partial: PartialConfig =
            toml::from_str(text).map_err(|e| PortError::Io(format!("parse config: {e}")))?;
        let mut cfg = partial.merge(defaults);
        cfg.normalize_paths(None);
        Ok(cfg)
    }

    /// Ensure `output_dir` is absolute (SPEC: hooks copy absolute path).
    pub fn normalize_paths(&mut self, base: Option<&Path>) {
        let cwd = base
            .map(Path::to_path_buf)
            .or_else(|| env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        if self.output_dir.as_os_str().is_empty() {
            self.output_dir = cwd.join("Videos");
        } else {
            self.output_dir = absolutize_path(&self.output_dir, &cwd);
        }
    }

    pub fn stop_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.stop_timeout_ms)
    }

    pub fn stop_term_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.stop_term_timeout_ms)
    }
}

/// Optional keys only — missing fields keep defaults.
#[derive(Debug, Default, Deserialize)]
struct PartialConfig {
    output_dir: Option<PathBuf>,
    audio_default: Option<bool>,
    copy_path: Option<bool>,
    notify: Option<bool>,
    notify_on_start_cli: Option<bool>,
    stop_timeout_ms: Option<u64>,
    stop_term_timeout_ms: Option<u64>,
    fullscreen_output: Option<String>,
    one_fps: Option<u32>,
}

impl PartialConfig {
    fn merge(self, mut base: Config) -> Config {
        if let Some(v) = self.output_dir {
            base.output_dir = v;
        }
        if let Some(v) = self.audio_default {
            base.audio_default = v;
        }
        if let Some(v) = self.copy_path {
            base.copy_path = v;
        }
        if let Some(v) = self.notify {
            base.notify = v;
        }
        if let Some(v) = self.notify_on_start_cli {
            base.notify_on_start_cli = v;
        }
        if let Some(v) = self.stop_timeout_ms {
            base.stop_timeout_ms = v;
        }
        if let Some(v) = self.stop_term_timeout_ms {
            base.stop_term_timeout_ms = v;
        }
        if let Some(v) = self.fullscreen_output {
            base.fullscreen_output = Some(v);
        }
        if let Some(v) = self.one_fps {
            base.one_fps = Some(v);
        }
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn u17_config_defaults() {
        let home = PathBuf::from("/home/test");
        let cfg = Config::with_home(&home, None);
        assert_eq!(cfg.output_dir, PathBuf::from("/home/test/Videos"));
        assert!(!cfg.audio_default);
        assert!(cfg.copy_path);
        assert!(cfg.notify);
        assert!(cfg.notify_on_start_cli);
        assert_eq!(cfg.stop_timeout_ms, 5000);
        assert_eq!(cfg.stop_term_timeout_ms, 2000);
        assert!(cfg.fullscreen_output.is_none());
        assert!(cfg.one_fps.is_none());
    }

    #[test]
    fn u17_config_overrides_from_toml() {
        let defaults = Config::with_home(Path::new("/home/test"), None);
        let toml = r#"
output_dir = "/tmp/clips"
audio_default = true
copy_path = false
notify = false
notify_on_start_cli = false
stop_timeout_ms = 1000
stop_term_timeout_ms = 500
fullscreen_output = "HDMI-A-1"
one_fps = 144
"#;
        let cfg = Config::parse_toml(toml, defaults).unwrap();
        assert_eq!(cfg.output_dir, PathBuf::from("/tmp/clips"));
        assert!(cfg.audio_default);
        assert!(!cfg.copy_path);
        assert!(!cfg.notify);
        assert!(!cfg.notify_on_start_cli);
        assert_eq!(cfg.stop_timeout_ms, 1000);
        assert_eq!(cfg.stop_term_timeout_ms, 500);
        assert_eq!(cfg.fullscreen_output.as_deref(), Some("HDMI-A-1"));
        assert_eq!(cfg.one_fps, Some(144));
        assert_eq!(cfg.one_fps_override(), Some(144));
    }

    #[test]
    fn u17_partial_toml_keeps_defaults() {
        let defaults = Config::with_home(Path::new("/home/test"), None);
        let cfg = Config::parse_toml("audio_default = true\n", defaults.clone()).unwrap();
        assert!(cfg.audio_default);
        assert_eq!(cfg.output_dir, defaults.output_dir);
        assert_eq!(cfg.stop_timeout_ms, 5000);
        assert!(cfg.one_fps.is_none());
    }

    #[test]
    fn u17_xdg_user_dir_videos() {
        let cfg = Config::with_home(
            Path::new("/home/test"),
            Some(Path::new("/mnt/media/Videos")),
        );
        assert_eq!(cfg.output_dir, PathBuf::from("/mnt/media/Videos"));
    }

    #[test]
    fn relative_output_dir_is_absolutized() {
        let defaults = Config::with_home(Path::new("/home/test"), None);
        let cfg = Config::parse_toml("output_dir = \"clips\"\n", defaults).unwrap();
        assert!(cfg.output_dir.is_absolute(), "{:?}", cfg.output_dir);
        assert!(cfg.output_dir.ends_with("clips"));
    }

    fn temp_cfg_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "record-ui-cfg-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn save_to_path_roundtrip_one_pickers() {
        let dir = temp_cfg_dir();
        // Nested path: save must create parent dirs.
        let path = dir.join("nested").join("record-ui").join("config.toml");

        let mut cfg = Config::with_home(Path::new("/home/test"), None);
        cfg.audio_default = true;
        cfg.copy_path = false;
        cfg.fullscreen_output = Some("HDMI-A-1".into());
        cfg.one_fps = Some(144);
        cfg.save_to_path(&path).unwrap();
        assert!(path.parent().unwrap().is_dir());

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("fullscreen_output"), "{text}");
        assert!(text.contains("HDMI-A-1"), "{text}");
        assert!(text.contains("one_fps"), "{text}");
        assert!(text.contains("144"), "{text}");
        // No leftover temp after successful rename (path + ".tmp").
        let tmp = {
            let mut t = path.as_os_str().to_os_string();
            t.push(".tmp");
            PathBuf::from(t)
        };
        assert!(!tmp.exists(), "temp file should be renamed away");

        let loaded =
            Config::load_from_path(&path, Config::with_home(Path::new("/home/test"), None))
                .unwrap();
        assert_eq!(loaded.fullscreen_output.as_deref(), Some("HDMI-A-1"));
        assert_eq!(loaded.one_fps, Some(144));
        // Non-picker fields survive load→mutate→save.
        assert!(loaded.audio_default);
        assert!(!loaded.copy_path);

        // Sticky Auto: one_fps = 0 is written and reloaded.
        cfg.one_fps = Some(0);
        cfg.save_to_path(&path).unwrap();
        let text2 = fs::read_to_string(&path).unwrap();
        assert!(
            text2
                .lines()
                .any(|l| l.contains("one_fps") && l.contains('0')),
            "one_fps = 0 should be written for sticky Auto: {text2}"
        );
        let loaded2 =
            Config::load_from_path(&path, Config::with_home(Path::new("/home/test"), None))
                .unwrap();
        assert_eq!(loaded2.one_fps, Some(0));
        assert_eq!(loaded2.fullscreen_output.as_deref(), Some("HDMI-A-1"));
        assert!(loaded2.audio_default);
        assert!(!loaded2.copy_path);

        // Unset one_fps / fullscreen_output (None) omits those keys.
        cfg.one_fps = None;
        cfg.fullscreen_output = None;
        cfg.save_to_path(&path).unwrap();
        let text3 = fs::read_to_string(&path).unwrap();
        assert!(
            !text3.lines().any(|l| l.trim_start().starts_with("one_fps")),
            "one_fps omitted when None: {text3}"
        );
        assert!(
            !text3
                .lines()
                .any(|l| l.trim_start().starts_with("fullscreen_output")),
            "fullscreen_output omitted when None: {text3}"
        );
        assert!(text3.contains("audio_default"), "{text3}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_to_path_creates_missing_parent_dirs() {
        let dir = temp_cfg_dir();
        let path = dir.join("a").join("b").join("config.toml");
        let cfg = Config::with_home(Path::new("/home/test"), None);
        cfg.save_to_path(&path).unwrap();
        assert!(path.is_file());
        let _ = fs::remove_dir_all(&dir);
    }
}
