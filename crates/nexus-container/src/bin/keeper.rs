#![forbid(unsafe_code)]

use std::{ffi::OsStr, fs, thread, time::Duration};

const KEEPER_PATH: &str = "/usr/local/bin/nexus-keeper";

fn main() {
    let mut args = std::env::args_os();
    let _program = args.next();
    match args.next().as_deref() {
        Some(arg) if arg == OsStr::new("run") && args.next().is_none() => run(),
        Some(arg) if arg == OsStr::new("healthcheck") && args.next().is_none() => {
            if healthy_pid_one() {
                std::process::exit(0);
            }
            std::process::exit(1);
        }
        _ => std::process::exit(2),
    }
}

fn run() -> ! {
    loop {
        thread::park_timeout(Duration::from_secs(60));
    }
}

fn healthy_pid_one() -> bool {
    let Ok(children) = fs::read_to_string("/proc/1/task/1/children") else {
        return false;
    };
    let mut children = children.split_whitespace();
    let Some(pid) = children.next() else {
        return false;
    };
    if children.next().is_some() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let Ok(command_line) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    command_is_keeper(&command_line)
}

fn command_is_keeper(command_line: &[u8]) -> bool {
    let mut args = command_line.split(|byte| *byte == 0);
    args.next() == Some(KEEPER_PATH.as_bytes())
        && args.next() == Some(b"run".as_slice())
        && args.next().is_some_and(|arg| arg.is_empty())
        && args.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthcheck_requires_the_exact_pid_one_command() {
        assert!(command_is_keeper(b"/usr/local/bin/nexus-keeper\0run\0"));
        assert!(!command_is_keeper(b"/usr/local/bin/other\0run\0"));
        assert!(!command_is_keeper(
            b"/usr/local/bin/nexus-keeper\0healthcheck\0"
        ));
        assert!(!command_is_keeper(
            b"/usr/local/bin/nexus-keeper\0run\0extra\0"
        ));
    }
}
