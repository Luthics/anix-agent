//! Clipboard helpers (macOS/Linux text + image capture), workspace file scanner for `@`,
//! untracked diff synthesis, and CJK-aware formatting utilities.

use std::path::PathBuf;
use std::time::Duration;

use unicode_width::UnicodeWidthStr;

pub(super) fn strip_frontmatter(raw: &str) -> &str {
    let trimmed = raw.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---")
        && let Some((_, body)) = rest.split_once("\n---")
    {
        return body.trim_start_matches('\r').trim_start_matches('\n');
    }
    raw
}

pub(super) fn fmt_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", d.as_secs_f64())
    } else {
        let secs = d.as_secs();
        format!("{}m{}s", secs / 60, secs % 60)
    }
}

/// Capture an image from the OS clipboard into a private `0600` PNG under `runtime_dir`.
/// Returns `Ok(Some(("image/png", path)))` if an image was found, or `Ok(None)` if clipboard has no image.
pub(super) fn capture_clipboard_image(
    runtime_dir: &std::path::Path,
) -> Result<Option<(String, PathBuf)>, String> {
    let _ = crate::session::ensure_dir_0700(runtime_dir);
    let id = &uuid::Uuid::new_v4().simple().to_string()[..10];
    let dest = runtime_dir.join(format!("clip-{id}.png"));

    #[cfg(target_os = "macos")]
    {
        // Fast path: `pngpaste` if installed.
        if let Ok(st) = std::process::Command::new("pngpaste")
            .arg(&dest)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            && st.success()
            && dest.is_file()
            && std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) > 0
        {
            return Ok(Some(("image/png".into(), dest)));
        }
        let _ = std::fs::remove_file(&dest);

        // Built-in macOS `osascript` PNG extraction (zero external dependencies).
        let dest_str = dest.display().to_string().replace('"', "\\\"");
        let png_script = format!(
            "try\n\
             set imgData to the clipboard as «class PNGf»\n\
             set f to open for access POSIX file \"{dest_str}\" with write permission\n\
             set eof f to 0\n\
             write imgData to f\n\
             close access f\n\
             return \"ok\"\n\
             on error\n\
             return \"no\"\n\
             end try"
        );
        if let Ok(out) = std::process::Command::new("osascript")
            .args(["-e", &png_script])
            .output()
            && out.status.success()
            && String::from_utf8_lossy(&out.stdout).trim() == "ok"
            && dest.is_file()
            && std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) > 0
        {
            return Ok(Some(("image/png".into(), dest)));
        }
        let _ = std::fs::remove_file(&dest);

        // Fallback: TIFF clipboard (common in Preview / Safari) converted via built-in `sips`.
        let tiff_path = runtime_dir.join(format!("clip-{id}.tiff"));
        let tiff_str = tiff_path.display().to_string().replace('"', "\\\"");
        let tiff_script = format!(
            "try\n\
             set imgData to the clipboard as TIFF picture\n\
             set f to open for access POSIX file \"{tiff_str}\" with write permission\n\
             set eof f to 0\n\
             write imgData to f\n\
             close access f\n\
             return \"ok\"\n\
             on error\n\
             return \"no\"\n\
             end try"
        );
        if let Ok(out) = std::process::Command::new("osascript")
            .args(["-e", &tiff_script])
            .output()
            && out.status.success()
            && String::from_utf8_lossy(&out.stdout).trim() == "ok"
            && tiff_path.is_file()
        {
            let conv = std::process::Command::new("sips")
                .args(["-s", "format", "png"])
                .arg(&tiff_path)
                .arg("--out")
                .arg(&dest)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            let _ = std::fs::remove_file(&tiff_path);
            if conv.is_ok_and(|s| s.success())
                && dest.is_file()
                && std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) > 0
            {
                return Ok(Some(("image/png".into(), dest)));
            }
        }
        let _ = std::fs::remove_file(&tiff_path);
        let _ = std::fs::remove_file(&dest);
        Ok(None)
    }

    #[cfg(not(target_os = "macos"))]
    {
        for (bin, args) in [
            ("wl-paste", vec!["--no-newline", "--type", "image/png"]),
            (
                "xclip",
                vec!["-selection", "clipboard", "-t", "image/png", "-o"],
            ),
        ] {
            if let Ok(out) = std::process::Command::new(bin).args(&args).output()
                && out.status.success()
                && !out.stdout.is_empty()
            {
                std::fs::write(&dest, &out.stdout).map_err(|e| e.to_string())?;
                return Ok(Some(("image/png".into(), dest)));
            }
        }
        Ok(None)
    }
}

pub(super) fn read_clipboard_text() -> Option<String> {
    #[cfg(target_os = "macos")]
    let candidates: &[(&str, &[&str])] = &[("pbpaste", &[])];
    #[cfg(not(target_os = "macos"))]
    let candidates: &[(&str, &[&str])] = &[
        ("wl-paste", &["--no-newline"]),
        ("xclip", &["-selection", "clipboard", "-o"]),
    ];
    for &(bin, args) in candidates {
        if let Ok(out) = std::process::Command::new(bin).args(args).output()
            && out.status.success()
        {
            let s = String::from_utf8_lossy(&out.stdout).into_owned();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

pub(super) fn write_clipboard_text(text: &str) -> Result<(), String> {
    use std::io::Write as _;
    use std::process::Stdio;

    #[cfg(target_os = "macos")]
    let candidates: &[(&str, &[&str])] = &[("pbcopy", &[])];
    #[cfg(not(target_os = "macos"))]
    let candidates: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard", "-i"]),
        ("xsel", &["--clipboard", "--input"]),
    ];

    let mut last_err = String::from("no clipboard utility available");
    for &(bin, args) in candidates {
        match std::process::Command::new(bin)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take()
                    && let Err(e) = stdin.write_all(text.as_bytes())
                {
                    last_err = format!("{bin} write failed: {e}");
                    let _ = child.wait();
                    continue;
                }
                match child.wait_with_output() {
                    Ok(out) if out.status.success() => return Ok(()),
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        last_err = format!("{bin} exited {}: {}", out.status, stderr.trim());
                    }
                    Err(e) => last_err = format!("{bin} wait failed: {e}"),
                }
            }
            Err(e) => {
                last_err = format!("{bin}: {e}");
            }
        }
    }
    Err(last_err)
}

/// Build a flat relative file list for `@` recursive fuzzy path completion.
/// Prefers `git ls-files` (respects `.gitignore` natively & runs in milliseconds) and falls back
/// to a bounded DFS walk outside git repositories.
pub(super) fn scan_workspace_files(cwd: &std::path::Path) -> Vec<String> {
    if let Ok(out) = std::process::Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(cwd)
        .output()
        && out.status.success()
    {
        let mut files: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .take(4000)
            .map(ToOwned::to_owned)
            .collect();
        if !files.is_empty() {
            files.sort();
            files.dedup();
            return files;
        }
    }

    let mut out = Vec::new();
    walk_workspace_dir(cwd, cwd, 0, &mut out);
    out.sort();
    out
}

fn walk_workspace_dir(
    root: &std::path::Path,
    dir: &std::path::Path,
    depth: usize,
    out: &mut Vec<String>,
) {
    if depth > 6 || out.len() >= 2000 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= 2000 {
            break;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.')
            || matches!(
                name.as_str(),
                "target" | "node_modules" | "dist" | "build" | "__pycache__" | "venv"
            )
        {
            continue;
        }
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            walk_workspace_dir(root, &path, depth + 1, out);
        } else if ft.is_file()
            && let Ok(rel) = path.strip_prefix(root)
        {
            out.push(rel.display().to_string());
        }
    }
}

/// Synthesize a unified diff view for an untracked (`??`) file in `/diff`.
pub(super) fn synthesize_untracked_diff(full_path: &std::path::Path, rel_path: &str) -> String {
    let Ok(bytes) = std::fs::read(full_path) else {
        return String::new();
    };
    if bytes.is_empty() {
        return format!(
            "diff --git a/{rel_path} b/{rel_path}\nnew file mode 100644\n--- /dev/null\n+++ b/{rel_path}\n"
        );
    }
    if bytes.iter().take(4096).any(|&b| b == 0) {
        return format!(
            "diff --git a/{rel_path} b/{rel_path}\nnew file mode 100644\nBinary file ({} bytes)\n",
            bytes.len()
        );
    }
    let text = String::from_utf8_lossy(&bytes);
    let all_lines: Vec<&str> = text.lines().collect();
    let shown = all_lines.len().min(400);
    let mut out = format!(
        "diff --git a/{rel_path} b/{rel_path}\nnew file mode 100644\n--- /dev/null\n+++ b/{rel_path}\n@@ -0,0 +1,{shown} @@\n"
    );
    for l in &all_lines[..shown] {
        out.push('+');
        out.push_str(l);
        out.push('\n');
    }
    if all_lines.len() > shown {
        out.push_str(&format!(
            "  … +{} more lines omitted\n",
            all_lines.len() - shown
        ));
    }
    out
}

pub(super) fn fmt_k(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

pub(super) fn tail_preview(s: &str, max_cols: usize) -> String {
    let single = s.replace(['\r', '\n'], " ");
    let trimmed = single.trim();
    if trimmed.width() <= max_cols {
        return trimmed.to_owned();
    }
    let mut rev_chars = Vec::new();
    let mut w = 0usize;
    for ch in trimmed.chars().rev() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw + 1 > max_cols {
            break;
        }
        rev_chars.push(ch);
        w += cw;
    }
    rev_chars.reverse();
    format!("…{}", rev_chars.into_iter().collect::<String>())
}

pub(super) fn pad_display_width(raw: &str, target_cols: usize) -> String {
    let clipped = truncate_single_line(raw, target_cols);
    let w = clipped.width();
    if w >= target_cols {
        clipped
    } else {
        format!("{clipped}{}", " ".repeat(target_cols - w))
    }
}

pub(super) fn truncate_single_line(raw: &str, max_cols: usize) -> String {
    let max_cols = max_cols.max(4);
    let single = raw.replace(['\r', '\n'], " ");
    let trimmed = single.trim();
    let mut out = String::new();
    let mut w = 0usize;
    for ch in trimmed.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw + 1 > max_cols {
            out.push('…');
            break;
        }
        out.push(ch);
        w += cw;
    }
    out
}

/// Resolve the canonical project root shared across git worktrees without spawning subprocesses
/// (kept byte-for-byte consistent with Layer 2 `anix-hook` `15-memory`).
pub(super) fn resolve_shared_project_root(cwd: &std::path::Path) -> PathBuf {
    let cwd_canon = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut cur = Some(cwd_canon.as_path());
    while let Some(dir) = cur {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return dir.to_path_buf();
        }
        if dot_git.is_file() {
            if let Ok(raw) = std::fs::read_to_string(&dot_git)
                && let Some(gd_raw) = raw.lines().find_map(|l| l.trim().strip_prefix("gitdir:"))
            {
                let gd_trimmed = gd_raw.trim();
                let gd_path = PathBuf::from(gd_trimmed);
                let gitdir = if gd_path.is_absolute() {
                    gd_path
                } else {
                    dir.join(gd_path)
                };
                let gitdir_canon = std::fs::canonicalize(&gitdir).unwrap_or(gitdir);
                if let Ok(common_raw) = std::fs::read_to_string(gitdir_canon.join("commondir")) {
                    let cd_trimmed = common_raw.trim();
                    if !cd_trimmed.is_empty() {
                        let common_path = if std::path::Path::new(cd_trimmed).is_absolute() {
                            PathBuf::from(cd_trimmed)
                        } else {
                            gitdir_canon.join(cd_trimmed)
                        };
                        let common_canon =
                            std::fs::canonicalize(&common_path).unwrap_or(common_path);
                        if common_canon.file_name().and_then(|s| s.to_str()) == Some(".git")
                            && let Some(main_root) = common_canon.parent()
                        {
                            return main_root.to_path_buf();
                        }
                    }
                }
            }
            return dir.to_path_buf();
        }
        cur = dir.parent();
    }
    cwd_canon
}

/// Stable project memory key matching Layer 2 `15-memory` (`<sanitized-basename>-<fnv1a32-hex>`).
pub(super) fn project_memory_key(cwd: &std::path::Path) -> String {
    let root = resolve_shared_project_root(cwd);
    let canon = std::fs::canonicalize(&root).unwrap_or(root);
    let raw_base = canon
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project");
    let mut clean_base: String = raw_base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    clean_base = clean_base.trim_matches('-').to_owned();
    if clean_base.is_empty() {
        clean_base = "project".to_owned();
    }
    if clean_base.len() > 32 {
        clean_base.truncate(32);
    }

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in canon.to_string_lossy().as_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let folded = (hash ^ (hash >> 32)) as u32;
    format!("{clean_base}-{folded:08x}")
}

pub(super) fn anix_config_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
        .join("anix")
}

pub(super) fn anix_hooks_pre_context_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ANIX_HOOKS_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir).join("pre-context.d");
    }
    anix_config_dir().join("hooks/pre-context.d")
}

pub(super) fn anix_memory_dir(cwd: &std::path::Path) -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let data_dir = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"))
        .join("anix");
    data_dir.join("memory").join(project_memory_key(cwd))
}

pub(super) fn resolve_anix_hook_bin(anix_bin: &std::path::Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(prefix) = std::env::var_os("ANIX_PREFIX").filter(|v| !v.is_empty()) {
        candidates.push(PathBuf::from(prefix).join("libexec/anix/anix-hook"));
    }
    if let Some(bin_dir) = anix_bin.parent()
        && let Some(prefix) = bin_dir.parent()
    {
        candidates.push(prefix.join("libexec/anix/anix-hook"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".local/libexec/anix/anix-hook"));
    }
    candidates.push(PathBuf::from("/usr/local/libexec/anix/anix-hook"));
    candidates.into_iter().find(|p| p.is_file())
}

pub(super) fn parse_memory_frontmatter_type(raw: &str) -> Option<String> {
    let normalized = raw.replace("\r\n", "\n");
    let mut lines = normalized.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some(val) = trimmed.strip_prefix("type:") {
            let clean = val.trim().trim_matches(|c| c == '"' || c == '\'');
            if !clean.is_empty() {
                return Some(clean.to_owned());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_memory_key_shares_across_worktrees() {
        let root = std::env::temp_dir().join(format!("anix-agent-mem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let main_repo = root.join("demo-proj");
        let wt = root.join("demo-proj-wt");
        std::fs::create_dir_all(main_repo.join(".git/worktrees/wt1")).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(main_repo.join(".git/worktrees/wt1/commondir"), "../..\n").unwrap();
        std::fs::write(
            wt.join(".git"),
            format!(
                "gitdir: {}\n",
                main_repo.join(".git/worktrees/wt1").display()
            ),
        )
        .unwrap();

        assert_eq!(project_memory_key(&main_repo), project_memory_key(&wt));
        assert!(project_memory_key(&main_repo).starts_with("demo-proj-"));
        assert_eq!(
            parse_memory_frontmatter_type("---\nname: test\ntype: feedback\n---\nbody"),
            Some("feedback".into())
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
