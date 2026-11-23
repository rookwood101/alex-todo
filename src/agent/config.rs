//! Process configuration: the mode tool allowlists and the exact argv used
//! to launch Pi.
//!
//! All Pi CLI syntax lives here so board/power differences are one enum and
//! the tests can assert the complete argument vectors from 005.

use std::path::{Path, PathBuf};

pub use super::PiProfile;

/// Default Pi shim name; resolves through PATH on Windows.
pub const DEFAULT_EXECUTABLE: &str = "pi";
/// Tool name exposed by the rustboard MCP server when registered directly.
pub const BOARD_TOOL_PATTERN: &str = "mcp__rustboard__*";

impl PiProfile {
    /// Human label for the UI.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Board => "board",
            Self::Power => "power",
        }
    }

    /// The exact profile flags from 005, placed after the model arguments.
    ///
    /// Board mode is shell-free: `--no-builtin-tools` plus only the board MCP
    /// tools. Power mode adds the opt-in OS tools.
    pub const fn tool_args(self) -> &'static [&'static str] {
        match self {
            Self::Board => &[
                "--no-builtin-tools",
                "--tools",
                BOARD_TOOL_PATTERN,
                "--approve",
            ],
            Self::Power => &[
                "--tools",
                "read,bash,powershell,edit,write,grep,find,ls,mcp__rustboard__*",
                "--approve",
            ],
        }
    }
}

/// Everything needed to launch one Pi child.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PiConfig {
    /// Pi executable, normally `pi`. On Windows this is resolved by `cmd.exe`.
    pub executable: String,
    /// Pi model id override. None (the default) leaves the choice to Pi's own
    /// settings (`defaultProvider`/`defaultModel` in ~/.pi/agent/settings.json).
    pub model: Option<String>,
    /// Pi provider id override; used only together with `model`.
    pub provider: Option<String>,
    /// Working directory for Pi; also where `.pi/mcp.json` lives.
    pub board_home: PathBuf,
    /// Tool profile.
    pub profile: PiProfile,
    /// Extra arguments appended after the profile flags (tests or user flags).
    pub extra_args: Vec<String>,
    /// Launcher program. Defaults to `cmd.exe` on Windows and the Pi
    /// executable elsewhere. This is the status command.
    pub command: Option<String>,
    /// Launcher arguments placed before the generated Pi command line.
    /// Defaults to `[/d, /s, /c]` on Windows. Tests point this at a fake
    /// child launcher.
    pub base_args: Option<Vec<String>>,
    /// Keep Pi sessions in this folder (None runs Pi with `--no-session`).
    pub session_dir: Option<PathBuf>,
    /// With a session folder: resume the latest session (`--continue`)
    /// instead of starting a fresh one.
    pub continue_session: bool,
}

impl PiConfig {
    /// Board defaults with the given home and profile.
    pub fn new(board_home: impl Into<PathBuf>, profile: PiProfile) -> Self {
        Self {
            executable: DEFAULT_EXECUTABLE.to_owned(),
            model: None,
            provider: None,
            board_home: board_home.into(),
            profile,
            extra_args: Vec::new(),
            command: None,
            base_args: None,
            session_dir: None,
            continue_session: false,
        }
    }

    /// Arguments after the executable: protocol, model and tool profile.
    pub fn pi_args(&self) -> Vec<String> {
        self.pi_args_for(self.profile)
    }

    /// Profile-specific Pi arguments. Kept separate so a restart can switch
    /// modes without mutating the config.
    pub fn pi_args_for(&self, profile: PiProfile) -> Vec<String> {
        let mut args = vec!["--mode".to_owned(), "rpc".to_owned()];
        match &self.session_dir {
            Some(dir) => {
                args.push("--session-dir".to_owned());
                args.push(dir.display().to_string());
                if self.continue_session {
                    args.push("--continue".to_owned());
                }
            }
            None => args.push("--no-session".to_owned()),
        }
        if let (Some(provider), Some(model)) = (&self.provider, &self.model) {
            args.extend([
                "--provider".to_owned(),
                provider.clone(),
                "--model".to_owned(),
                model.clone(),
            ]);
        }
        args.extend(profile.tool_args().iter().map(|arg| (*arg).to_owned()));
        args.extend(self.extra_args.iter().cloned());
        args
    }

    /// Complete launch plan for the configured profile.
    pub fn launch_plan(&self) -> LaunchPlan {
        self.launch_plan_for(self.profile)
    }

    #[cfg(windows)]
    /// Complete launch plan, including the `cmd.exe` wrapper and raw command
    /// line used with `CommandExt::raw_arg`.
    pub fn launch_plan_for(&self, profile: PiProfile) -> LaunchPlan {
        let program = self.command.clone().unwrap_or_else(|| "cmd.exe".to_owned());
        let args = self
            .base_args
            .clone()
            .unwrap_or_else(|| vec!["/d".to_owned(), "/s".to_owned(), "/c".to_owned()]);

        let mut argv = Vec::with_capacity(1 + 8 + self.extra_args.len());
        argv.push(self.executable.clone());
        argv.extend(self.pi_args_for(profile));
        let inner = quote_windows_argv(&argv);

        let is_cmd = Path::new(&program)
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.eq_ignore_ascii_case("cmd.exe") || name.eq_ignore_ascii_case("cmd"))
            .unwrap_or(false);
        let has_slash_s = args.iter().any(|arg| arg.eq_ignore_ascii_case("/s"));
        // `cmd /s /c` strips one outer pair of quotes. Wrapping the whole
        // command keeps a quoted executable path (this repo lives under a path
        // with spaces) intact while still allowing quoted extra arguments.
        let raw_command_line = if is_cmd && has_slash_s {
            format!("\"{inner}\"")
        } else {
            inner
        };

        LaunchPlan {
            program,
            args,
            raw_command_line: Some(raw_command_line),
        }
    }

    #[cfg(not(windows))]
    /// Direct spawn plan for non-Windows hosts.
    pub fn launch_plan_for(&self, profile: PiProfile) -> LaunchPlan {
        let program = self
            .command
            .clone()
            .unwrap_or_else(|| self.executable.clone());
        let mut args = self.base_args.clone().unwrap_or_default();
        if self.command.is_some() {
            args.push(self.executable.clone());
        }
        args.extend(self.pi_args_for(profile));
        LaunchPlan {
            program,
            args,
            raw_command_line: None,
        }
    }
}

/// Find an executable on PATH, honouring PATHEXT on Windows (pi.cmd, pi.exe).
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
            .split(';')
            .filter(|ext| !ext.is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&path).find_map(|dir| {
        extensions.iter().find_map(|ext| {
            let candidate = dir.join(format!("{name}{ext}"));
            candidate.is_file().then_some(candidate)
        })
    })
}

/// Pi's agent folder: `PI_CODING_AGENT_DIR`, else `~/.pi/agent`.
pub fn pi_agent_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("PI_CODING_AGENT_DIR").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|home| !home.is_empty())?;
    Some(PathBuf::from(home).join(".pi").join("agent"))
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Pi's global default (provider, model id) from its settings.json.
pub fn pi_default_model(agent_dir: &Path) -> Option<(String, String)> {
    let settings = read_json(&agent_dir.join("settings.json"))?;
    let provider = settings.get("defaultProvider")?.as_str()?.to_owned();
    let model = settings.get("defaultModel")?.as_str()?.to_owned();
    Some((provider, model))
}

/// A readable message when Pi's google-vertex entry points at a credentials
/// file that cannot be read (for example a WSL path while WSL is stopped).
pub fn vertex_credentials_problem(agent_dir: &Path) -> Option<String> {
    let auth = read_json(&agent_dir.join("auth.json"))?;
    let path = auth
        .get("google-vertex")?
        .get("env")?
        .get("GOOGLE_APPLICATION_CREDENTIALS")?
        .as_str()?;
    if std::fs::metadata(path).is_ok_and(|meta| meta.is_file()) {
        return None;
    }
    Some(format!(
        "Vertex credentials are not readable at {path}. If this is a WSL path, start WSL \
         (open a WSL terminal) and try again, or copy the file to \
         %APPDATA%\\gcloud\\application_default_credentials.json and point \
         GOOGLE_APPLICATION_CREDENTIALS there in {}.",
        agent_dir.join("auth.json").display()
    ))
}

/// A resolved executable and arguments. On Windows `raw_command_line` is
/// appended verbatim with `CommandExt::raw_arg`; elsewhere `args` is used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchPlan {
    /// Program passed to `CreateProcess`.
    pub program: String,
    /// Arguments passed through the normal quoting path.
    pub args: Vec<String>,
    /// Windows-only raw command line (already quoted for `cmd.exe`).
    pub raw_command_line: Option<String>,
}

/// Quote one argument using the MSVC/`CommandLineToArgvW` rules.
pub(crate) fn quote_windows_arg(arg: &str) -> String {
    if arg.is_empty() {
        return "\"\"".to_owned();
    }
    if !arg.contains([' ', '\t', '"']) {
        return arg.to_owned();
    }

    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                // Backslashes before a quote are doubled, then the quote is
                // escaped.
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                quoted.push(ch);
                backslashes = 0;
            }
        }
    }
    // Backslashes before the closing quote are doubled.
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

fn quote_windows_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| quote_windows_arg(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(profile: PiProfile) -> PiConfig {
        PiConfig::new(PathBuf::from(r"C:\board home"), profile)
    }

    #[test]
    fn a_session_folder_replaces_no_session_and_can_continue() {
        let mut config = config(PiProfile::Board);
        config.session_dir = Some(PathBuf::from("sessions"));
        let fresh = config.pi_args();
        assert!(
            fresh
                .windows(2)
                .any(|pair| pair == ["--session-dir", "sessions"])
        );
        assert!(!fresh.contains(&"--no-session".to_owned()));
        assert!(!fresh.contains(&"--continue".to_owned()));
        config.continue_session = true;
        assert!(config.pi_args().contains(&"--continue".to_owned()));
    }

    #[test]
    fn pi_settings_are_read_and_unreadable_vertex_credentials_are_reported() {
        let dir = std::env::temp_dir().join(format!("rustboard-pi-agent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        assert_eq!(pi_default_model(&dir), None);
        assert_eq!(
            vertex_credentials_problem(&dir),
            None,
            "no auth.json, nothing to check"
        );
        std::fs::write(
            dir.join("settings.json"),
            r#"{"defaultProvider":"google-vertex","defaultModel":"gemini-4-carbon"}"#,
        )
        .expect("settings");
        assert_eq!(
            pi_default_model(&dir),
            Some(("google-vertex".to_owned(), "gemini-4-carbon".to_owned()))
        );
        let missing = dir.join("missing-adc.json");
        std::fs::write(
            dir.join("auth.json"),
            serde_json::json!({"google-vertex": {"type": "api_key", "env": {
                "GOOGLE_APPLICATION_CREDENTIALS": missing.display().to_string()
            }}})
            .to_string(),
        )
        .expect("auth");
        let problem = vertex_credentials_problem(&dir).expect("missing file is reported");
        assert!(problem.contains("not readable"), "{problem}");
        std::fs::write(&missing, "{}").expect("adc");
        assert_eq!(vertex_credentials_problem(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn provider_and_model_flags_appear_only_when_both_are_set() {
        let mut config = config(PiProfile::Board);
        assert!(!config.pi_args().contains(&"--model".to_owned()));
        config.model = Some("gemini-4-carbon".to_owned());
        assert!(!config.pi_args().contains(&"--model".to_owned()));
        config.provider = Some("google-vertex".to_owned());
        let args = config.pi_args();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--provider", "google-vertex"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--model", "gemini-4-carbon"])
        );
    }

    #[test]
    fn board_profile_has_exactly_the_shell_free_tool_flags() {
        let config = config(PiProfile::Board);
        assert_eq!(
            config.pi_args(),
            vec![
                "--mode",
                "rpc",
                "--no-session",
                "--no-builtin-tools",
                "--tools",
                "mcp__rustboard__*",
                "--approve",
            ]
        );

        let args = config.pi_args();
        for forbidden in [
            "read",
            "bash",
            "powershell",
            "edit",
            "write",
            "grep",
            "find",
            "ls",
        ] {
            assert!(
                !args.iter().any(|arg| arg == forbidden),
                "board mode must not allow the OS tool {forbidden}"
            );
        }
        assert!(args.iter().any(|arg| arg == "--no-builtin-tools"));
    }

    #[test]
    fn power_profile_adds_the_os_tools() {
        let config = config(PiProfile::Power);
        assert_eq!(
            config.pi_args(),
            vec![
                "--mode",
                "rpc",
                "--no-session",
                "--tools",
                "read,bash,powershell,edit,write,grep,find,ls,mcp__rustboard__*",
                "--approve",
            ]
        );
        assert!(
            !config
                .pi_args()
                .iter()
                .any(|arg| arg == "--no-builtin-tools")
        );
    }

    #[test]
    fn board_profile_rejects_the_power_allowlist() {
        let board = config(PiProfile::Board).pi_args().join(" ");
        assert!(!board.contains("bash"));
        assert!(!board.contains("powershell"));
        assert!(!board.contains("read,"));
        assert!(board.contains("mcp__rustboard__*"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_launch_plan_uses_cmd_and_quotes_the_whole_command() {
        let config = config(PiProfile::Board);
        let plan = config.launch_plan();
        assert_eq!(plan.program, "cmd.exe");
        assert_eq!(plan.args, vec!["/d", "/s", "/c"]);
        let expected = r#""pi --mode rpc --no-session --no-builtin-tools --tools mcp__rustboard__* --approve""#;
        assert_eq!(plan.raw_command_line.as_deref(), Some(expected));
    }

    #[cfg(windows)]
    #[test]
    fn launch_override_points_at_a_spaced_executable() {
        let mut config = config(PiProfile::Board);
        config.executable = r"C:\target\test-tmp\fake pi.bat".to_owned();
        config.command = Some("cmd.exe".to_owned());
        config.base_args = Some(vec!["/d".to_owned(), "/s".to_owned(), "/c".to_owned()]);

        let plan = config.launch_plan();
        assert_eq!(plan.program, "cmd.exe");
        assert_eq!(plan.args, vec!["/d", "/s", "/c"]);
        let raw = plan.raw_command_line.expect("cmd launches with a raw line");
        assert!(
            raw.contains("\"C:\\target\\test-tmp\\fake pi.bat\""),
            "{raw}"
        );
        assert!(
            raw.starts_with("\"\""),
            "outer quotes protect the path: {raw}"
        );
    }

    #[test]
    fn extra_args_are_appended_after_profile_flags() {
        let mut config = config(PiProfile::Board);
        config.extra_args = vec!["--thinking".to_owned(), "low".to_owned()];
        let args = config.pi_args();
        assert_eq!(&args[args.len() - 2..], &["--thinking", "low"]);
        assert_eq!(args[3], "--no-builtin-tools");
    }

    #[test]
    fn windows_quoting_handles_spaces_quotes_and_backslashes() {
        assert_eq!(quote_windows_arg("plain"), "plain");
        assert_eq!(quote_windows_arg("two words"), "\"two words\"");
        assert_eq!(quote_windows_arg(r#"a"b"#), r#""a\"b""#);
        assert_eq!(
            quote_windows_arg(r#"C:\path with space\"#),
            r#""C:\path with space\\""#
        );
    }
}
