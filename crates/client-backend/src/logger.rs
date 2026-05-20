use chrono::Local;

fn prefix(level: &str) -> String {
    format!(
        "[{}][client-backend][{}]",
        Local::now().format("%Y-%m-%d %H:%M:%S"),
        level
    )
}

pub fn info(message: impl AsRef<str>) {
    eprintln!("{} {}", prefix("info"), message.as_ref());
}

pub fn warn(message: impl AsRef<str>) {
    eprintln!("{} {}", prefix("warn"), message.as_ref());
}

pub fn fatal(message: impl AsRef<str>) {
    eprintln!("{} {}", prefix("fatal"), message.as_ref());
}

pub fn transport_info(protocol: &str, message: impl AsRef<str>) {
    info(format!("protocol={} {}", protocol, message.as_ref()));
}

pub fn transport_warn(protocol: &str, message: impl AsRef<str>) {
    warn(format!("protocol={} {}", protocol, message.as_ref()));
}
