use std::{collections::HashMap, path::PathBuf};

use anyhow::Context;
use parking_lot::lock_api::RawMutex;

use crate::{
    config::Config,
    firecracker::{BootSource, DriveConfig, VmConfig},
    tasks::Task,
};

mod afl;
mod config;
mod firecracker;
mod image_builder;
mod setup;
mod tasks;
mod utils;
mod worker;

enum WorkerBackend {
    Local,
    Firecracker,
    Dummy,
}

impl std::str::FromStr for WorkerBackend {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "local" => Ok(WorkerBackend::Local),
            "firecracker" => Ok(WorkerBackend::Firecracker),
            "dummy" => Ok(WorkerBackend::Dummy),
            _ => Err(anyhow::anyhow!("Invalid worker backend: {}", s)),
        }
    }
}
enum Command {
    Build,
    Debug(String),
    Bench { id: String, trials: usize, tasks: String },
    ExpandTask(String),
}

struct Args {
    config: PathBuf,
    workers: usize,
    backend: WorkerBackend,
    command: Command,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut pargs = pico_args::Arguments::from_env();

        Ok(Self {
            config: pargs
                .opt_value_from_str("--config")?
                .unwrap_or_else(|| PathBuf::from("config.toml")),
            workers: pargs.opt_value_from_str("--workers")?.unwrap_or(1),
            backend: pargs.opt_value_from_str("--backend")?.unwrap_or(WorkerBackend::Firecracker),
            command: {
                match pargs
                    .free_from_str::<String>()
                    .map_err(|_| anyhow::format_err!("missing COMMAND"))?
                    .as_str()
                {
                    "build" => Command::Build,
                    "debug" => Command::Debug(
                        pargs
                            .free_from_str()
                            .map_err(|_| anyhow::format_err!("expected instance name"))?,
                    ),
                    "bench" => {
                        let id = pargs
                            .opt_value_from_str("--id")?
                            .unwrap_or_else(|| "debug".to_string());
                        let trials = pargs.opt_value_from_str("--trials")?.unwrap_or(1);
                        let tasks = pargs
                            .free_from_str()
                            .map_err(|_| anyhow::format_err!("expected task list"))?;
                        Command::Bench { id, trials, tasks }
                    }
                    "expand-task" => {
                        let task = pargs
                            .free_from_str()
                            .map_err(|_| anyhow::format_err!("expected task name"))?;
                        Command::ExpandTask(task)
                    }

                    other => anyhow::bail!("Unknown command: {other}"),
                }
            },
        })
    }

    fn usage_str() -> &'static str {
        "bench-harness [--config=<path>] [--workers=<num>] [--backend=<backend>] COMMAND"
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_env_var("RUST_LOG")
                .with_default_directive(tracing::Level::INFO.into())
                .from_env_lossy(),
        )
        .with_target(false)
        .init();

    init_cancellation();

    let args = match Args::parse() {
        Ok(args) => args,
        Err(err) => {
            eprintln!("bench-harness: {err}");
            eprintln!("Usage: {}", Args::usage_str());
            std::process::exit(1);
        }
    };
    if let Err(e) = run(&args) {
        eprintln!("{:?}", e);
    }
}

fn run(args: &Args) -> anyhow::Result<()> {
    let mut config: Config = config::toml_from_path(&args.config)?;

    for entry in &config.include {
        let path = match args.config.parent() {
            Some(parent) => parent.join(entry),
            None => entry.clone(),
        };
        config
            .data
            .merge(config::toml_from_path(&path)?)
            .with_context(|| format!("error loading config from {}", path.display()))?;
    }

    for entry in &config.include_template {
        let path = match args.config.parent() {
            Some(parent) => parent.join(entry),
            None => entry.clone(),
        };

        config
            .data
            .merge(config::config_data_from_template(&path)?)
            .with_context(|| format!("error loading config from {}", path.display()))?;
    }

    std::fs::create_dir_all(&config.cache.dir).with_context(|| {
        format!("error creating cache directory {}", config.cache.dir.display())
    })?;

    match &args.command {
        Command::Build => build_images(&config),
        Command::Debug(instance_name) => {
            let instances = get_instance_config(&config)?;
            let instance = instances
                .get(instance_name)
                .ok_or_else(|| anyhow::format_err!("Unknown instance: {instance_name}"))?;
            spawn_debug_vm(instance)
        }
        Command::Bench { id, trials, tasks } => run_bench(args, config, id, *trials, tasks),
        Command::ExpandTask(task_name) => {
            match config.get_task(task_name) {
                Ok(task) => eprintln!("{task:#?}"),
                Err(e) => eprintln!("Error expanding {task_name}: {e}"),
            }
            Ok(())
        }
    }
}

fn run_bench(
    args: &Args,
    mut config: Config,
    id: &str,
    trials: usize,
    task_list: &str,
) -> anyhow::Result<()> {
    let mut worker_pool = worker::WorkerPool::new();
    match args.backend {
        WorkerBackend::Local => {
            let config = config
                .local_worker
                .as_ref()
                .ok_or_else(|| anyhow::format_err!("No local worker config"))?;
            for i in 0..args.workers {
                let mut worker = config.clone();
                worker.id = i;
                worker_pool.add_worker(move |task| worker.run_task(task))?;
            }
        }
        WorkerBackend::Firecracker => {
            let instances = std::sync::Arc::new(get_instance_config(&config)?);
            for i in 0..args.workers {
                let mut worker = worker::FirecrackerWorker {
                    id: format!("vm{i}-data"),
                    instances: instances.clone(),
                };
                worker_pool.add_worker(move |task| worker.run_task(task))?;
            }
        }
        WorkerBackend::Dummy => {
            for id in 0..args.workers {
                let mut worker = worker::DummyWorker { id };
                worker_pool.add_worker(move |task| worker.run_task(task))?;
            }
        }
    }
    tracing::info!("{} workers started", args.workers);

    config.vars.push(config::KeyValue::new("BENCH_ID", id));

    for task_name in task_list.split(&[',', '\n']).map(|x| x.trim()).filter(|x| !x.is_empty()) {
        let task = match config.get_task(task_name) {
            Ok(task) => task,
            Err(e) => {
                tracing::error!("Error running {task_name}: {e}");
                continue;
            }
        };

        for i in 0..trials {
            let mut task = task.clone();

            // Merge task specific variables with global variables. Note, the ordering matters here,
            // as we want to allow task local variables to reference globals.
            let mut vars = config.vars.clone();
            vars.push(config::KeyValue::new("TRIAL", format!("{i}")));
            vars.push(config::KeyValue::new("TASK_NAME", task_name));
            vars.extend(std::mem::take(&mut task.vars));

            worker_pool.add_task(Task {
                name: task_name.to_string(),
                instance: task.instance.clone(),
                vars,
                runable: Box::new(tasks::DynamicTask::TaskList { tasks: task.tasks.clone() }),
            })?;
        }
    }
    tracing::info!("All pending tasks started");

    worker_pool.wait_for_workers();
    tracing::info!("All tasks complete");

    Ok(())
}

fn get_instance_config(config: &Config) -> anyhow::Result<HashMap<String, VmConfig>> {
    let firecracker_config = config
        .firecracker
        .as_ref()
        .ok_or_else(|| anyhow::format_err!("[firecracker] config missing"))?;
    let firecracker = setup::get_firecracker_path(firecracker_config, &config.cache)?;
    tracing::debug!("firecracker: {}", firecracker.display());

    let kernel_config =
        config.kernel.as_ref().ok_or_else(|| anyhow::format_err!("[kernel] config missing"))?;
    let kernel = setup::get_kernel_path(kernel_config, &config.cache)?;
    tracing::debug!("kernel: {}", kernel.display());

    let image_paths: HashMap<_, _> = config
        .data
        .images
        .iter()
        .map(|(name, _)| Ok((name, image_builder::get_image_path(name, &config.cache)?)))
        .collect::<anyhow::Result<_>>()?;

    let mut instances = HashMap::new();
    for (name, instance) in &config.data.instances {
        let vm_config =
            build_instance(&firecracker, instance, kernel_config, &kernel, &image_paths)
                .with_context(|| format!("failed to build: {name}"))?;
        instances.insert(name.clone(), vm_config);
    }

    Ok(instances)
}

fn build_instance(
    firecracker: &PathBuf,
    instance: &config::Instance,
    kernel_config: &config::Kernel,
    kernel: &PathBuf,
    image_paths: &HashMap<&String, PathBuf>,
) -> anyhow::Result<VmConfig> {
    Ok(VmConfig {
        bin: firecracker.clone(),
        boot_delay_sec: instance.boot_delay_sec,
        recreate_work_dir: instance.recreate_workdir,
        kernel_entropy: kernel_config.entropy.clone(),
        boot: BootSource {
            kernel_image_path: kernel.clone(),
            boot_args: kernel_config.boot_args.clone(),
        },
        machine: instance.machine.clone(),
        rootfs: DriveConfig {
            name: instance.rootfs.name.clone(),
            path: image_paths
                .get(&instance.rootfs.image)
                .ok_or_else(|| {
                    anyhow::format_err!("failed to find rootfs image: {}", instance.rootfs.image)
                })?
                .clone(),
            mount: instance.rootfs.mount_as,
        },
        drives: instance
            .drives
            .iter()
            .map(|drive| {
                Ok(DriveConfig {
                    name: drive.name.clone(),
                    path: image_paths
                        .get(&drive.image)
                        .ok_or_else(|| {
                            anyhow::format_err!("failed to find drive: {}", drive.image)
                        })?
                        .clone(),
                    mount: drive.mount_as,
                })
            })
            .collect::<anyhow::Result<Vec<DriveConfig>>>()?,
    })
}

fn spawn_debug_vm(config: &VmConfig) -> anyhow::Result<()> {
    let vm = firecracker::spawn_vm("vm-debug-data".into(), config, true)?;

    let mut agent = firecracker::connect_to_vsock_agent(&vm)?;
    if let Some(entropy) = config.kernel_entropy.clone() {
        agent.send(agent_interface::Request::AddEntropy(entropy))?;
    }

    let pid = agent.spawn_task(
        agent_interface::RunCommand::from_cmd_string("/bin/bash -i")
            .unwrap()
            .stdin(agent_interface::Stdio::Inherit)
            .stdout(agent_interface::Stdio::Inherit)
            .stderr(agent_interface::Stdio::Inherit),
    )?;
    tracing::debug!("`/bin/bash` pid={pid}");

    vm.wait_for_exit()?;
    Ok(())
}

/// Builds all images used for VMs. This is not done as part of normal execution because it
/// currently requires root permissions (in order to mount disks).
fn build_images(config: &Config) -> anyhow::Result<()> {
    for (name, source) in &config.data.images {
        image_builder::build_image(&name, &source, &config.cache)
            .with_context(|| format!("failed to build: {name}"))?;
    }
    Ok(())
}

pub trait XShellExt {
    /// Runs a command, returning stdout on success, and including stderr in the error message
    fn read_with_err(self) -> anyhow::Result<String>;

    /// Echos command to tracing
    fn trace_cmd(self) -> Self;
}

impl<'a> XShellExt for xshell::Cmd<'a> {
    fn read_with_err(self) -> anyhow::Result<String> {
        let cmd = format!("{}", self);
        let output = self.trace_cmd().ignore_status().output()?;
        match output.status.success() {
            true => Ok(String::from_utf8(output.stdout)?),
            false => {
                anyhow::bail!("`{}` failed with {}", cmd, String::from_utf8_lossy(&output.stderr))
            }
        }
    }

    fn trace_cmd(mut self) -> Self {
        tracing::info!("$ {}", self);
        self.set_quiet(false);
        self
    }
}

/// Mutex for syncronizing host file system operations in workers.
pub static HOST_FS_LOCK: parking_lot::Mutex<()> =
    parking_lot::Mutex::const_new(parking_lot::RawMutex::INIT, ());

/// Global stop flag used for supporting clean exits.
static STOP_NOW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Channel used for listening for cancellation events.
static CANCELATION_RECEIVER: once_cell::sync::OnceCell<crossbeam_channel::Receiver<()>> =
    once_cell::sync::OnceCell::new();

fn init_cancellation() {
    let (cancel_tx, cancel_rx) = crossbeam_channel::bounded(0);
    CANCELATION_RECEIVER.set(cancel_rx).unwrap();
    let mut cancel_tx = Some(cancel_tx);
    ctrlc::set_handler(move || {
        STOP_NOW.store(true, std::sync::atomic::Ordering::Release);
        cancel_tx.take();
    })
    .unwrap();
}

pub(crate) fn should_stop() -> bool {
    STOP_NOW.load(std::sync::atomic::Ordering::Acquire)
}

pub(crate) fn cancellation_channel() -> &'static crossbeam_channel::Receiver<()> {
    CANCELATION_RECEIVER.get().unwrap()
}
