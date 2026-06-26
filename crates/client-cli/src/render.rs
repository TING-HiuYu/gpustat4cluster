use std::{
    collections::HashMap,
    io::IsTerminal,
    process::Command,
    sync::{Mutex, OnceLock},
};

use chrono::{Local, TimeZone};

use crate::{
    args::CliOptions,
    backend::{GresView, NodeView, ProcessView, QueryResponse},
};

const DEFAULT_GRESNAME_WIDTH: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOptions {
    pub color: bool,
    pub show_header: bool,
    pub no_processes: bool,
    pub show_cmd: bool,
    pub show_user: bool,
    pub show_pid: bool,
    pub gresname_width: Option<usize>,
    pub latency_display: bool,
}

impl RenderOptions {
    pub fn from_cli(opts: &CliOptions) -> Self {
        Self {
            color: opts.force_color || (!opts.no_color && std::io::stdout().is_terminal()),
            show_header: opts.show_header,
            no_processes: opts.no_processes,
            show_cmd: opts.show_cmd,
            show_user: opts.show_user,
            show_pid: opts.show_pid,
            gresname_width: opts.gresname_width,
            latency_display: crate::backend::latency_display_from_options(opts),
        }
    }
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            color: false,
            show_header: true,
            no_processes: false,
            show_cmd: false,
            show_user: false,
            show_pid: false,
            gresname_width: None,
            latency_display: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct IncrementalRenderer {
    last_node_rows: HashMap<String, usize>,
}

impl IncrementalRenderer {
    pub fn render_full(
        &mut self,
        resp: &QueryResponse,
        user_filter: Option<&str>,
        opts: &RenderOptions,
    ) -> String {
        let output = render_table(resp, user_filter, opts);
        self.remember_layout(resp, user_filter);
        output
    }

    fn remember_layout(&mut self, resp: &QueryResponse, user_filter: Option<&str>) {
        self.last_node_rows.clear();
        for node in &resp.nodes {
            let rows = node
                .gres
                .iter()
                .filter(|gres| should_render_gres(gres, user_filter))
                .count();
            self.last_node_rows.insert(node.hostname.clone(), rows);
        }
    }
}

pub fn render_table(
    resp: &QueryResponse,
    user_filter: Option<&str>,
    opts: &RenderOptions,
) -> String {
    let mut out = String::new();
    let palette = Palette::new(opts.color);
    let global_name_width = gresname_width(resp, user_filter, opts.gresname_width);

    for node in &resp.nodes {
        let visible_gres: Vec<_> = node
            .gres
            .iter()
            .filter(|gres| should_render_gres(gres, user_filter))
            .collect();
        if visible_gres.is_empty() && !node.stale {
            continue;
        }

        if opts.show_header {
            render_node_header(&mut out, node, global_name_width, opts, &palette);
        }

        for gres in visible_gres {
            render_gres_row(
                &mut out,
                gres,
                user_filter,
                global_name_width,
                opts,
                &palette,
            );
        }
    }
    out
}

fn render_node_header(
    out: &mut String,
    node: &NodeView,
    name_width: usize,
    opts: &RenderOptions,
    palette: &Palette,
) {
    let width = name_width.saturating_add(3).max(DEFAULT_GRESNAME_WIDTH + 3);
    out.push_str(&palette.bold_white(&format!("{:<width$}", node.hostname, width = width)));
    out.push_str("  ");
    out.push_str(&format_snapshot_time(node.timestamp_ms));
    if let Some(driver) = node
        .driver_version
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        out.push_str("  ");
        out.push_str(&palette.bold_black(driver));
    }
    if opts.latency_display {
        if let Some(delay) = format_delay(node.delay_us) {
            out.push_str("  ");
            out.push_str(&palette.cyan(&format!("delay={delay}")));
        }
    }
    if node.stale {
        out.push_str(" ");
        out.push_str(&palette.bold_red("stale"));
    }
    if let Some(error) = &node.error {
        out.push_str("  ");
        out.push_str(error);
    }
    out.push('\n');
}

fn render_gres_row(
    out: &mut String,
    gres: &GresView,
    user_filter: Option<&str>,
    name_width: usize,
    opts: &RenderOptions,
    palette: &Palette,
) {
    out.push_str(&palette.cyan(&format!("[{}]", gres.index)));
    out.push(' ');

    if opts.gresname_width != Some(0) {
        let gres_name = if gres.name.trim().is_empty() {
            "GPU"
        } else {
            gres.name.as_str()
        };
        out.push_str(&palette.blue(&format!(
            "{:<width$}",
            shorten_left(gres_name, name_width),
            width = name_width
        )));
        out.push_str(" |");
    }

    let temp_value = gres
        .temperature_c
        .map(|value| format!("{:>3}", value))
        .unwrap_or_else(|| " ??".to_string());
    let temp_color = if gres.temperature_c.is_some_and(|value| value >= 50) {
        palette.bold_red(&format!("{temp_value}°C"))
    } else {
        palette.red(&format!("{temp_value}°C"))
    };
    out.push_str(&temp_color);
    out.push_str(", ");

    let util_value = format!("{:>3}", gres.util);
    let util_color = if gres.util >= 30 {
        palette.bold_green(&format!("{util_value} %"))
    } else {
        palette.green(&format!("{util_value} %"))
    };
    out.push_str(&util_color);
    out.push_str(" | ");

    out.push_str(&palette.bold_yellow(&format!("{:>5}", gres.mem_used_mb)));
    out.push_str(" / ");
    out.push_str(&palette.yellow(&format!("{:>5}", gres.mem_total_mb)));
    out.push_str(" MB");

    if !opts.no_processes {
        out.push_str(" |");
        for process in visible_processes(gres, user_filter) {
            out.push(' ');
            out.push_str(&format_process(process, opts, palette));
        }
    }
    out.push('\n');
}

pub fn render_json(resp: &QueryResponse) -> Result<String, String> {
    serde_json::to_string_pretty(resp)
        .map(|json| format!("{}\n", json))
        .map_err(|e| format!("encode JSON output failed: {}", e))
}

fn should_render_gres(gres: &GresView, user_filter: Option<&str>) -> bool {
    let Some(user) = normalized_user_filter(user_filter) else {
        return true;
    };

    match &gres.processes {
        Some(processes) => processes
            .iter()
            .any(|process| display_username(process) == user),
        None => true,
    }
}

fn visible_processes<'a>(gres: &'a GresView, user_filter: Option<&str>) -> Vec<&'a ProcessView> {
    let Some(processes) = &gres.processes else {
        return Vec::new();
    };
    let Some(user) = normalized_user_filter(user_filter) else {
        return processes.iter().collect();
    };
    processes
        .iter()
        .filter(|process| display_username(process) == user)
        .collect()
}

fn normalized_user_filter(user_filter: Option<&str>) -> Option<&str> {
    user_filter.and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

fn format_process(process: &ProcessView, opts: &RenderOptions, palette: &Palette) -> String {
    let mut out = String::new();
    if !opts.show_cmd || opts.show_user {
        out.push_str(&palette.bold_black(&display_username(process)));
    }
    if opts.show_cmd {
        if !out.is_empty() {
            out.push(':');
        }
        let command = process
            .command
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("--");
        out.push_str(&palette.cyan(command));
    }
    if opts.show_pid {
        out.push('/');
        out.push_str(&process.pid.to_string());
    }
    out.push('(');
    out.push_str(&palette.yellow(&format!("{}M", process.used_memory_mb)));
    out.push(')');
    out
}

fn format_delay(delay_us: Option<u64>) -> Option<String> {
    let delay_us = delay_us?;
    if delay_us < 1_000 {
        Some(format!("{}us", delay_us))
    } else {
        Some(format!("{:.2}ms", delay_us as f64 / 1_000.0))
    }
}

fn format_snapshot_time(timestamp_ms: i64) -> String {
    Local
        .timestamp_millis_opt(timestamp_ms)
        .single()
        .map(|dt| dt.format("%a %b %e %H:%M:%S %Y").to_string())
        .unwrap_or_else(|| "??? ?? ?? ??:??:?? ????".to_string())
}

fn gresname_width(
    resp: &QueryResponse,
    user_filter: Option<&str>,
    configured: Option<usize>,
) -> usize {
    if let Some(width) = configured {
        return width;
    }
    resp.nodes
        .iter()
        .flat_map(|node| node.gres.iter())
        .filter(|gres| should_render_gres(gres, user_filter))
        .map(|gres| {
            if gres.name.trim().is_empty() {
                3
            } else {
                gres.name.chars().count()
            }
        })
        .max()
        .unwrap_or(DEFAULT_GRESNAME_WIDTH)
        .max(DEFAULT_GRESNAME_WIDTH)
}

fn shorten_left(value: &str, width: usize) -> String {
    let len = value.chars().count();
    if width == 0 {
        return String::new();
    }
    if len <= width {
        return value.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }
    let keep = width - 1;
    let tail: String = value
        .chars()
        .rev()
        .take(keep)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{}", tail)
}

fn display_username(process: &ProcessView) -> String {
    let username = process.username.trim();
    if !username.is_empty() && username != "?" {
        return username.to_string();
    }
    username_for_uid(process.uid)
}

fn username_for_uid(uid: u32) -> String {
    if uid == 0 {
        return "root".to_string();
    }
    if uid == u32::MAX {
        return "?".to_string();
    }
    static CACHE: OnceLock<Mutex<HashMap<u32, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock() {
        if let Some(name) = guard.get(&uid) {
            return name.clone();
        }
    }
    let name = resolve_username_for_uid(uid).unwrap_or_else(|| uid.to_string());
    if let Ok(mut guard) = cache.lock() {
        guard.insert(uid, name.clone());
    }
    name
}

fn resolve_username_for_uid(uid: u32) -> Option<String> {
    let output = Command::new("getent")
        .arg("passwd")
        .arg(uid.to_string())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8(output.stdout).ok()?;
    line.split(':')
        .next()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

#[derive(Debug, Clone, Copy)]
struct Palette {
    enabled: bool,
}

impl Palette {
    fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    fn paint(self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0;10m")
        } else {
            text.to_string()
        }
    }

    fn bold_white(self, text: &str) -> String {
        self.paint("1;37", text)
    }
    fn bold_black(self, text: &str) -> String {
        self.paint("1;30", text)
    }
    fn bold_red(self, text: &str) -> String {
        self.paint("1;31", text)
    }
    fn red(self, text: &str) -> String {
        self.paint("31", text)
    }
    fn blue(self, text: &str) -> String {
        self.paint("34", text)
    }
    fn cyan(self, text: &str) -> String {
        self.paint("36", text)
    }
    fn green(self, text: &str) -> String {
        self.paint("32", text)
    }
    fn bold_green(self, text: &str) -> String {
        self.paint("1;32", text)
    }
    fn yellow(self, text: &str) -> String {
        self.paint("33", text)
    }
    fn bold_yellow(self, text: &str) -> String {
        self.paint("1;33", text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::NodeView;

    #[test]
    fn user_filter_is_noop_when_process_field_missing() {
        let resp = QueryResponse {
            meta: Default::default(),
            nodes: vec![NodeView {
                hostname: "node-a".to_string(),
                connection_id: String::new(),
                addr: String::new(),
                timestamp_ms: 0,
                driver_version: None,
                num: 0,
                stale: false,
                error: None,
                delay_us: None,
                gres: vec![GresView {
                    index: 0,
                    name: "NVIDIA A100".to_string(),
                    temperature_c: Some(32),
                    util: 42,
                    mem_used_mb: 1,
                    mem_total_mb: 2,
                    processes: None,
                }],
            }],
        };

        let rendered = render_table(&resp, Some("alice"), &RenderOptions::default());
        assert!(rendered.contains("node-a"));
    }

    #[test]
    fn test_rows_render_gres_data_not_only_header() {
        let resp = QueryResponse {
            meta: Default::default(),
            nodes: vec![NodeView {
                hostname: "node-a".to_string(),
                connection_id: String::new(),
                addr: String::new(),
                timestamp_ms: 0,
                driver_version: None,
                num: 0,
                stale: false,
                error: None,
                delay_us: None,
                gres: vec![
                    GresView {
                        index: 0,
                        name: "NVIDIA A100".to_string(),
                        temperature_c: Some(32),
                        util: 42,
                        mem_used_mb: 1024,
                        mem_total_mb: 8192,
                        processes: None,
                    },
                    GresView {
                        index: 1,
                        name: "NVIDIA A100".to_string(),
                        temperature_c: Some(32),
                        util: 77,
                        mem_used_mb: 4096,
                        mem_total_mb: 8192,
                        processes: None,
                    },
                ],
            }],
        };

        let rendered = render_table(&resp, None, &RenderOptions::default());
        assert!(rendered.contains("node-a"));
        assert!(rendered.contains("[0] NVIDIA A100"));
        assert!(rendered.contains("42 %"));
        assert!(rendered.contains("77 %"));
        assert!(rendered.lines().count() > 1);
    }

    #[test]
    fn hostname_renders_query_delay_when_available() {
        let resp = QueryResponse {
            meta: Default::default(),
            nodes: vec![NodeView {
                hostname: "node-a".to_string(),
                connection_id: String::new(),
                addr: String::new(),
                timestamp_ms: 0,
                driver_version: None,
                num: 0,
                stale: false,
                error: None,
                delay_us: Some(280),
                gres: vec![GresView {
                    index: 0,
                    name: "NVIDIA A100".to_string(),
                    temperature_c: Some(32),
                    util: 42,
                    mem_used_mb: 1024,
                    mem_total_mb: 8192,
                    processes: None,
                }],
            }],
        };

        let rendered = render_table(&resp, None, &RenderOptions::default());
        assert!(rendered.lines().next().unwrap().contains("delay=280us"));
    }

    #[test]
    fn empty_table_renders_stable_header_only() {
        let resp = QueryResponse {
            meta: Default::default(),
            nodes: Vec::new(),
        };

        let rendered = render_table(&resp, None, &RenderOptions::default());
        assert!(rendered.is_empty());
    }

    #[test]
    fn process_summary_renders_inline_with_gres_row() {
        let resp = QueryResponse {
            meta: Default::default(),
            nodes: vec![NodeView {
                hostname: "node-a".to_string(),
                connection_id: String::new(),
                addr: String::new(),
                timestamp_ms: 0,
                driver_version: None,
                num: 0,
                stale: false,
                error: None,
                delay_us: None,
                gres: vec![GresView {
                    index: 0,
                    name: "NVIDIA A100".to_string(),
                    temperature_c: Some(32),
                    util: 42,
                    mem_used_mb: 1024,
                    mem_total_mb: 8192,
                    processes: Some(vec![ProcessView {
                        uid: 1000,
                        username: "alice".to_string(),
                        pid: 7,
                        command: Some("python".to_string()),
                        used_memory_mb: 512,
                    }]),
                }],
            }],
        };

        let rendered = render_table(&resp, None, &RenderOptions::default());
        assert!(rendered.contains("node-a"));
        assert!(rendered.contains("alice(512M)"));
        assert!(!rendered.contains("pid=7"));
    }

    #[test]
    fn user_filter_hides_gres_with_non_matching_processes() {
        let resp = QueryResponse {
            meta: Default::default(),
            nodes: vec![NodeView {
                hostname: "node-a".to_string(),
                connection_id: String::new(),
                addr: String::new(),
                timestamp_ms: 0,
                driver_version: None,
                num: 0,
                stale: false,
                error: None,
                delay_us: None,
                gres: vec![GresView {
                    index: 0,
                    name: "NVIDIA A100".to_string(),
                    temperature_c: Some(32),
                    util: 42,
                    mem_used_mb: 1,
                    mem_total_mb: 2,
                    processes: Some(vec![ProcessView {
                        uid: 1001,
                        username: "bob".to_string(),
                        pid: 7,
                        command: None,
                        used_memory_mb: 0,
                    }]),
                }],
            }],
        };

        let rendered = render_table(&resp, Some("alice"), &RenderOptions::default());
        assert!(!rendered.contains("node-a"));
    }

    #[test]
    fn user_filter_keeps_and_trims_matching_processes() {
        let resp = QueryResponse {
            meta: Default::default(),
            nodes: vec![NodeView {
                connection_id: String::new(),
                hostname: "sample-node".to_string(),
                addr: String::new(),
                timestamp_ms: 0,
                driver_version: None,
                num: 0,
                stale: false,
                error: None,
                delay_us: None,
                gres: vec![GresView {
                    index: 0,
                    name: "NVIDIA A100".to_string(),
                    temperature_c: Some(32),
                    util: 66,
                    mem_used_mb: 1024,
                    mem_total_mb: 81920,
                    processes: Some(vec![
                        ProcessView {
                            uid: 1000,
                            username: "alice".to_string(),
                            pid: 7,
                            command: Some("python".to_string()),
                            used_memory_mb: 512,
                        },
                        ProcessView {
                            uid: 1001,
                            username: "bob".to_string(),
                            pid: 8,
                            command: Some("train".to_string()),
                            used_memory_mb: 256,
                        },
                    ]),
                }],
            }],
        };

        let rendered = render_table(&resp, Some("alice"), &RenderOptions::default());
        assert!(rendered.contains("sample-node"));
        assert!(rendered.contains("66 %"));
        assert!(rendered.contains("alice(512M)"));
        assert!(!rendered.contains("bob(256M)"));
    }

    #[test]
    fn json_output_includes_nodes_gres_and_processes() {
        let resp = QueryResponse {
            meta: Default::default(),
            nodes: vec![NodeView {
                hostname: "node-a".to_string(),
                connection_id: String::new(),
                addr: String::new(),
                timestamp_ms: 0,
                driver_version: None,
                num: 0,
                stale: false,
                error: None,
                delay_us: None,
                gres: vec![GresView {
                    index: 0,
                    name: "NVIDIA A100".to_string(),
                    temperature_c: Some(32),
                    util: 42,
                    mem_used_mb: 1024,
                    mem_total_mb: 8192,
                    processes: Some(vec![ProcessView {
                        uid: 1000,
                        username: "alice".to_string(),
                        pid: 7,
                        command: Some("python".to_string()),
                        used_memory_mb: 512,
                    }]),
                }],
            }],
        };

        let rendered = render_json(&resp).expect("json output");
        let decoded: serde_json::Value = serde_json::from_str(&rendered).expect("valid json");
        assert_eq!(decoded["meta"]["status"], "unknown");
        assert_eq!(decoded["nodes"][0]["hostname"], "node-a");
        assert_eq!(decoded["nodes"][0]["stale"], false);
        assert_eq!(decoded["nodes"][0]["gres"][0]["util"], 42);
        assert_eq!(
            decoded["nodes"][0]["gres"][0]["processes"][0]["username"],
            "alice"
        );
    }

    #[test]
    fn empty_json_output_keeps_schema() {
        let resp = QueryResponse {
            meta: Default::default(),
            nodes: Vec::new(),
        };

        let rendered = render_json(&resp).expect("json output");
        let decoded: serde_json::Value = serde_json::from_str(&rendered).expect("valid json");
        assert_eq!(decoded["meta"]["status"], "unknown");
        assert_eq!(decoded["nodes"].as_array().unwrap().len(), 0);
    }
}
