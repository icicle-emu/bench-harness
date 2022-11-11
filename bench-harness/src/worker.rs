use std::{collections::HashMap, time::Duration};

use anyhow::Context;
use crossbeam_channel::{Receiver, Sender};

use crate::{
    firecracker::{self, VmConfig},
    tasks::Task,
};

pub struct WorkerPool {
    task_sender: Option<Sender<Task>>,
    task_receiver: Receiver<Task>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        if self.task_sender.is_some() {
            self.wait_for_workers();
        }
    }
}

impl WorkerPool {
    pub fn new() -> Self {
        let (task_sender, task_receiver) = crossbeam_channel::bounded(0);
        Self { task_sender: Some(task_sender), task_receiver, workers: vec![] }
    }

    /// Spawn a new worker and add it to pool.
    pub fn add_worker<F>(&mut self, mut worker: F) -> anyhow::Result<()>
    where
        F: FnMut(Task) -> anyhow::Result<()> + Send + Sync + 'static,
    {
        let id = self.workers.len();

        let span = tracing::info_span!("worker", id = %id);
        let _guard = span.enter();

        let rx = self.task_receiver.clone();
        let name = format!("[worker#{id:02}] task receiver");

        let parent = tracing::Span::current();
        let worker = std::thread::Builder::new().name(name).spawn(move || {
            let _guard = parent.enter();

            // Wait a short amount of time before handling any tasks to avoid contention during
            // worker start up (e.g. spawning the VM). This also (mostly) ensures that each worker
            // will receive the same initial task which is useful for debugging.
            std::thread::sleep(Duration::from_millis(10 * id as u64));

            tracing::debug!("Thread started");
            for task in rx {
                if let Err(e) = worker(task) {
                    tracing::error!("error running task: {:?}", e);
                }
            }
        })?;

        self.workers.push(worker);

        Ok(())
    }

    /// Queue a task on the pool, blocking if no worker is available.
    pub fn add_task(&self, task: Task) -> anyhow::Result<()> {
        if let Some(sender) = self.task_sender.as_ref() {
            crossbeam_channel::select! {
                send(sender, task) -> res => {
                    if res.is_err() {
                        anyhow::bail!("Failed to send task to worker");
                    }
                },
                recv(crate::cancellation_channel()) -> _ => anyhow::bail!("Cancellation requested"),
            }
        }
        Ok(())
    }

    /// Wait for all workers to finish execution.
    pub fn wait_for_workers(&mut self) {
        // Notify the workers that there is no jobs remaining by dropping the task sender.
        drop(self.task_sender.take());

        tracing::debug!("Waiting for {} workers to finish", self.workers.len());
        for worker in self.workers.drain(..) {
            if let Err(e) = worker.join() {
                tracing::error!("Worker crashed: {:?}", e);
            }
        }
    }
}

pub(crate) struct FirecrackerWorker {
    pub(crate) id: String,
    pub(crate) instances: std::sync::Arc<HashMap<String, VmConfig>>,
}

impl FirecrackerWorker {
    pub fn run_task(&mut self, mut task: Task) -> anyhow::Result<()> {
        tracing::info!("running {} on firecracker: id={}", task.name, self.id);

        let instance = &task.instance;
        let vm_config = self
            .instances
            .get(instance)
            .ok_or_else(|| anyhow::format_err!("Unknown instance {instance}"))?;

        let vm = firecracker::spawn_vm(self.id.clone(), &vm_config, false)?;
        let mut agent = firecracker::connect_to_vsock_agent(&vm)?;

        // @todo: consider adding different entropy for each worker? Most cases this should not
        // matter since there is other entropy available and we are not doing anything that needs to
        // be secure.
        if let Some(entropy) = vm_config.kernel_entropy.clone() {
            agent
                .send(agent_interface::Request::AddEntropy(entropy))
                .context("failed to add entropy to VM")?;
        }

        task.run(0, agent.as_mut())?;
        agent.shutdown_vm()?;

        if let Err(e) = vm.wait_for_exit_timeout(std::time::Duration::from_secs(10)) {
            tracing::error!("Error waiting for VM to exit: {:#}", e)
        }

        Ok(())
    }
}

#[derive(serde::Deserialize, Clone)]
pub(crate) struct LocalWorker {
    pub(crate) workdir: std::path::PathBuf,
    #[serde(skip)]
    pub(crate) id: usize,
}

impl LocalWorker {
    pub fn run_task(&mut self, mut task: Task) -> anyhow::Result<()> {
        if !self.workdir.exists() {
            anyhow::bail!("workdir: {} does not exist", self.workdir.display());
        }

        let (mut agent, handle) = agent::spawn_local_agent(Some(self.workdir.clone()))
            .context("failed to spawn local agent")?;

        task.run(self.id, agent.as_mut())?;
        agent.exit()?;

        let _ = handle.join();
        Ok(())
    }
}

pub(crate) struct DummyWorker {
    pub(crate) id: usize,
}

impl DummyWorker {
    pub fn run_task(&mut self, mut task: Task) -> anyhow::Result<()> {
        println!("running {} on worker {}", task.name, self.id);
        task.run(self.id, &mut DummyAgent::new())?;
        Ok(())
    }
}

struct DummyAgent {
    next_pid: u32,
}

impl DummyAgent {
    fn new() -> Self {
        Self { next_pid: 2 }
    }

    fn handle_request(
        &mut self,
        req: agent_interface::Request,
    ) -> anyhow::Result<agent_interface::Response> {
        use agent_interface::*;

        match req {
            Request::Reboot => eprintln!("reboot"),
            Request::RestartAgent => eprint!("restart agent"),
            Request::GetStats => eprintln!("get stats"),
            Request::SpawnProcess(process) => {
                let pid = self.next_pid;
                eprintln!("spawn({process}) = {pid}");
                self.next_pid += 1;
                return Ok(Response::Value(serde_json::json!(pid)));
            }
            Request::RunProcess(process) => {
                eprintln!("run({process})");
                return Ok(Response::Value(serde_json::to_value(&RunOutput {
                    exit: ExitKind::Success,
                    stdout: vec![],
                    stderr: vec![],
                })?));
            }
            Request::WaitPid(pid) => {
                eprintln!("wait({pid})");
                return Ok(Response::Value(serde_json::json!(0)));
            }
            Request::GetStatus(pid) => {
                eprintln!("status(pid={pid})");
                return Ok(agent_interface::Response::Value(serde_json::json!(null)));
            }
            Request::KillProcess { pid, signal } => {
                eprintln!("kill(pid={pid}, sig={signal})");
            }
            Request::ReadFile { path, offset, len } => {
                eprintln!("readat({}, {offset}, {})", path.display(), len.unwrap_or(0))
            }
            Request::StatFile(path) => {
                eprintln!("stat({})", path.display());
                return Ok(Response::Value(serde_json::json!(null)));
            }
            Request::ReadDir(path) => eprintln!("readdir({})", path.display()),
            Request::AddEntropy(bytes) => eprintln!("add_entropy({bytes:0x?})"),
            Request::Bulk(bulk) => {
                for req in bulk {
                    self.handle_request(req)?;
                }
            }
        }

        Ok(agent_interface::Response::Value(serde_json::json!({})))
    }
}

impl agent_interface::client::Agent for DummyAgent {
    fn send_request(
        &mut self,
        request: agent_interface::Request,
        _read_timeout: Option<std::time::Duration>,
    ) -> anyhow::Result<agent_interface::Response> {
        self.handle_request(request)
    }
}
