//! Linux terminal readiness supplements bounded periodic parser draining.
//!
//! Crossterm remains the sole reader/parser. The separate nonblocking file
//! description only observes readiness and never changes stdin's flags.

use std::io;

#[derive(Default)]
pub struct InputReady {
    #[cfg(target_os = "linux")]
    fd: Option<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>>,
    #[cfg(target_os = "linux")]
    armed: bool,
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    fn fixture() -> (InputReady, UnixStream) {
        let (reader, writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        (InputReady::from_fd(reader.into()).unwrap(), writer)
    }

    #[tokio::test]
    async fn cancelled_wait_and_stale_readiness_do_not_lose_later_input() {
        let (input, mut writer) = fixture();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), input.ready())
                .await
                .is_err()
        );
        for _ in 0..2 {
            writer.write_all(b"x").unwrap();
            tokio::time::timeout(Duration::from_secs(1), input.ready())
                .await
                .unwrap()
                .unwrap();
            let mut byte = [0];
            assert_eq!(
                rustix::io::read(input.fd.as_ref().unwrap().get_ref(), &mut byte).unwrap(),
                1
            );
            assert_eq!(byte, [b'x']);
            assert!(
                tokio::time::timeout(Duration::from_millis(10), input.ready())
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn readable_input_is_suppressed_during_the_batch_cooldown() {
        let (mut input, mut writer) = fixture();
        writer.write_all(b"still readable").unwrap();
        tokio::time::timeout(Duration::from_secs(1), input.ready())
            .await
            .unwrap()
            .unwrap();
        let mut interval = tokio::time::interval(Duration::from_millis(20));
        input.defer(&mut interval);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), input.ready())
                .await
                .is_err()
        );
        input.arm();
        tokio::time::timeout(Duration::from_secs(1), input.ready())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn closed_input_reports_an_error_instead_of_spinning() {
        let (input, writer) = fixture();
        drop(writer);
        let error = tokio::time::timeout(Duration::from_secs(1), input.ready())
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }
}

impl InputReady {
    pub fn new() -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            use std::io::IsTerminal;
            if !io::stdin().is_terminal() {
                return Ok(Self::default());
            }
            // Reopening gives a separate file description; dup would share
            // O_NONBLOCK with Crossterm's stdin. /dev/tty permits a process
            // to observe its controlling terminal even when an unprivileged
            // launcher inherited a PTY owned by another user. A PTY with no
            // controlling session instead needs the stdin alias.
            let session = rustix::termios::tcgetsid(io::stdin());
            let path = if session.is_ok() {
                "/dev/tty"
            } else {
                "/proc/self/fd/0"
            };
            let fd = rustix::fs::open(
                path,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )?;
            if let Ok(session) = session
                && rustix::termios::tcgetsid(&fd)? != session
            {
                return Err(io::Error::other("terminal input session changed"));
            }
            Self::from_fd(fd)
        }
        #[cfg(not(target_os = "linux"))]
        Ok(Self::default())
    }

    #[cfg(target_os = "linux")]
    fn from_fd(fd: std::os::fd::OwnedFd) -> io::Result<Self> {
        Ok(Self {
            fd: Some(tokio::io::unix::AsyncFd::with_interest(
                fd,
                tokio::io::Interest::READABLE,
            )?),
            armed: true,
        })
    }

    pub fn arm(&mut self) {
        #[cfg(target_os = "linux")]
        {
            self.armed = true;
        }
    }

    /// Keep continuous input at the existing batch cadence, even when the
    /// kernel remains readable. The periodic path also drains parsed events
    /// that Crossterm retained after consuming the underlying bytes.
    pub fn defer(&mut self, _interval: &mut tokio::time::Interval) {
        #[cfg(target_os = "linux")]
        if self.fd.is_some() {
            self.armed = false;
            _interval.reset();
        }
    }

    pub async fn ready(&self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        if let Some(fd) = &self.fd
            && self.armed
        {
            use rustix::event::{PollFd, PollFlags, Timespec, poll};
            loop {
                let mut guard = fd.readable().await?;
                let mut fds = [PollFd::new(fd.get_ref(), PollFlags::IN)];
                match poll(&mut fds, Some(&Timespec::default())) {
                    Ok(_) => {}
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(error) => return Err(error.into()),
                }
                let events = fds[0].revents();
                if events.intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL) {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "terminal input closed",
                    ));
                }
                if events.contains(PollFlags::IN) {
                    // Keep readiness until Crossterm drains the bytes. The
                    // batch cooldown prevents a readable fd from busy-spinning.
                    return Ok(());
                }
                // Only clear a stale reactor notification after the kernel
                // confirms there is no input, preserving edge-triggered wakeups.
                guard.clear_ready();
            }
        }
        std::future::pending().await
    }
}
