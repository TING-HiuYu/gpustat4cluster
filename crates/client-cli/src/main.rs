use std::{
    io::{self, Write},
    thread,
    time::Duration,
};

mod args;
mod backend;
mod render;

fn main() {
    if let Err(e) = run() {
        if e.starts_with("Usage:") || e.starts_with("clustat ") {
            println!("{}", e);
            return;
        }
        eprintln!("[clustat][error] {}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let opts = args::parse_args(std::env::args().skip(1).collect())?;
    if opts.watch {
        let mut connection = backend::connect_backend(&opts)?;
        run_refresh_loop(&opts, || connection.query(&opts), None, thread::sleep).map(|_| ())
    } else {
        run_refresh_loop(&opts, || backend::query_backend(&opts), None, thread::sleep).map(|_| ())
    }
}

fn run_refresh_loop<F, S>(
    opts: &args::CliOptions,
    mut fetch: F,
    max_rounds: Option<usize>,
    mut sleep: S,
) -> Result<usize, String>
where
    F: FnMut() -> Result<backend::QueryResponse, String>,
    S: FnMut(Duration),
{
    let mut renderer = render::IncrementalRenderer::default();
    let render_opts = render::RenderOptions::from_cli(opts);
    let mut rounds = 0;

    loop {
        let resp = fetch()?;
        let mut out = String::new();
        if opts.watch {
            out.push_str("\x1b[2J\x1b[H");
        }
        if opts.json {
            out.push_str(&render::render_json(&resp)?);
        } else {
            out.push_str(&renderer.render_full(&resp, opts.user_filter.as_deref(), &render_opts));
        }
        if !write_stdout(&out)? {
            break;
        }
        rounds += 1;

        if !opts.watch || max_rounds.is_some_and(|max| rounds >= max) {
            break;
        }
        sleep(opts.refresh_interval);
    }

    Ok(rounds)
}

fn write_stdout(output: &str) -> Result<bool, String> {
    let mut stdout = io::stdout().lock();
    if let Err(e) = stdout.write_all(output.as_bytes()) {
        if e.kind() == io::ErrorKind::BrokenPipe {
            return Ok(false);
        } else {
            return Err(format!("write output failed: {}", e));
        }
    }
    if let Err(e) = stdout.flush() {
        if e.kind() == io::ErrorKind::BrokenPipe {
            return Ok(false);
        } else {
            return Err(format!("flush output failed: {}", e));
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{GresView, NodeView, QueryResponse};
    use std::cell::Cell;

    #[test]
    fn watch_loop_can_exit_after_two_refresh_rounds() {
        let opts = args::CliOptions {
            watch: true,
            refresh_interval: Duration::from_secs(1),
            ..args::CliOptions::default()
        };
        let fetch_count = Cell::new(0);
        let sleep_count = Cell::new(0);

        let rounds = run_refresh_loop(
            &opts,
            || {
                fetch_count.set(fetch_count.get() + 1);
                Ok(QueryResponse {
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
                            processes: None,
                        }],
                    }],
                })
            },
            Some(2),
            |_| sleep_count.set(sleep_count.get() + 1),
        )
        .expect("watch loop");

        assert_eq!(rounds, 2);
        assert_eq!(fetch_count.get(), 2);
        assert_eq!(sleep_count.get(), 1);
    }

    #[test]
    fn json_mode_refresh_loop_can_render_once() {
        let opts = args::CliOptions {
            json: true,
            ..args::CliOptions::default()
        };

        let rounds = run_refresh_loop(
            &opts,
            || {
                Ok(QueryResponse {
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
                            processes: None,
                        }],
                    }],
                })
            },
            None,
            |_| {},
        )
        .expect("json loop");

        assert_eq!(rounds, 1);
    }
}
