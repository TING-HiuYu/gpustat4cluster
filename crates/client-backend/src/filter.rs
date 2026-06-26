use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeFilter {
    patterns: Vec<String>,
}

impl NodeFilter {
    pub fn parse(raw: Option<&str>) -> Self {
        let mut seen = HashSet::new();
        let patterns = raw
            .into_iter()
            .flat_map(split_filter_items)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .flat_map(|item| expand_bracket_expression(&item))
            .filter(|pattern| seen.insert(pattern.clone()))
            .collect();
        Self { patterns }
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    pub fn matches_target(&self, hostname: &str, addr: SocketAddr, connection_id: &str) -> bool {
        if self.is_empty() {
            return true;
        }

        let ip = addr.ip().to_string();
        let addr_string = addr.to_string();
        self.patterns.iter().any(|pattern| {
            if is_exact_addr_pattern(pattern) {
                return exact_addr_match(pattern, addr);
            }
            exact_addr_match(pattern, addr)
                || wildcard_match(pattern, hostname)
                || wildcard_match(pattern, &ip)
                || wildcard_match(pattern, &addr_string)
                || wildcard_match(pattern, connection_id)
        })
    }

    #[cfg(test)]
    fn patterns(&self) -> &[String] {
        &self.patterns
    }
}

fn split_filter_items(raw: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut start = 0;
    let mut bracket_depth: usize = 0;

    for (idx, ch) in raw.char_indices() {
        match ch {
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            ',' if bracket_depth == 0 => {
                items.push(&raw[start..idx]);
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    items.push(&raw[start..]);
    items
}

fn expand_bracket_expression(raw: &str) -> Vec<String> {
    let Some(open) = raw.find('[') else {
        return vec![raw.to_string()];
    };
    let Some(close_rel) = raw[open + 1..].find(']') else {
        return vec![raw.to_string()];
    };
    let close = open + 1 + close_rel;
    let expression = &raw[open + 1..close];
    let prefix = &raw[..open];
    let suffix = &raw[close + 1..];

    if let Some((start, end)) = expression.split_once('-') {
        return expand_numeric_range(raw, prefix, suffix, start, end);
    }

    let list: Vec<_> = expression
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| format!("{}{}{}", prefix, item, suffix))
        .collect();

    if list.is_empty() {
        vec![raw.to_string()]
    } else {
        list
    }
}

fn expand_numeric_range(
    raw: &str,
    prefix: &str,
    suffix: &str,
    start: &str,
    end: &str,
) -> Vec<String> {
    let Ok(start_num) = start.parse::<u32>() else {
        return vec![raw.to_string()];
    };
    let Ok(end_num) = end.parse::<u32>() else {
        return vec![raw.to_string()];
    };
    if start_num > end_num {
        return vec![raw.to_string()];
    }

    let width = start.len().max(end.len());
    (start_num..=end_num)
        .map(|n| format!("{}{:0width$}{}", prefix, n, suffix, width = width))
        .collect()
}

fn exact_addr_match(pattern: &str, addr: SocketAddr) -> bool {
    if let Ok(pattern_addr) = pattern.parse::<SocketAddr>() {
        return pattern_addr == addr;
    }
    if let Ok(pattern_ip) = pattern.parse::<IpAddr>() {
        return pattern_ip == addr.ip();
    }
    false
}

fn is_exact_addr_pattern(pattern: &str) -> bool {
    !pattern.contains('*')
        && (pattern.parse::<SocketAddr>().is_ok() || pattern.parse::<IpAddr>().is_ok())
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return text.contains(pattern);
    }

    let mut rest = text;
    let mut first = true;
    for part in pattern.split('*') {
        if part.is_empty() {
            first = false;
            continue;
        }
        if first && !pattern.starts_with('*') {
            let Some(next) = rest.strip_prefix(part) else {
                return false;
            };
            rest = next;
        } else {
            let Some(pos) = rest.find(part) else {
                return false;
            };
            rest = &rest[pos + part.len()..];
        }
        first = false;
    }

    pattern.ends_with('*') || rest.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(raw: &str) -> SocketAddr {
        raw.parse().unwrap()
    }

    #[test]
    fn empty_filter_matches_everything() {
        let filter = NodeFilter::parse(None);
        assert!(filter.matches_target("node-a", addr("10.0.0.1:30000"), "conn-001"));
    }

    #[test]
    fn comma_filter_matches_hostname_or_ip() {
        let filter = NodeFilter::parse(Some("node-b,10.0.0.1"));
        assert!(filter.matches_target("node-b", addr("192.168.1.1:30000"), "conn-001"));
        assert!(filter.matches_target("node-a", addr("10.0.0.1:30000"), "conn-002"));
        assert!(!filter.matches_target("node-c", addr("10.0.0.2:30000"), "conn-003"));
    }

    #[test]
    fn ip_filter_is_exact_for_plain_ip_patterns() {
        let filter = NodeFilter::parse(Some("10.0.0.1"));
        assert!(filter.matches_target("node-a", addr("10.0.0.1:30000"), "conn-001"));
        assert!(!filter.matches_target("node-b", addr("10.0.0.10:30000"), "conn-002"));
    }

    #[test]
    fn wildcard_filter_matches_hostname_suffix_and_ip() {
        let filter = NodeFilter::parse(Some("*.cluster,10.1.*"));
        assert!(filter.matches_target("gres-a.cluster", addr("192.168.1.1:30000"), "conn-001"));
        assert!(filter.matches_target("cpu-a", addr("10.1.2.3:30000"), "conn-002"));
        assert!(!filter.matches_target("cpu-a", addr("10.2.2.3:30000"), "conn-003"));
    }

    #[test]
    fn bracket_range_preserves_zero_padding() {
        let filter = NodeFilter::parse(Some("node[01-03]"));
        assert!(filter.matches_target("node01", addr("10.0.0.1:30000"), "conn-001"));
        assert!(filter.matches_target("node03", addr("10.0.0.3:30000"), "conn-003"));
        assert!(!filter.matches_target("node4", addr("10.0.0.4:30000"), "conn-004"));
    }

    #[test]
    fn bracket_list_expands_without_splitting_inner_commas() {
        let filter = NodeFilter::parse(Some("node[1,3,5]"));
        assert_eq!(filter.patterns(), &["node1", "node3", "node5"]);
        assert!(filter.matches_target("node3", addr("10.0.0.3:30000"), "conn-003"));
        assert!(!filter.matches_target("node4", addr("10.0.0.4:30000"), "conn-004"));
    }

    #[test]
    fn comma_filter_deduplicates_expanded_patterns() {
        let filter = NodeFilter::parse(Some("node1,node[1,3],node3"));
        assert_eq!(filter.patterns(), &["node1", "node3"]);
    }

    #[test]
    fn filter_can_match_connection_id() {
        let filter = NodeFilter::parse(Some("conn-007"));
        assert!(filter.matches_target("node-a", addr("10.0.0.1:30000"), "conn-007"));
    }
}
