//! Add support for running a command with a timeout, while capturing both stdout and stderr.

use std::{
    io,
    process::{Command, Stdio},
    time::Duration,
};

use crate::{ExitKind, RunOutput};

pub fn run_command(mut cmd: Command, timeout: Option<Duration>) -> io::Result<RunOutput> {
    let cmd = cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn()?;
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let (stdout, stderr, timeout) = imp::read2_or_timeout(out, err, timeout)?;

    let result = match timeout {
        true => {
            let _ = child.kill();
            child.wait()?;
            None
        }
        false => Some(child.wait()?),
    };

    let exit = match result {
        Some(e) if e.success() => ExitKind::Success,
        Some(e) => match e.code() {
            Some(code) => ExitKind::Exit(code),
            None => ExitKind::Crash,
        },
        None => {
            let _ = child.kill();
            let _ = child.try_wait();
            ExitKind::Hang
        }
    };

    Ok(RunOutput { exit, stdout, stderr })
}

/// Based on code from: https://github.com/rust-lang/cargo/blob/905af549966f23a9288e9993a85d1249a5436556/crates/cargo-util/src/read2.rs
#[cfg(unix)]
mod imp {
    use std::{
        convert::TryInto,
        io::{self, prelude::*},
        os::unix::prelude::*,
    };

    pub(crate) fn read2_or_timeout(
        mut out_pipe: std::process::ChildStdout,
        mut err_pipe: std::process::ChildStderr,
        timeout: Option<std::time::Duration>,
    ) -> io::Result<(Vec<u8>, Vec<u8>, bool)> {
        unsafe {
            libc::fcntl(out_pipe.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
            libc::fcntl(err_pipe.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
        }

        let mut out = Vec::new();
        let mut err = Vec::new();

        let mut fds: [libc::pollfd; 2] = [
            libc::pollfd { fd: out_pipe.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: err_pipe.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        let mut nfds = 2;

        let mut outfd = Some(0);
        let mut errfd = Some(1);

        let start = std::time::Instant::now();
        while nfds > 0 {
            let timeout: libc::c_int = match timeout {
                Some(x) => match x.checked_sub(start.elapsed()) {
                    Some(x) => x.as_millis().try_into().expect("timeout too large"),
                    None => return Ok((out, err, true)),
                },
                None => -1,
            };

            // wait for either pipe to become readable using `select`
            let r = unsafe { libc::poll(fds.as_mut_ptr(), nfds, timeout) };
            match r {
                0 => return Ok((out, err, true)),
                n if n < 0 => {
                    let err = io::Error::last_os_error();
                    if err.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(err);
                }
                _ => {}
            }

            // Read as much as we can from each pipe, ignoring EWOULDBLOCK or
            // EAGAIN. If we hit EOF, then this will happen because the underlying
            // reader will return Ok(0), in which case we'll see `Ok` ourselves.
            let handle = |res: io::Result<_>| match res {
                Ok(_) => Ok(true),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
                Err(e) => Err(e),
            };

            if let Some(fd) = errfd {
                if fds[fd].revents != 0 && handle(err_pipe.read_to_end(&mut err))? {
                    // Remove `err_pipe` from the active file descriptors we are polling.
                    errfd = None;
                    nfds -= 1;
                }
            }

            if let Some(fd) = outfd {
                if fds[fd].revents != 0 && handle(out_pipe.read_to_end(&mut out))? {
                    // Remove `out_pipe` from the active file descriptors we are polling.
                    outfd = None;
                    nfds -= 1;
                    // Move `err_pipe` to the first position in the `pollfd` list.
                    fds[0].fd = err_pipe.as_raw_fd();
                    errfd = errfd.map(|_| 0);
                }
            }
        }

        Ok((out, err, false))
    }
}

#[cfg(not(unix))]
mod imp {
    pub(crate) fn read2_or_timeout(
        _out_pipe: std::process::ChildStdout,
        _err_pipe: std::process::ChildStderr,
        _timeout: Option<std::time::Duration>,
    ) -> std::io::Result<(Vec<u8>, Vec<u8>, bool)> {
        unimplemented!()
    }
}
