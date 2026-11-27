//! Read-only Tree WAL + cross-file `branch_base` inspector powering `/tree`,
//! `/inspect <node_id>`, and session resume replay (pure Layer 3 read-only consumer).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use anix_proto::wal::{WalEvent, WalRole};

/// Summarized node in a Tree WAL graph (for `/tree` rendering and `/inspect`).
#[derive(Debug, Clone)]
pub struct WalNodeRow {
    pub id: u64,
    pub parent_id: Option<u64>,
    pub turn_id: Option<u64>,
    pub kind: String,
    pub ts: String,
    pub preview: String,
    pub full_text: String,
    pub selectable_leaf: bool,
    pub is_head: bool,
    pub on_active_lineage: bool,
    pub tree_prefix: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// Complete parsed Tree WAL view (local file nodes + optional inherited `branch_base` reference).
#[derive(Debug, Clone, Default)]
pub struct WalGraph {
    pub path: PathBuf,
    pub branch_base: Option<String>,
    pub head_id: Option<u64>,
    pub rows: Vec<WalNodeRow>,
}

/// Compact replayed turn shown on session resume (`-r` / `/sessions`).
#[derive(Debug, Clone)]
pub struct ReplayTurn {
    pub role: String,
    pub text: String,
}

/// Conversation rewind checkpoint corresponding to a user prompt along the active WAL lineage.
#[derive(Debug, Clone)]
pub struct RewindPoint {
    pub user_node_id: u64,
    /// Leaf node right before this user prompt (restoring here lets the user edit & resend it).
    pub before_leaf: Option<u64>,
    /// Final safe leaf node of this prompt's turn (restoring here keeps this turn & drops later ones).
    pub after_leaf: u64,
    pub ts: String,
    pub prompt_text: String,
    pub preview: String,
    pub reply_preview: Option<String>,
}

/// Estimated token usage broken down by category along the active WAL lineage (for `/context`).
#[derive(Debug, Clone, Default)]
pub struct ContextBreakdown {
    pub max_tokens: u64,
    pub total_tokens: u64,
    pub system_tokens: u64,
    pub user_tokens: u64,
    pub assistant_tokens: u64,
    pub tool_tokens: u64,
    pub fold_tokens: u64,
    pub message_count: usize,
    pub tool_count: usize,
    pub fold_count: usize,
}

/// Analyze token distribution along the active lineage of `wal_path` (strictly read-only,
/// crossing `branch_base` files cleanly) and align total to `reported_tokens` when available.
pub fn analyze_wal_context(
    wal_path: &Path,
    active_leaf: Option<u64>,
    max_tokens: u64,
    reported_tokens: Option<u64>,
) -> ContextBreakdown {
    let mut visited = HashSet::new();
    let events = collect_lineage_events(wal_path, active_leaf, &mut visited);

    let mut sys_est = 0u64;
    let mut usr_est = 0u64;
    let mut asst_est = 0u64;
    let mut tool_est = 0u64;
    let mut fold_est = 0u64;
    let mut message_count = 0usize;
    let mut tool_count = 0usize;
    let mut fold_count = 0usize;

    for ev in &events {
        match ev {
            WalEvent::Message {
                role,
                text,
                reasoning,
                tool_calls,
                ..
            } => {
                let mut chars = text.as_deref().map(str::len).unwrap_or(0)
                    + reasoning.as_deref().map(str::len).unwrap_or(0);
                for tc in tool_calls {
                    chars += tc.name.len() + tc.arguments.to_string().len();
                }
                let est = ((chars as u64) / 3).max(4);
                match role {
                    WalRole::System => sys_est += est,
                    WalRole::User => {
                        usr_est += est;
                        message_count += 1;
                    }
                    WalRole::Assistant => {
                        asst_est += est;
                        message_count += 1;
                    }
                    WalRole::Tool => {
                        tool_est += est;
                        tool_count += 1;
                    }
                }
            }
            WalEvent::RuntimeUpdate { content, .. } => {
                sys_est += ((content.len() as u64) / 3).max(4);
            }
            WalEvent::Fold { summary, .. } => {
                fold_est += ((summary.len() as u64) / 3).max(4);
                fold_count += 1;
            }
            _ => {}
        }
    }

    let raw_sum = sys_est + usr_est + asst_est + tool_est + fold_est;
    let total_tokens = reported_tokens.unwrap_or(raw_sum);

    // Proportionally calibrate per-category estimates to match kernel's authoritative `reported_tokens`.
    let (system_tokens, user_tokens, assistant_tokens, tool_tokens, fold_tokens) =
        if raw_sum > 0 && total_tokens > 0 && reported_tokens.is_some() {
            let scale = |v: u64| ((v as u128 * total_tokens as u128) / raw_sum as u128) as u64;
            let s = scale(sys_est);
            let u = scale(usr_est);
            let a = scale(asst_est);
            let t = scale(tool_est);
            let f = total_tokens.saturating_sub(s + u + a + t);
            (s, u, a, t, f)
        } else {
            (sys_est, usr_est, asst_est, tool_est, fold_est)
        };

    ContextBreakdown {
        max_tokens: max_tokens.max(1),
        total_tokens,
        system_tokens,
        user_tokens,
        assistant_tokens,
        tool_tokens,
        fold_tokens,
        message_count,
        tool_count,
        fold_count,
    }
}

fn collect_lineage_events(
    wal_path: &Path,
    requested_leaf: Option<u64>,
    visited: &mut HashSet<(PathBuf, u64)>,
) -> Vec<WalEvent> {
    let Ok(events) = read_wal_events(wal_path) else {
        return Vec::new();
    };
    if events.is_empty() {
        return Vec::new();
    }
    let is_tree = events.last().and_then(WalEvent::id).is_some();
    if !is_tree {
        return events;
    }
    let eof_id = events.last().and_then(WalEvent::id).unwrap_or(0);
    let target = requested_leaf.unwrap_or(eof_id);
    if target == 0 {
        return Vec::new();
    }
    let canon = fs::canonicalize(wal_path).unwrap_or_else(|_| wal_path.to_path_buf());
    if !visited.insert((canon, target)) {
        return Vec::new();
    }

    let mut want = Some(target);
    let mut rev_events = Vec::new();
    let mut parent_events = Vec::new();
    for ev in events.into_iter().rev() {
        let Some(id) = ev.id() else { continue };
        if Some(id) != want {
            continue;
        }
        match &ev {
            WalEvent::SessionInit { .. } | WalEvent::Checkpoint { .. } => {
                rev_events.push(ev);
                break;
            }
            WalEvent::BranchBase { base, .. } => {
                if let Some((parent_path_s, leaf_s)) = base.rsplit_once('#')
                    && let Ok(parent_leaf) = leaf_s.parse::<u64>()
                {
                    parent_events = collect_lineage_events(
                        Path::new(parent_path_s),
                        Some(parent_leaf),
                        visited,
                    );
                }
                break;
            }
            _ => {
                want = ev.parent_id();
                rev_events.push(ev);
            }
        }
    }
    rev_events.reverse();
    parent_events.extend(rev_events);
    parent_events
}

impl WalGraph {
    /// Load and build the interactive tree graph for `wal_path` (strictly read-only, no `flock`).
    pub fn load(wal_path: &Path, active_leaf: Option<u64>) -> io::Result<Self> {
        if !wal_path.is_file() {
            return Ok(Self {
                path: wal_path.to_path_buf(),
                branch_base: None,
                head_id: None,
                rows: Vec::new(),
            });
        }

        let events = read_wal_events(wal_path)?;
        if events.is_empty() {
            return Ok(Self {
                path: wal_path.to_path_buf(),
                branch_base: None,
                head_id: None,
                rows: Vec::new(),
            });
        }

        let mut branch_base = None;
        let mut by_id: BTreeMap<u64, &WalEvent> = BTreeMap::new();
        let mut children_map: HashMap<u64, Vec<u64>> = HashMap::new();
        let mut roots: Vec<u64> = Vec::new();
        let mut synth_id: u64 = 0;

        // First collect valid events (supporting legacy linear WALs by assigning synthetic IDs).
        let is_tree = events.last().and_then(WalEvent::id).is_some();
        let mut indexed: Vec<(u64, Option<u64>, &WalEvent)> = Vec::with_capacity(events.len());

        for ev in &events {
            if let WalEvent::BranchBase { base, .. } = ev {
                branch_base = Some(base.clone());
            }
            let (id, pid) = if is_tree {
                let id = ev.id().unwrap_or(0);
                (id, ev.parent_id())
            } else {
                let prev = if synth_id == 0 { None } else { Some(synth_id) };
                synth_id += 1;
                (synth_id, prev)
            };
            if id == 0 {
                continue;
            }
            by_id.insert(id, ev);
            indexed.push((id, pid, ev));
            if let Some(p) = pid {
                children_map.entry(p).or_default().push(id);
            } else {
                roots.push(id);
            }
        }

        let head_id = active_leaf.or_else(|| indexed.last().map(|(id, _, _)| *id));

        // Walk active lineage from `head_id` back to root.
        let mut active_set: HashSet<u64> = HashSet::new();
        let mut parent_lookup: HashMap<u64, Option<u64>> = HashMap::new();
        for &(id, pid, _) in &indexed {
            parent_lookup.insert(id, pid);
        }
        let mut cur = head_id;
        while let Some(cid) = cur {
            if !active_set.insert(cid) {
                break;
            }
            cur = parent_lookup.get(&cid).copied().flatten();
        }

        // Pre-order DFS traversal producing clean Unicode branch connectors.
        let mut rows = Vec::with_capacity(indexed.len());
        for (r_idx, &root_id) in roots.iter().enumerate() {
            let is_last_root = r_idx + 1 == roots.len();
            dfs_emit(
                root_id,
                "",
                is_last_root,
                true,
                &by_id,
                &parent_lookup,
                &children_map,
                &active_set,
                head_id,
                &mut rows,
            );
        }

        Ok(Self {
            path: wal_path.to_path_buf(),
            branch_base,
            head_id,
            rows,
        })
    }

    pub fn find_node(&self, node_id: u64) -> Option<&WalNodeRow> {
        self.rows.iter().find(|r| r.id == node_id)
    }

    /// Build chronological rewind checkpoints (each user prompt along the active lineage) for `/rewind`.
    pub fn rewind_points(&self) -> Vec<RewindPoint> {
        let lineage: Vec<&WalNodeRow> = self.rows.iter().filter(|r| r.on_active_lineage).collect();
        let mut points = Vec::new();
        for (idx, row) in lineage.iter().enumerate() {
            if row.kind != "user" {
                continue;
            }
            // Walk forward until the next user turn on this lineage to find the turn's end leaf
            // and a brief assistant reply preview.
            let mut after_leaf = row.id;
            let mut reply_preview = None;
            for next in &lineage[idx + 1..] {
                if next.kind == "user" {
                    break;
                }
                if next.selectable_leaf {
                    after_leaf = next.id;
                }
                if next.kind == "assistant" && !next.preview.is_empty() {
                    reply_preview = Some(next.preview.clone());
                }
            }
            // Validate parent as a safe `--leaf` (System / Assistant / Tool / Fold / BranchBase are valid;
            // SessionInit / Checkpoint are not).
            let before_leaf = row.parent_id.filter(|&pid| {
                self.find_node(pid)
                    .is_some_and(|p| !matches!(p.kind.as_str(), "init" | "checkpoint"))
            });
            let clean_prompt = unwrap_maildir_input(&row.full_text);
            let preview = truncate_one_line(&clean_prompt, 72);
            points.push(RewindPoint {
                user_node_id: row.id,
                before_leaf,
                after_leaf,
                ts: row.ts.clone(),
                prompt_text: clean_prompt,
                preview,
                reply_preview,
            });
        }
        points
    }
}

/// Extract recent human-readable `(role, text)` turns along the active lineage (crossing
/// `branch_base` parent files read-only) for crisp session resume replay.
pub fn load_recent_replay(
    wal_path: &Path,
    active_leaf: Option<u64>,
    max_turns: usize,
) -> Vec<ReplayTurn> {
    let mut visited = HashSet::new();
    let mut all = collect_lineage_turns(wal_path, active_leaf, &mut visited);
    if all.len() > max_turns {
        all.drain(..all.len() - max_turns);
    }
    all
}

fn collect_lineage_turns(
    wal_path: &Path,
    requested_leaf: Option<u64>,
    visited: &mut HashSet<(PathBuf, u64)>,
) -> Vec<ReplayTurn> {
    let Ok(events) = read_wal_events(wal_path) else {
        return Vec::new();
    };
    if events.is_empty() {
        return Vec::new();
    }

    let is_tree = events.last().and_then(WalEvent::id).is_some();
    if !is_tree {
        let mut out = Vec::new();
        for ev in &events {
            if let Some(t) = wal_event_to_replay_turn(ev) {
                out.push(t);
            }
        }
        return out;
    }

    let eof_id = events.last().and_then(WalEvent::id).unwrap_or(0);
    let target = requested_leaf.unwrap_or(eof_id);
    if target == 0 {
        return Vec::new();
    }
    let canon = fs::canonicalize(wal_path).unwrap_or_else(|_| wal_path.to_path_buf());
    if !visited.insert((canon, target)) {
        return Vec::new();
    }

    let mut want = Some(target);
    let mut rev_turns = Vec::new();
    let mut parent_turns = Vec::new();

    for ev in events.iter().rev() {
        let Some(id) = ev.id() else { continue };
        if Some(id) != want {
            continue;
        }
        match ev {
            WalEvent::SessionInit { .. } | WalEvent::Checkpoint { .. } => break,
            WalEvent::BranchBase { base, .. } => {
                if let Some((parent_path_s, leaf_s)) = base.rsplit_once('#')
                    && let Ok(parent_leaf) = leaf_s.parse::<u64>()
                {
                    parent_turns =
                        collect_lineage_turns(Path::new(parent_path_s), Some(parent_leaf), visited);
                }
                break;
            }
            other => {
                if let Some(t) = wal_event_to_replay_turn(other) {
                    rev_turns.push(t);
                }
                want = other.parent_id();
            }
        }
    }

    rev_turns.reverse();
    parent_turns.extend(rev_turns);
    parent_turns
}

fn wal_event_to_replay_turn(ev: &WalEvent) -> Option<ReplayTurn> {
    match ev {
        WalEvent::Message {
            role: WalRole::User,
            text,
            ..
        } => {
            let clean = unwrap_maildir_input(text.as_deref().unwrap_or(""));
            let t = clean.trim();
            if t.is_empty() {
                None
            } else {
                Some(ReplayTurn {
                    role: "user".into(),
                    text: t.to_owned(),
                })
            }
        }
        WalEvent::Message {
            role: WalRole::Assistant,
            text,
            tool_calls,
            ..
        } => {
            let t = text.as_deref().unwrap_or("").trim();
            if !t.is_empty() {
                Some(ReplayTurn {
                    role: "assistant".into(),
                    text: t.to_owned(),
                })
            } else if !tool_calls.is_empty() {
                let cmds: Vec<String> = tool_calls
                    .iter()
                    .map(|tc| {
                        tc.arguments
                            .get("cmd")
                            .and_then(|v| v.as_str())
                            .map(|c| format!("{}: {}", tc.name, truncate_one_line(c, 60)))
                            .unwrap_or_else(|| tc.name.clone())
                    })
                    .collect();
                Some(ReplayTurn {
                    role: "tool_call".into(),
                    text: cmds.join("; "),
                })
            } else {
                None
            }
        }
        WalEvent::Fold {
            from_turn,
            to_turn,
            summary,
            ..
        } => Some(ReplayTurn {
            role: "fold".into(),
            text: format!("turns {from_turn}..{to_turn}: {summary}"),
        }),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn dfs_emit(
    id: u64,
    indent: &str,
    is_last: bool,
    is_root: bool,
    by_id: &BTreeMap<u64, &WalEvent>,
    parent_lookup: &HashMap<u64, Option<u64>>,
    children_map: &HashMap<u64, Vec<u64>>,
    active_set: &HashSet<u64>,
    head_id: Option<u64>,
    out: &mut Vec<WalNodeRow>,
) {
    let Some(&ev) = by_id.get(&id) else { return };
    let parent_id = parent_lookup.get(&id).copied().flatten();
    let tree_prefix = if is_root {
        String::new()
    } else if is_last {
        format!("{indent}└─ ")
    } else {
        format!("{indent}├─ ")
    };

    let (kind, turn_id, ts, preview, full_text, selectable_leaf, input_tokens, output_tokens) =
        summarize_event(ev);

    out.push(WalNodeRow {
        id,
        parent_id,
        turn_id,
        kind,
        ts,
        preview,
        full_text,
        selectable_leaf,
        is_head: head_id == Some(id),
        on_active_lineage: active_set.contains(&id),
        tree_prefix,
        input_tokens,
        output_tokens,
    });

    if let Some(children) = children_map.get(&id) {
        let next_indent = if is_root {
            String::new()
        } else if is_last {
            format!("{indent}   ")
        } else {
            format!("{indent}│  ")
        };
        for (idx, &child_id) in children.iter().enumerate() {
            let child_is_last = idx + 1 == children.len();
            dfs_emit(
                child_id,
                &next_indent,
                child_is_last,
                false,
                by_id,
                parent_lookup,
                children_map,
                active_set,
                head_id,
                out,
            );
        }
    }
}

type EventSummary = (
    String,
    Option<u64>,
    String,
    String,
    String,
    bool,
    Option<u64>,
    Option<u64>,
);

fn summarize_event(ev: &WalEvent) -> EventSummary {
    match ev {
        WalEvent::SessionInit { ts, session_id, .. } => {
            let s = format!("session_id={}", session_id.as_deref().unwrap_or("-"));
            (
                "init".into(),
                None,
                short_ts(ts),
                s.clone(),
                s,
                false,
                None,
                None,
            )
        }
        WalEvent::BranchBase { ts, base, .. } => {
            let s = format!("base={base}");
            (
                "fork_base".into(),
                None,
                short_ts(ts),
                truncate_one_line(&s, 72),
                s,
                true,
                None,
                None,
            )
        }
        WalEvent::Checkpoint {
            ts,
            turn_id,
            cumulative_usage,
            ..
        } => {
            let s = "compaction checkpoint".to_owned();
            (
                "checkpoint".into(),
                Some(*turn_id),
                short_ts(ts),
                s.clone(),
                s,
                false,
                cumulative_usage.map(|u| u.input_tokens),
                cumulative_usage.map(|u| u.output_tokens),
            )
        }
        WalEvent::RuntimeUpdate {
            ts,
            turn_id,
            content,
            ..
        } => (
            "update".into(),
            Some(*turn_id),
            short_ts(ts),
            truncate_one_line(content, 72),
            content.clone(),
            true,
            None,
            None,
        ),
        WalEvent::Fold {
            ts,
            from_turn,
            to_turn,
            summary,
            ..
        } => {
            let full = format!("folded turns {from_turn}..{to_turn}: {summary}");
            (
                "fold".into(),
                Some(*from_turn),
                short_ts(ts),
                truncate_one_line(&full, 72),
                full,
                true,
                None,
                None,
            )
        }
        WalEvent::Message {
            ts,
            turn_id,
            role,
            text,
            tool_calls,
            tool_call_id,
            usage,
            ..
        } => {
            let (kind, selectable) = match role {
                WalRole::System => ("system", false),
                WalRole::User => ("user", true),
                WalRole::Assistant => ("assistant", tool_calls.is_empty()),
                WalRole::Tool => ("tool", true),
            };
            let full = if !tool_calls.is_empty() {
                let calls: Vec<String> = tool_calls
                    .iter()
                    .map(|tc| format!("{}({})", tc.name, tc.arguments))
                    .collect();
                let mut s = calls.join("\n");
                if let Some(t) = text
                    && !t.trim().is_empty()
                {
                    s = format!("{t}\n{s}");
                }
                s
            } else if let Some(cid) = tool_call_id {
                format!("[{cid}]\n{}", text.as_deref().unwrap_or(""))
            } else if matches!(role, WalRole::User) {
                unwrap_maildir_input(text.as_deref().unwrap_or(""))
            } else {
                text.clone().unwrap_or_default()
            };
            (
                kind.into(),
                Some(*turn_id),
                short_ts(ts),
                truncate_one_line(&full, 72),
                full,
                selectable,
                usage.map(|u| u.input_tokens),
                usage.map(|u| u.output_tokens),
            )
        }
    }
}

fn read_wal_events(path: &Path) -> io::Result<Vec<WalEvent>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(ev) = serde_json::from_str::<WalEvent>(&line) {
            events.push(ev);
        }
    }
    Ok(events)
}

fn short_ts(ts: &str) -> String {
    if let Some((_, time_part)) = ts.split_once('T') {
        time_part.chars().take(8).collect()
    } else {
        ts.chars().take(8).collect()
    }
}

/// Strip `<input ...>...</input>` Maildir envelope wrappers so UI views (`/rewind`, `/tree`,
/// `/resume` replay) display clean user prompt text.
pub fn unwrap_maildir_input(raw: &str) -> String {
    let trimmed = raw.trim();
    if !trimmed.starts_with("<input ") || !trimmed.ends_with("</input>") {
        return trimmed.to_owned();
    }
    let mut parts = Vec::new();
    let mut rest = trimmed;
    while let Some(start) = rest.find("<input ") {
        let after_start = &rest[start..];
        let Some(gt) = after_start.find('>') else {
            break;
        };
        let body_and_tail = &after_start[gt + 1..];
        let Some(close) = body_and_tail.find("</input>") else {
            break;
        };
        let inner = body_and_tail[..close].trim();
        if !inner.is_empty() {
            parts.push(inner);
        }
        rest = &body_and_tail[close + "</input>".len()..];
    }
    if parts.is_empty() {
        trimmed.to_owned()
    } else {
        parts.join("\n\n")
    }
}

fn truncate_one_line(raw: &str, max_chars: usize) -> String {
    let single = raw.replace(['\r', '\n'], " ");
    let trimmed = single.trim();
    let mut out = String::new();
    for (idx, ch) in trimmed.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use anix_proto::IR_VERSION;
    use anix_proto::wal::WalConfigSnapshot;

    #[test]
    fn builds_branching_tree_graph_and_active_lineage() {
        let tmp =
            std::env::temp_dir().join(format!("anix-agent-walview-{}.jsonl", uuid::Uuid::new_v4()));
        let lines = [
            serde_json::to_string(&WalEvent::SessionInit {
                id: Some(1),
                ir_version: IR_VERSION,
                ts: "2026-11-27T02:00:00Z".into(),
                session_id: Some("sid-1".into()),
                config: WalConfigSnapshot::default(),
            })
            .unwrap(),
            serde_json::to_string(&WalEvent::Message {
                id: Some(2),
                parent_id: Some(1),
                turn_id: 0,
                role: WalRole::System,
                ts: "2026-11-27T02:00:00Z".into(),
                text: Some("sys".into()),
                reasoning: None,
                tool_calls: Vec::new(),
                tool_call_id: None,
                is_error: false,
                media: Vec::new(),
                provider_opaque: None,
                usage: None,
                message_ids: Vec::new(),
            })
            .unwrap(),
            serde_json::to_string(&WalEvent::Message {
                id: Some(3),
                parent_id: Some(2),
                turn_id: 1,
                role: WalRole::User,
                ts: "2026-11-27T02:00:01Z".into(),
                text: Some("first".into()),
                reasoning: None,
                tool_calls: Vec::new(),
                tool_call_id: None,
                is_error: false,
                media: Vec::new(),
                provider_opaque: None,
                usage: None,
                message_ids: Vec::new(),
            })
            .unwrap(),
            serde_json::to_string(&WalEvent::Message {
                id: Some(4),
                parent_id: Some(3),
                turn_id: 2,
                role: WalRole::Assistant,
                ts: "2026-11-27T02:00:02Z".into(),
                text: Some("branch A".into()),
                reasoning: None,
                tool_calls: Vec::new(),
                tool_call_id: None,
                is_error: false,
                media: Vec::new(),
                provider_opaque: None,
                usage: None,
                message_ids: Vec::new(),
            })
            .unwrap(),
            serde_json::to_string(&WalEvent::Message {
                id: Some(5),
                parent_id: Some(3),
                turn_id: 3,
                role: WalRole::Assistant,
                ts: "2026-11-27T02:00:03Z".into(),
                text: Some("branch B".into()),
                reasoning: None,
                tool_calls: Vec::new(),
                tool_call_id: None,
                is_error: false,
                media: Vec::new(),
                provider_opaque: None,
                usage: None,
                message_ids: Vec::new(),
            })
            .unwrap(),
        ];
        fs::write(&tmp, format!("{}\n", lines.join("\n"))).unwrap();

        let graph = WalGraph::load(&tmp, None).unwrap();
        assert_eq!(graph.head_id, Some(5));
        assert_eq!(graph.rows.len(), 5);
        assert!(graph.find_node(5).unwrap().is_head);
        assert!(graph.find_node(3).unwrap().on_active_lineage);
        assert!(!graph.find_node(4).unwrap().on_active_lineage);

        let replay = load_recent_replay(&tmp, None, 10);
        assert_eq!(replay.len(), 2);
        assert_eq!(replay[0].text, "first");
        assert_eq!(replay[1].text, "branch B");

        let rps = graph.rewind_points();
        assert_eq!(rps.len(), 1);
        assert_eq!(rps[0].user_node_id, 3);
        assert_eq!(rps[0].before_leaf, Some(2));
        assert_eq!(rps[0].after_leaf, 5);
        assert_eq!(
            unwrap_maildir_input("<input source=\"a.msg\" from=\"user\">\nhello world\n</input>"),
            "hello world"
        );

        let _ = fs::remove_file(&tmp);
    }
}
