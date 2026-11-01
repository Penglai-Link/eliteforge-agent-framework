//! Host shell selection and native command-string arguments. Explicit argv
//! bypasses these helpers and never receives shell interpretation.
use std::path::{Path, PathBuf};

fn shell_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase()
        .trim_end_matches(".exe")
        .to_owned()
}

fn supported_shell(path: &Path) -> bool {
    path.is_absolute()
        && path.is_file()
        && matches!(
            shell_name(path).as_str(),
            "sh" | "bash" | "zsh" | "dash" | "ksh" | "fish" | "pwsh" | "powershell" | "cmd"
        )
}

/// Resolve a real shell on the current operating system, without spawning it.
pub fn default_shell_program() -> PathBuf {
    if let Some(shell) = std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|path| supported_shell(path))
    {
        return shell;
    }
    #[cfg(windows)]
    {
        let paths = std::env::var_os("PATH")
            .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
            .unwrap_or_default();
        for name in ["pwsh.exe", "powershell.exe"] {
            if let Some(path) = paths
                .iter()
                .map(|dir| dir.join(name))
                .find(|path| supported_shell(path))
            {
                return path;
            }
        }
        let root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let powershell = root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        if powershell.is_file() {
            return powershell;
        }
        if let Some(shell) = std::env::var_os("COMSPEC")
            .map(PathBuf::from)
            .filter(|path| supported_shell(path))
        {
            return shell;
        }
        root.join(r"System32\cmd.exe")
    }
    #[cfg(not(windows))]
    {
        #[cfg(target_os = "macos")]
        let candidates = [
            "/bin/zsh",
            "/bin/bash",
            "/bin/sh",
            "/usr/bin/bash",
            "/usr/bin/zsh",
        ];
        #[cfg(not(target_os = "macos"))]
        let candidates = [
            "/bin/sh",
            "/bin/bash",
            "/bin/zsh",
            "/usr/bin/bash",
            "/usr/bin/zsh",
        ];
        candidates
            .iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
            .unwrap_or_else(|| PathBuf::from("/bin/sh"))
    }
}

pub(crate) fn arguments(
    shell: &Path,
    command: String,
    login: bool,
    interactive: bool,
) -> Vec<String> {
    match shell_name(shell).as_str() {
        "cmd" => vec!["/D".into(), "/S".into(), "/C".into(), command],
        "powershell" | "pwsh" => {
            use base64::Engine;
            // -EncodedCommand requires UTF-16LE. Encoding avoids Windows argv
            // escaping changing quotes, pipes, Unicode or script operators.
            let script = format!(
                "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); $OutputEncoding = [Console]::OutputEncoding; {command}"
            );
            let bytes = script
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>();
            let mut args = vec!["-NoLogo".into(), "-NoProfile".into()];
            if !interactive {
                args.push("-NonInteractive".into());
            }
            args.extend([
                "-EncodedCommand".into(),
                base64::engine::general_purpose::STANDARD.encode(bytes),
            ]);
            args
        }
        _ => vec![if login { "-lc" } else { "-c" }.into(), command],
    }
}

/// cmd.exe parses its /C text as a command line rather than a C argv element.
/// Use raw_arg only for the exact command-string form generated above.
pub(crate) fn process_arguments(
    process: &mut tokio::process::Command,
    program: &str,
    args: &[String],
) {
    #[cfg(windows)]
    if shell_name(Path::new(program)) == "cmd"
        && args.len() == 4
        && args[0] == "/D"
        && args[1] == "/S"
        && args[2] == "/C"
    {
        process.args(&args[..3]);
        process.raw_arg(format!("\"{}\"", args[3]));
        return;
    }
    #[cfg(not(windows))]
    let _ = program;
    process.args(args);
}
