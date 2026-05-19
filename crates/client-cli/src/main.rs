use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;

use serde::Deserialize;

const BACKEND_ADDR: &str = "127.0.0.1:4521";

#[derive(Debug, Deserialize)]
struct QueryResponse {
    nodes: Vec<NodeView>,
}

#[derive(Debug, Deserialize)]
struct NodeView {
    hostname: String,
    gpus: Vec<GpuView>,
}

#[derive(Debug, Deserialize)]
struct GpuView {
    index: u8,
    util: u8,
    mem_used_mb: u32,
    mem_total_mb: u32,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("[gpustat4cluster][error] {}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let (node_filter, user_filter) = parse_args(std::env::args().skip(1).collect())?;
    let mut stream = TcpStream::connect(BACKEND_ADDR).map_err(|e| {
        format!(
            "backend 未运行：请先启动 gpustat4cluster-client-backend（{}）。连接失败: {}",
            BACKEND_ADDR, e
        )
    })?;

    let req = serde_json::json!({ "filter": node_filter, "user": user_filter });
    let cmd = format!("QUERY {}\n", req);
    stream
        .write_all(cmd.as_bytes())
        .map_err(|e| format!("send QUERY failed: {}", e))?;

    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read QUERY response failed: {}", e))?;

    let resp: QueryResponse =
        serde_json::from_str(line.trim()).map_err(|e| format!("invalid backend response: {}", e))?;
    render_table(&resp);
    Ok(())
}

fn parse_args(args: Vec<String>) -> Result<(Option<String>, Option<String>), String> {
    let mut node_filter = None;
    let mut user_filter = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" => {
                i += 1;
                let v = args.get(i).ok_or_else(|| "-n requires a value".to_string())?;
                node_filter = Some(v.clone());
            }
            "-user" => {
                i += 1;
                let v = args.get(i).ok_or_else(|| "-user requires a value".to_string())?;
                user_filter = Some(v.clone());
            }
            other => return Err(format!("unsupported arg: {}", other)),
        }
        i += 1;
    }
    Ok((node_filter, user_filter))
}

fn render_table(resp: &QueryResponse) {
    println!("{:<20} {:<6} {:<8}", "HOSTNAME", "GPU", "MEM(MB)");
    println!("{:-<20} {:-<6} {:-<8}", "", "", "");
    for node in &resp.nodes {
        for gpu in &node.gpus {
            println!(
                "{:<20} {:>3}% {:>4}/{:<4}",
                node.hostname,
                gpu.util,
                gpu.mem_used_mb,
                gpu.mem_total_mb
            );
            let _ = gpu.index;
        }
    }
}
