use std::{
    fs,
    io::{self, Read},
    os::fd::AsRawFd,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub fn cpu_sample(text: &str) -> Option<(u64, u64)> {
    let mut fields = text.lines().next()?.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    let times: Vec<u64> = fields.map(str::parse).collect::<Result<_, _>>().ok()?;
    if times.len() < 5 {
        return None;
    }
    let total = times
        .iter()
        .take(8) // guest fields are already included in user/nice.
        .try_fold(0_u64, |sum, time| sum.checked_add(*time))?;
    Some((total, times[3].checked_add(times[4])?))
}

pub fn cpu_percent(before: (u64, u64), after: (u64, u64)) -> Option<u64> {
    let total = after.0.checked_sub(before.0)?;
    let idle = after.1.checked_sub(before.1)?;
    (total > 0 && idle <= total).then(|| ((total - idle) as u128 * 100 / total as u128) as u64)
}

pub fn memory_percent(text: &str) -> Option<u64> {
    let mut total = None;
    let mut available = None;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        match parts.next()? {
            "MemTotal:" => total = parts.next()?.parse::<u64>().ok(),
            "MemAvailable:" => available = parts.next()?.parse::<u64>().ok(),
            _ => {}
        }
    }
    let (total, available) = (total?, available?);
    (total > 0 && available <= total).then(|| (total - available) * 100 / total)
}

pub fn read_cpu() -> Option<(u64, u64)> {
    cpu_sample(&fs::read_to_string("/proc/stat").ok()?)
}
pub fn read_memory() -> Option<String> {
    Some(format!(
        "MEM {}%",
        memory_percent(&fs::read_to_string("/proc/meminfo").ok()?)?
    ))
}

pub fn clean_output(bytes: &[u8]) -> Option<String> {
    if bytes.len() > 256 {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?.trim();
    if text.is_empty() || text.chars().any(char::is_control) {
        return None;
    }
    Some(text.chars().take(64).collect())
}

pub fn run_command(argv: &[String]) -> Option<String> {
    let mut child = Command::new(argv.first()?)
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // SAFETY: fcntl only changes the flags on our owned stdout pipe.
    unsafe {
        let flags = libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0
        {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    }
    let mut bytes = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    let success = loop {
        let mut buffer = [0_u8; 257];
        match stdout.read(&mut buffer) {
            Ok(n) if n > 0 => bytes.extend_from_slice(&buffer[..n]),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
            _ => {}
        }
        if bytes.len() > 256 {
            let _ = child.kill();
            let _ = child.wait();
            break false;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // Drain bytes already buffered when the child exits.
                loop {
                    match stdout.read(&mut buffer) {
                        Ok(n) if n > 0 => bytes.extend_from_slice(&buffer[..n]),
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                        _ => break,
                    }
                    if bytes.len() > 256 {
                        break;
                    }
                }
                break status.success();
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    success.then(|| clean_output(&bytes)).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cpu_uses_deltas_and_memory_uses_available_not_free() {
        assert_eq!(cpu_sample("cpu 10 2 4 30 4 1 1 0 0 0\n"), Some((52, 34)));
        assert_eq!(cpu_percent((100, 80), (200, 130)), Some(50));
        assert_eq!(cpu_percent((100, 80), (100, 80)), None);
        assert_eq!(
            memory_percent("MemTotal: 1000 kB\nMemFree: 5 kB\nMemAvailable: 400 kB\n"),
            Some(60)
        );
        assert_eq!(memory_percent("MemTotal: 0 kB\nMemAvailable: 0 kB\n"), None);
    }

    #[test]
    fn command_output_is_one_line_utf8_and_bounded() {
        assert_eq!(clean_output(b" sun 21C \n"), Some("sun 21C".into()));
        for bytes in [b"".as_slice(), b"bad\nsecond", b"\x1b[31mred", b"\xff"] {
            assert_eq!(clean_output(bytes), None);
        }
        assert_eq!(clean_output(&vec![b'x'; 257]), None);
        assert_eq!(
            run_command(&["/bin/sh".into(), "-c".into(), "printf 'ok\\n'".into()]),
            Some("ok".into())
        );
        assert_eq!(
            run_command(&["/bin/sh".into(), "-c".into(), "exit 1".into()]),
            None
        );
        assert_eq!(run_command(&["/bin/sleep".into(), "3".into()]), None);
        assert_eq!(
            run_command(&["/bin/sh".into(), "-c".into(), "printf '\\377'".into()]),
            None
        );
        assert_eq!(
            run_command(&["/bin/sh".into(), "-c".into(), "yes x".into()]),
            None
        );
    }
}
