use anyhow::{Context as _, Result, anyhow};
use futures::channel::oneshot;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value>>>>>;

pub struct Backend {
    input: Mutex<Option<ChildStdin>>,
    process: Mutex<Child>,
    pending: Pending,
    sequence: AtomicU64,
    #[cfg(target_os = "windows")]
    _job: ProcessJob,
}

pub struct Request {
    pub id: String,
    response: oneshot::Receiver<Result<Value>>,
}

impl Request {
    pub async fn result(self) -> Result<Value> {
        self.response
            .await
            .context("학습 백엔드와의 연결이 종료됐습니다.")?
    }
}

impl Backend {
    pub fn start(root: PathBuf, data: PathBuf) -> Result<Arc<Self>> {
        Self::start_worker(root, data, false)
    }

    pub fn start_sources(root: PathBuf, data: PathBuf) -> Result<Arc<Self>> {
        Self::start_worker(root, data, true)
    }

    fn start_worker(root: PathBuf, data: PathBuf, sources: bool) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&data)?;
        let diagnostics = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(data.join(if sources {
                "zed-sources.log"
            } else {
                "zed-worker.log"
            }))?;
        let venv_python = root.join(if cfg!(windows) {
            ".venv/Scripts/python.exe"
        } else {
            ".venv/bin/python"
        });
        let python = if cfg!(windows) {
            if let Some(path) = std::env::var_os("GDP_PYTHON") {
                PathBuf::from(path)
            } else {
                let configuration = std::fs::read_to_string(root.join(".venv/pyvenv.cfg"))?;
                let home = configuration
                    .lines()
                    .filter_map(|line| line.split_once('='))
                    .find(|(key, _)| key.trim() == "home")
                    .map(|(_, value)| value.trim())
                    .context("가상환경의 실제 Python 경로를 찾지 못했습니다.")?;
                PathBuf::from(home).join("python.exe")
            }
        } else {
            venv_python.clone()
        };
        let mut command = Command::new(python);
        command.args([
            "-u",
            "-m",
            "quizcore.cli",
            if sources { "source-serve" } else { "serve" },
        ]);
        command.arg("--startup-gate");
        if !sources {
            command.arg("--exclusive");
        }
        command
            .arg("--packs")
            .arg(root.join("puzzles"))
            .arg("--data")
            .arg(&data)
            .current_dir(&root)
            .env("PYTHONUTF8", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(diagnostics);
        #[cfg(target_os = "windows")]
        let job = ProcessJob::new()?;
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt as _;
            command.creation_flags(0x08000000);
            command.env("__PYVENV_LAUNCHER__", &venv_python);
        }
        let mut process = command
            .spawn()
            .context("학습 백엔드를 시작할 수 없습니다.")?;
        #[cfg(target_os = "windows")]
        match job.assign(&process) {
            Ok(()) => {}
            Err(error) => {
                process
                    .kill()
                    .context("준비되지 않은 백엔드를 종료하지 못했습니다.")?;
                process
                    .wait()
                    .context("백엔드 종료를 확인하지 못했습니다.")?;
                return Err(error);
            }
        };
        let mut input = process
            .stdin
            .take()
            .context("백엔드 입력을 열지 못했습니다.")?;
        let output = process
            .stdout
            .take()
            .context("백엔드 출력을 열지 못했습니다.")?;
        // Python cannot open learning data or spawn workers until it belongs to our job.
        input.write_all(b"GDP_START\n")?;
        input.flush()?;
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        std::thread::Builder::new()
            .name("daily-puzzle-responses".into())
            .spawn({
                let pending = pending.clone();
                move || {
                    for line in BufReader::new(output).lines() {
                        let response = match line
                            .context("백엔드 출력을 읽지 못했습니다.")
                            .and_then(|line| {
                                serde_json::from_str::<Value>(&line).map_err(Into::into)
                            }) {
                            Ok(response) => response,
                            Err(error) => {
                                log::error!("Daily Puzzle response: {error:#}");
                                break;
                            }
                        };
                        let Some(id) = response.get("id").and_then(Value::as_str) else {
                            log::error!("Daily Puzzle response has no request identifier");
                            continue;
                        };
                        if let Ok(mut requests) = pending.lock()
                            && let Some(sender) = requests.remove(id)
                        {
                            let result =
                                if response.get("ok").and_then(Value::as_bool) == Some(true) {
                                    Ok(response.get("data").cloned().unwrap_or(Value::Null))
                                } else {
                                    Err(anyhow!(
                                        response
                                            .get("error")
                                            .and_then(Value::as_str)
                                            .unwrap_or("학습 작업을 완료하지 못했습니다.")
                                            .to_owned()
                                    ))
                                };
                            if sender.send(result).is_err() {
                                log::debug!("Daily Puzzle request {id} was closed");
                            }
                        }
                    }
                    if let Ok(mut requests) = pending.lock() {
                        for (_, sender) in requests.drain() {
                            if sender
                                .send(Err(anyhow!(
                                    "학습 백엔드가 종료됐습니다. zed-worker.log를 확인해 주세요."
                                )))
                                .is_err()
                            {
                                log::debug!("Daily Puzzle request was closed before worker exit");
                            }
                        }
                    }
                }
            })?;
        Ok(Arc::new(Self {
            input: Mutex::new(Some(input)),
            process: Mutex::new(process),
            pending,
            sequence: AtomicU64::new(1),
            #[cfg(target_os = "windows")]
            _job: job,
        }))
    }

    pub fn request(&self, mut value: Value) -> Request {
        let id = self.sequence.fetch_add(1, Ordering::Relaxed).to_string();
        value["id"] = json!(id);
        let (sender, receiver) = oneshot::channel();
        match self.pending.lock() {
            Ok(mut requests) => {
                requests.insert(id.clone(), sender);
            }
            Err(_) => {
                if sender
                    .send(Err(anyhow!("백엔드 요청 상태가 손상됐습니다.")))
                    .is_err()
                {
                    log::error!("Daily Puzzle could not report a poisoned request queue");
                }
                return Request {
                    id,
                    response: receiver,
                };
            }
        }
        let written = (|| -> Result<()> {
            let mut owner = self
                .input
                .lock()
                .map_err(|_| anyhow!("백엔드 입력 잠금이 손상됐습니다."))?;
            let input = owner.as_mut().context("학습 백엔드가 종료 중입니다.")?;
            serde_json::to_writer(&mut *input, &value)?;
            input.write_all(b"\n")?;
            input.flush()?;
            Ok(())
        })();
        if let Err(error) = written
            && let Ok(mut requests) = self.pending.lock()
            && let Some(sender) = requests.remove(&id)
            && sender.send(Err(error)).is_err()
        {
            log::debug!("Daily Puzzle request was closed before a write failure");
        }
        Request {
            id,
            response: receiver,
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        if let Ok(input) = self.input.get_mut() {
            drop(input.take());
        }
        if let Ok(process) = self.process.get_mut() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                match process.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(20))
                    }
                    Ok(None) => break,
                    Err(error) => {
                        log::error!("Inspecting Daily Puzzle worker: {error}");
                        break;
                    }
                }
            }
            match process.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) => {
                    if let Err(error) = process.kill() {
                        log::error!("Stopping Daily Puzzle worker: {error}");
                    }
                    if let Err(error) = process.wait() {
                        log::error!("Waiting for Daily Puzzle worker: {error}");
                    }
                }
                Err(error) => log::error!("Inspecting Daily Puzzle worker: {error}"),
            }
        }
    }
}

#[cfg(target_os = "windows")]
struct ProcessJob {
    handle: usize,
}

#[cfg(target_os = "windows")]
impl ProcessJob {
    fn new() -> Result<Self> {
        use windows::Win32::{Foundation::CloseHandle, System::JobObjects::*};
        unsafe {
            let handle = CreateJobObjectW(None, None)?;
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let initialized = SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of_val(&limits) as u32,
            );
            if let Err(error) = initialized {
                CloseHandle(handle)?;
                return Err(error.into());
            }
            Ok(Self {
                handle: handle.0 as usize,
            })
        }
    }

    fn assign(&self, child: &Child) -> Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::{Foundation::HANDLE, System::JobObjects::AssignProcessToJobObject};
        unsafe {
            AssignProcessToJobObject(HANDLE(self.handle as *mut _), HANDLE(child.as_raw_handle()))?;
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
impl Drop for ProcessJob {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        if let Err(error) = unsafe { CloseHandle(HANDLE(self.handle as *mut _)) } {
            log::error!("Closing Daily Puzzle process job: {error}");
        }
    }
}
