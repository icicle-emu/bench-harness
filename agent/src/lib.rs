pub mod log_collector;

use std::{
    collections::HashMap,
    io::{Read, Seek},
    path::PathBuf,
    process,
    sync::{Arc, Mutex},
};

use agent_interface::{client::Agent, Request, Response};
use anyhow::Context;

use crate::log_collector::StatsdData;

struct LocalAgent {
    sender: crossbeam_channel::Sender<Request>,
    receiver: crossbeam_channel::Receiver<Response>,
}

impl Agent for LocalAgent {
    fn send_request(
        &mut self,
        request: Request,
        read_timeout: Option<std::time::Duration>,
    ) -> anyhow::Result<Response> {
        self.sender.send(request).context("failed to send request")?;

        let timeout =
            read_timeout.map(|d| crossbeam_channel::after(d)).unwrap_or(crossbeam_channel::never());
        crossbeam_channel::select! {
            recv(self.receiver) -> response => {
                response.map_err(|e| anyhow::format_err!("failed to receive response: {e}"))
            }
            recv(timeout) -> _ => anyhow::bail!("failed to receive response: timeout"),
        }
    }
}

pub fn spawn_local_agent(
    workdir: Option<PathBuf>,
) -> anyhow::Result<(Box<dyn Agent>, std::thread::JoinHandle<()>)> {
    let mut state = AgentState::new(Arc::new(Mutex::new(log_collector::StatsdData::new(0))));
    state.workdir = workdir;

    let (req_tx, req_rx) = crossbeam_channel::bounded(0);
    let (res_tx, res_rx) = crossbeam_channel::bounded(0);

    let handle = std::thread::spawn(move || {
        for req in req_rx {
            state.reap_dead();

            let res = map_response(state.handle_request(req));
            if res_tx.send(res).is_err() {
                break;
            }

            if state.exit.is_some() {
                if let Err(e) = state.kill_all() {
                    eprintln!("failed to kill all processes: {e}");
                }
                break;
            }
        }
    });

    Ok((Box::new(LocalAgent { sender: req_tx, receiver: res_rx }), handle))
}

pub enum Exit {
    RestartAgent,
    Shutdown,
}

pub fn map_response(result: anyhow::Result<serde_json::Value>) -> Response {
    match result {
        Ok(value) => Response::Value(value),
        Err(err) => Response::Error { error: err.to_string() },
    }
}

pub struct AgentState {
    pub exit: Option<Exit>,
    workdir: Option<PathBuf>,
    stats: Arc<Mutex<StatsdData>>,
    buf: Vec<u8>,
    subprocesses: HashMap<u32, process::Child>,
}

impl AgentState {
    pub fn new(stats: Arc<Mutex<StatsdData>>) -> Self {
        Self { stats, buf: vec![], exit: None, subprocesses: HashMap::new(), workdir: None }
    }

    pub fn handle_request(&mut self, request: Request) -> anyhow::Result<serde_json::Value> {
        match request {
            Request::Reboot => {
                self.exit = Some(Exit::Shutdown);
            }
            Request::RestartAgent => {
                self.exit = Some(Exit::RestartAgent);
            }
            Request::GetStats => {
                self.buf.clear();
                for entry in self.stats.lock().unwrap().drain_all() {
                    self.buf.extend_from_slice(entry);
                }

                let entries = std::str::from_utf8(&self.buf)?;
                return Ok(serde_json::json!(entries));
            }
            Request::RunProcess(mut subprocess) => {
                if subprocess.current_dir.is_none() {
                    subprocess.current_dir = self.workdir.clone();
                }
                eprintln!("[agent] running: {}", subprocess);
                let output = subprocess.run()?;
                return Ok(serde_json::json!(output));
            }
            Request::SpawnProcess(mut subprocess) => {
                if subprocess.current_dir.is_none() {
                    subprocess.current_dir = self.workdir.clone();
                }
                eprintln!("[agent] spawning: {}", subprocess);
                let child = subprocess.spawn()?;
                let pid = child.id();
                eprintln!("[agent] spawned PID={}", pid);
                self.subprocesses.insert(pid, child);
                return Ok(serde_json::json!(pid));
            }
            Request::WaitPid(pid) => {
                return match self.subprocesses.get_mut(&pid) {
                    Some(p) => {
                        let exit = p.wait()?;
                        let _ = self.subprocesses.remove(&pid);
                        Ok(serde_json::json!(exit.code()))
                    }
                    None => Ok(serde_json::json!(null)),
                };
            }
            Request::GetStatus(id) => {
                return match self.subprocesses.get(&id) {
                    Some(c) => Ok(serde_json::json!(c.id())),
                    None => Ok(serde_json::json!(null)),
                };
            }
            Request::KillProcess { pid, signal } => {
                let result = self.kill_subprocess(pid, signal)?;
                return Ok(serde_json::json!(result));
            }
            Request::ReadFile { path, offset, len } => {
                let path = match self.workdir.as_ref() {
                    Some(workdir) => workdir.join(path),
                    None => path,
                };
                let mut file = std::fs::File::open(&path)?;

                let remaining_len = file.metadata()?.len().saturating_sub(offset);
                let len = match len {
                    Some(len) => len.min(remaining_len),
                    None => remaining_len,
                };

                file.seek(std::io::SeekFrom::Start(offset))?;
                let mut buf = vec![0; len as usize];
                file.read_exact(&mut buf)?;

                return Ok(serde_json::json!(buf));
            }
            Request::StatFile(path) => {
                let path = match self.workdir.as_ref() {
                    Some(workdir) => workdir.join(path),
                    None => path,
                };
                let metadata = std::fs::metadata(&path)?;
                return Ok(serde_json::json!(agent_interface::DirEntry {
                    path: path.canonicalize()?,
                    is_file: metadata.is_file(),
                    len: metadata.len(),
                    modified: metadata.modified().unwrap_or_else(|_| std::time::SystemTime::now()),
                }));
            }
            Request::ReadDir(path) => {
                let path = match self.workdir.as_ref() {
                    Some(workdir) => workdir.join(path),
                    None => path,
                };
                let entries = agent_interface::utils::read_dir_entries(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?;
                return Ok(serde_json::json!(entries));
            }
            Request::AddEntropy(data) => {
                add_entropy(&data)?;
            }
            Request::Bulk(requests) => {
                let out: Vec<_> = requests
                    .into_iter()
                    .map(|req| map_response(self.handle_request(req)))
                    .collect();
                return Ok(serde_json::json!(out));
            }
        }
        Ok(serde_json::json!(null))
    }

    pub fn reap_dead(&mut self) {
        let mut dead = vec![];
        for (name, process) in &mut self.subprocesses {
            if let Ok(Some(exit)) = process.try_wait() {
                eprintln!("[agent] pid={} exit: {:?}", process.id(), exit);
                dead.push(name.clone());
            }
        }
        dead.into_iter().for_each(|dead| {
            self.subprocesses.remove(&dead);
        });
    }

    fn kill_subprocess(&mut self, key: u32, signal: i32) -> Result<bool, anyhow::Error> {
        if let Some(process) = self.subprocesses.get_mut(&key) {
            #[cfg(unix)]
            {
                let signal = nix::sys::signal::Signal::try_from(signal)?;
                nix::sys::signal::kill(nix::unistd::Pid::from_raw(process.id() as i32), signal)?;
            }

            #[cfg(not(unix))]
            {
                let _signal = signal;
                process.kill()?;
            }

            let exit = process.wait()?;
            eprintln!("[agent] pid={} exit: {:?}", key, exit);

            // Managed to actually kill the subprocess so drop the handle.
            let _ = self.subprocesses.remove(&key);
            Ok(true)
        }
        else {
            Ok(false)
        }
    }

    pub fn kill_all(&mut self) -> Result<(), anyhow::Error> {
        for (_, process) in &mut self.subprocesses {
            let _ = process.kill();
        }
        for (pid, mut process) in self.subprocesses.drain() {
            let exit = process.wait()?;
            eprintln!("[agent] pid={} exit: {:?}", pid, exit);
        }
        Ok(())
    }
}

impl Drop for AgentState {
    fn drop(&mut self) {
        let _ = self.kill_all();
    }
}

#[cfg(not(unix))]
fn add_entropy(_bytes: &[u32]) -> anyhow::Result<()> {
    anyhow::bail!("Unable add entropy on target platform")
}

#[cfg(unix)]
fn add_entropy(data: &[u32]) -> anyhow::Result<()> {
    let fd = unsafe { nix::libc::open("/dev/urandom\0".as_ptr().cast(), nix::libc::O_RDWR) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("failed to open `/dev/urandom`");
    }

    // Assume every bit in `data` contains full entropy.
    let entropy_count = data.len() * std::mem::size_of::<u32>() * 8;
    let result = unsafe { add_entropy_ioctl(fd, entropy_count.try_into().unwrap(), data) };

    if unsafe { nix::libc::close(fd) } < 0 {
        return Err(std::io::Error::last_os_error()).context("failed to close `/dev/urandom`");
    }

    result.context("error executing `RNDADDENTROPY` ioctl")?;
    Ok(())
}

#[cfg(unix)]
unsafe fn add_entropy_ioctl(
    fd: i32,
    entropy_count: nix::libc::c_int,
    data: &[u32],
) -> std::io::Result<i32> {
    use std::alloc::Layout;

    nix::ioctl_write_ptr!(rnd_add_entropy, b'R', 0x03, linux_raw_sys::general::rand_pool_info);

    let layout = Layout::new::<nix::libc::c_int>(); // entropy_count
    let (layout, _) = layout.extend(Layout::new::<nix::libc::c_int>()).unwrap(); // buf_size;
    let (layout, _) = layout.extend(Layout::array::<u32>(data.len()).unwrap()).unwrap(); // buf;

    let alloc = std::alloc::alloc_zeroed(layout);
    if alloc == std::ptr::null_mut() {
        return Err(std::io::Error::last_os_error());
    }

    let info: *mut linux_raw_sys::general::rand_pool_info = alloc.cast();
    (*info).entropy_count = entropy_count;
    (*info).buf_size = (data.len() * std::mem::size_of::<u32>()).try_into().unwrap();
    (*info).buf.as_mut_slice(data.len()).copy_from_slice(data);

    let result = rnd_add_entropy(fd, info);
    std::alloc::dealloc(info.cast(), layout);

    result.map_err(|x| std::io::Error::from_raw_os_error(x as i32))
}
