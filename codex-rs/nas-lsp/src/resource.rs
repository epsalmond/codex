//! Enforceable aggregate resource policy. See `.codex/design/nas-lsp-broker.md`.

use crate::config::PoolLimits;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct ResourceLimits {
    pub cgroup: PathBuf,
    pub memory_max_bytes: u64,
    pub pids_max: u64,
}
fn unavailable() -> io::Error {
    io::Error::other("resource_limit_unavailable")
}

impl ResourceLimits {
    /// Read the actual unified cgroup membership and mounted controller files.
    /// A sampled RSS value can never substitute for these kernel-enforced limits.
    pub fn validate(limits: &PoolLimits) -> io::Result<Self> {
        if !cfg!(target_os = "linux") {
            return Err(unavailable());
        }
        let membership = fs::read_to_string("/proc/self/cgroup")?;
        let group = membership
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .ok_or_else(unavailable)?;
        if group == "/"
            || !group.starts_with('/')
            || Path::new(group)
                .components()
                .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(unavailable());
        }
        let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
        let directory = locate_cgroup(group, &mountinfo).ok_or_else(unavailable)?;
        Self::at(directory, limits)
    }

    fn at(cgroup: PathBuf, limits: &PoolLimits) -> io::Result<Self> {
        let memory_max_bytes = finite_limit(&cgroup.join("memory.max"))?;
        let pids_max = finite_limit(&cgroup.join("pids.max"))?;
        if memory_max_bytes > limits.memory_max_bytes || pids_max > limits.pids_max {
            return Err(unavailable());
        }
        Ok(Self {
            cgroup,
            memory_max_bytes,
            pids_max,
        })
    }
    pub fn under_pressure(&self) -> bool {
        fs::read_to_string(self.cgroup.join("memory.current"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .is_some_and(|used| u128::from(used) * 100 >= u128::from(self.memory_max_bytes) * 80)
    }
}
fn finite_limit(path: &Path) -> io::Result<u64> {
    let value = fs::read_to_string(path)?
        .trim()
        .parse::<u64>()
        .map_err(|_| unavailable())?;
    if value == 0 {
        return Err(unavailable());
    }
    Ok(value)
}
fn locate_cgroup(group: &str, mountinfo: &str) -> Option<PathBuf> {
    for line in mountinfo.lines() {
        let (left, right) = line.split_once(" - ")?;
        if right.split_whitespace().next() != Some("cgroup2") {
            continue;
        }
        let fields: Vec<_> = left.split_whitespace().collect();
        if fields.len() < 5 {
            continue;
        }
        let root = unescape_mount(fields[3]);
        let mount = unescape_mount(fields[4]);
        let relative = Path::new(group).strip_prefix(&root).ok()?;
        return Some(Path::new(&mount).join(relative));
    }
    None
}
fn unescape_mount(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

/// Observability only; includes descendants and never authorizes admission.
pub fn process_tree_rss(root_pid: u32) -> io::Result<u64> {
    let mut processes = Vec::new();
    for entry in fs::read_dir("/proc")?.take(65536) {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((_, tail)) = stat.rsplit_once(')') else {
            continue;
        };
        let Some(ppid) = tail
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        processes.push((pid, ppid));
    }
    let mut descendants = HashSet::from([root_pid]);
    loop {
        let before = descendants.len();
        for &(pid, parent) in &processes {
            if descendants.contains(&parent) {
                descendants.insert(pid);
            }
        }
        if before == descendants.len() {
            break;
        }
    }
    let mut bytes = 0u64;
    for pid in descendants {
        if let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) {
            if let Some(kib) = status
                .lines()
                .find_map(|s| s.strip_prefix("VmRSS:"))
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse::<u64>().ok())
            {
                bytes = bytes.saturating_add(kib.saturating_mul(1024));
            }
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unbounded_or_excessive_kernel_limits() {
        let dir = tempfile::tempdir().expect("temp");
        fs::write(dir.path().join("memory.max"), "max\n").expect("write");
        fs::write(dir.path().join("pids.max"), "64\n").expect("write");
        assert!(ResourceLimits::at(dir.path().to_owned(), &PoolLimits::default()).is_err());
        fs::write(dir.path().join("memory.max"), "1073741824\n").expect("write");
        assert!(ResourceLimits::at(dir.path().to_owned(), &PoolLimits::default()).is_ok());
        fs::write(dir.path().join("pids.max"), "65\n").expect("write");
        assert!(ResourceLimits::at(dir.path().to_owned(), &PoolLimits::default()).is_err());
    }
    #[test]
    fn resolves_mount_root_and_namespace_membership() {
        let mounts = "1 2 0:1 /user.slice /sys/fs/cgroup rw - cgroup2 cgroup rw";
        assert_eq!(
            locate_cgroup("/user.slice/a.scope", mounts),
            Some(PathBuf::from("/sys/fs/cgroup/a.scope"))
        );
    }
}
