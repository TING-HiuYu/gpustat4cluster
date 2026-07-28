use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliOptions {
    pub node_filter: Option<String>,
    pub user_filter: Option<String>,
    pub watch: bool,
    pub refresh_interval: Duration,
    pub json: bool,
    pub force_color: bool,
    pub no_color: bool,
    pub show_header: bool,
    pub no_processes: bool,
    pub show_cmd: bool,
    pub show_user: bool,
    pub show_pid: bool,
    pub gresname_width: Option<usize>,
    pub latency_display: Option<bool>,
    pub backend_socket: Option<String>,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            node_filter: None,
            user_filter: None,
            watch: false,
            refresh_interval: Duration::from_secs(2),
            json: false,
            force_color: false,
            no_color: false,
            show_header: true,
            no_processes: false,
            show_cmd: false,
            show_user: false,
            show_pid: false,
            gresname_width: None,
            latency_display: None,
            backend_socket: None,
        }
    }
}

pub fn parse_args(args: Vec<String>) -> Result<CliOptions, String> {
    let mut opts = CliOptions::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => return Err(help_text().to_string()),
            "-v" | "--version" => return Err("clustat 1.0.0".to_string()),
            "-n" => {
                i += 1;
                opts.node_filter = Some(required_value(&args, i, "-n")?.to_string());
            }
            "-user" => {
                i += 1;
                opts.user_filter = Some(required_value(&args, i, "-user")?.to_string());
            }
            "--watch" | "-w" => {
                opts.watch = true;
                if let Some(next) = args.get(i + 1) {
                    if !next.starts_with('-') {
                        i += 1;
                        opts.refresh_interval = parse_interval(next)?;
                    }
                }
            }
            "--interval" | "--refresh" | "--refresh-interval" | "-i" => {
                opts.watch = true;
                if let Some(next) = args.get(i + 1) {
                    if !next.starts_with('-') {
                        i += 1;
                        opts.refresh_interval = parse_interval(next)?;
                    }
                }
            }
            "--json" => {
                opts.json = true;
            }
            "--force-color" | "--color" => {
                opts.force_color = true;
            }
            "--no-color" => {
                opts.no_color = true;
            }
            "--no-header" => {
                opts.show_header = false;
            }
            "--no-processes" => {
                opts.no_processes = true;
            }
            "-c" | "--show-cmd" => {
                opts.show_cmd = true;
            }
            "-u" | "--show-user" => {
                opts.show_user = true;
            }
            "-p" | "--show-pid" => {
                opts.show_pid = true;
            }
            "-a" | "--show-all" => {
                opts.show_cmd = true;
                opts.show_user = true;
                opts.show_pid = true;
            }
            "--latency-display" => {
                i += 1;
                opts.latency_display =
                    Some(parse_bool(required_value(&args, i, "--latency-display")?)?);
            }
            "--no-latency" | "--hide-latency" => {
                opts.latency_display = Some(false);
            }
            "--gpuname-width" => {
                i += 1;
                let raw = required_value(&args, i, "--gpuname-width")?;
                opts.gresname_width = Some(raw.parse().map_err(|_| {
                    format!(
                        "--gpuname-width must be a non-negative integer, got '{}'",
                        raw
                    )
                })?);
            }
            "--backend-socket" | "--backend-uds" => {
                i += 1;
                opts.backend_socket =
                    Some(required_value(&args, i, args[i - 1].as_str())?.to_string());
            }
            other => return Err(format!("unsupported arg: {}", other)),
        }
        i += 1;
    }
    if opts.force_color && opts.no_color {
        return Err("--color and --no-color can't be used at the same time".to_string());
    }
    if opts.watch && opts.json {
        return Err("--json and --interval/-i can't be used together".to_string());
    }
    Ok(opts)
}

pub fn help_text() -> &'static str {
    "Usage: clustat [OPTIONS]\n\nOptions:\n  -n <FILTER>                 Filter cluster nodes by hostname/ip/connection id\n  -user <USER>                Filter rendered processes/GPU rows by user\n  --backend-socket <PATH>     Connect to client-backend UDS path\n  --json                      Print cluster data as JSON\n  -i, --interval [SEC]        Watch mode; optional refresh interval in seconds\n  -w, --watch [SEC]           Alias for --interval\n  --color, --force-color      Force ANSI colored output\n  --no-color                  Suppress ANSI colored output\n  --no-header                 Suppress node header lines\n  --gpuname-width <N>         GPU name display width; 0 hides GPU names\n  --no-processes              Hide process summaries\n  -c, --show-cmd              Display process command instead of user\n  -u, --show-user             Display process user with command\n  -p, --show-pid              Display process PID\n  -a, --show-all              Show user, command and PID\n  -h, --help                  Show this help\n  -v, --version               Show version"
}

fn required_value<'a>(args: &'a [String], index: usize, flag: &str) -> Result<&'a str, String> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| format!("{} requires a value", flag))
}

fn parse_interval(raw: &str) -> Result<Duration, String> {
    let secs: f64 = raw
        .parse()
        .map_err(|_| format!("refresh interval must be seconds, got '{}'", raw))?;
    if secs <= 0.0 || !secs.is_finite() {
        return Err("refresh interval must be greater than 0".to_string());
    }
    Ok(Duration::from_secs_f64(secs.max(0.05)))
}

fn parse_bool(raw: &str) -> Result<bool, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!("boolean value expected, got '{}'", raw)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_node_and_user_filters() {
        let opts = parse_args(args(&["-n", "node[01-02]", "-user", "alice"])).unwrap();
        assert_eq!(opts.node_filter.as_deref(), Some("node[01-02]"));
        assert_eq!(opts.user_filter.as_deref(), Some("alice"));
        assert!(!opts.watch);
    }

    #[test]
    fn parses_watch_default_interval() {
        let opts = parse_args(args(&["--watch"])).unwrap();
        assert!(opts.watch);
        assert_eq!(opts.refresh_interval, Duration::from_secs(2));
    }

    #[test]
    fn parses_watch_with_inline_interval() {
        let opts = parse_args(args(&["-w", "5"])).unwrap();
        assert!(opts.watch);
        assert_eq!(opts.refresh_interval, Duration::from_secs(5));
    }

    #[test]
    fn parses_explicit_refresh_interval() {
        let opts = parse_args(args(&["--interval", "3"])).unwrap();
        assert!(opts.watch);
        assert_eq!(opts.refresh_interval, Duration::from_secs(3));
    }

    #[test]
    fn parses_interval_flag_without_value_as_watch() {
        let opts = parse_args(args(&["-i"])).unwrap();
        assert!(opts.watch);
        assert_eq!(opts.refresh_interval, Duration::from_secs(2));
    }

    #[test]
    fn parses_fractional_refresh_interval() {
        let opts = parse_args(args(&["-i", "0.05"])).unwrap();
        assert!(opts.watch);
        assert_eq!(opts.refresh_interval, Duration::from_millis(50));
    }

    #[test]
    fn clamps_refresh_interval_to_50ms() {
        let opts = parse_args(args(&["-i", "0.001"])).unwrap();
        assert!(opts.watch);
        assert_eq!(opts.refresh_interval, Duration::from_millis(50));
    }

    #[test]
    fn parses_latency_display_flag() {
        let opts = parse_args(args(&["--latency-display", "false"])).unwrap();
        assert_eq!(opts.latency_display, Some(false));
        let opts = parse_args(args(&["--no-latency"])).unwrap();
        assert_eq!(opts.latency_display, Some(false));
    }

    #[test]
    fn parses_json_output_flag() {
        let opts = parse_args(args(&["--json", "-n", "node-a"])).unwrap();
        assert!(opts.json);
        assert_eq!(opts.node_filter.as_deref(), Some("node-a"));
    }

    #[test]
    fn parses_backend_socket() {
        let opts = parse_args(args(&["--backend-socket", "/tmp/clustat.sock"])).unwrap();
        assert_eq!(opts.backend_socket.as_deref(), Some("/tmp/clustat.sock"));
    }
}
