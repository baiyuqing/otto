//! Current-process resource sampling for Prometheus exposition.

use std::fmt::Write as _;

#[derive(Debug, Default)]
pub(super) struct Sample {
    pub resident_memory_bytes: Option<u64>,
    pub virtual_memory_bytes: Option<u64>,
    pub cpu_seconds_total: Option<f64>,
    pub threads: Option<u64>,
    pub open_fds: Option<u64>,
    pub failures: Vec<&'static str>,
}

pub(super) fn write(out: &mut String, sample: &Sample) {
    write_optional_gauge(
        out,
        "process_resident_memory_bytes",
        "Resident memory size of the Otto process in bytes.",
        sample.resident_memory_bytes,
    );
    write_optional_gauge(
        out,
        "process_virtual_memory_bytes",
        "Virtual memory size of the Otto process in bytes.",
        sample.virtual_memory_bytes,
    );
    super::write_help(
        out,
        "process_cpu_seconds_total",
        "counter",
        "Total user and system CPU time consumed by the Otto process in seconds.",
    );
    if let Some(value) = sample.cpu_seconds_total {
        let _ = writeln!(
            out,
            "process_cpu_seconds_total {}",
            super::format_float(value)
        );
    }
    write_optional_gauge(
        out,
        "process_threads",
        "Number of operating system threads in the Otto process.",
        sample.threads,
    );
    write_optional_gauge(
        out,
        "process_open_fds",
        "Number of open file descriptors in the Otto process.",
        sample.open_fds,
    );
}

fn write_optional_gauge(out: &mut String, name: &str, help: &str, value: Option<u64>) {
    super::write_help(out, name, "gauge", help);
    if let Some(value) = value {
        let _ = writeln!(out, "{name} {value}");
    }
}

pub(super) fn sample() -> Sample {
    let mut sample = Sample::default();
    sample_cpu(&mut sample);
    sample_platform(&mut sample);
    sample
}

#[allow(unsafe_code)]
fn sample_cpu(sample: &mut Sample) {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `usage` points to writable storage of the exact type getrusage expects.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) } != 0 {
        sample.failures.push("cpu");
        return;
    }
    sample.cpu_seconds_total =
        Some(timeval_seconds(usage.ru_utime) + timeval_seconds(usage.ru_stime));
}

fn timeval_seconds(value: libc::timeval) -> f64 {
    value.tv_sec as f64 + value.tv_usec as f64 / 1_000_000.0
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn sample_platform(sample: &mut Sample) {
    let pid = std::process::id() as libc::c_int;
    let mut task: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let task_size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: `task` is writable for `task_size` bytes and the flavor matches its type.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            (&raw mut task).cast(),
            task_size,
        )
    };
    if written == task_size {
        sample.resident_memory_bytes = Some(task.pti_resident_size);
        sample.virtual_memory_bytes = Some(task.pti_virtual_size);
        sample.threads = u64::try_from(task.pti_threadnum).ok();
        if sample.threads.is_none() {
            sample.failures.push("threads");
        }
    } else {
        sample
            .failures
            .extend(["resident_memory", "virtual_memory", "threads"]);
    }

    // With a null buffer, PROC_PIDLISTFDS returns the bytes required for the list.
    // SAFETY: a null buffer and zero size request only the required buffer length.
    let fd_bytes =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if fd_bytes >= 0 {
        sample.open_fds = Some(fd_bytes as u64 / std::mem::size_of::<libc::proc_fdinfo>() as u64);
    } else {
        sample.failures.push("open_fds");
    }
}

#[cfg(target_os = "linux")]
fn sample_platform(sample: &mut Sample) {
    sample_linux_stat(sample);
    sample_linux_memory(sample);
    match std::fs::read_dir("/proc/self/fd") {
        Ok(entries) => {
            let count = entries.filter_map(Result::ok).count();
            sample.open_fds = Some(count.saturating_sub(1) as u64);
        }
        Err(_) => sample.failures.push("open_fds"),
    }
}

#[cfg(target_os = "linux")]
fn sample_linux_stat(sample: &mut Sample) {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        sample.failures.push("threads");
        return;
    };
    let Some(after_comm) = stat.rsplit_once(") ").map(|(_, fields)| fields) else {
        sample.failures.push("threads");
        return;
    };
    // state=0 ... num_threads=17 after the parenthesized command field.
    sample.threads = after_comm
        .split_whitespace()
        .nth(17)
        .and_then(|value| value.parse().ok());
    if sample.threads.is_none() {
        sample.failures.push("threads");
    }
}

#[cfg(target_os = "linux")]
fn sample_linux_memory(sample: &mut Sample) {
    let Some(page_size) = positive_sysconf(libc::_SC_PAGESIZE) else {
        sample
            .failures
            .extend(["resident_memory", "virtual_memory"]);
        return;
    };
    let Ok(statm) = std::fs::read_to_string("/proc/self/statm") else {
        sample
            .failures
            .extend(["resident_memory", "virtual_memory"]);
        return;
    };
    let mut fields = statm.split_whitespace();
    sample.virtual_memory_bytes = fields
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(|pages| pages.checked_mul(page_size));
    sample.resident_memory_bytes = fields
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(|pages| pages.checked_mul(page_size));
    if sample.virtual_memory_bytes.is_none() {
        sample.failures.push("virtual_memory");
    }
    if sample.resident_memory_bytes.is_none() {
        sample.failures.push("resident_memory");
    }
}

#[cfg(target_os = "linux")]
fn positive_sysconf(name: libc::c_int) -> Option<u64> {
    // SAFETY: sysconf accepts this constant and has no pointer arguments.
    u64::try_from(unsafe { libc::sysconf(name) })
        .ok()
        .filter(|value| *value > 0)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn sample_platform(sample: &mut Sample) {
    sample
        .failures
        .extend(["resident_memory", "virtual_memory", "threads", "open_fds"]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_samples_keep_metadata_and_other_values() {
        let sample = Sample {
            resident_memory_bytes: Some(4096),
            cpu_seconds_total: Some(1.25),
            failures: vec!["virtual_memory", "threads", "open_fds"],
            ..Sample::default()
        };
        let mut body = String::new();
        write(&mut body, &sample);
        assert!(body.contains("process_resident_memory_bytes 4096"));
        assert!(body.contains("process_cpu_seconds_total 1.25"));
        assert!(body.contains("# TYPE process_virtual_memory_bytes gauge"));
        assert!(
            !body
                .lines()
                .any(|line| line.starts_with("process_virtual_memory_bytes "))
        );
        assert!(
            !body
                .lines()
                .any(|line| line.starts_with("process_threads "))
        );
        assert!(
            !body
                .lines()
                .any(|line| line.starts_with("process_open_fds "))
        );
    }
}
