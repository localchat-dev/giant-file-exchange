use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use tokio::{runtime::Runtime, task::JoinHandle};

use crate::{
    api::{ExchangeApiClient, UploadOptions},
    logging::application_log,
    model::{UploadStatus, UploadTask},
};

enum QueueEvent {
    Progress {
        id: u64,
        generation: u64,
        sent: u64,
        total: u64,
    },
    Finished {
        id: u64,
        generation: u64,
        result: Result<Option<String>, String>,
    },
}

struct TaskOptions {
    generation: u64,
    options: UploadOptions,
}

pub struct UploadQueue {
    runtime: Runtime,
    api: ExchangeApiClient,
    tasks: Vec<UploadTask>,
    options: HashMap<u64, TaskOptions>,
    next_id: u64,
    current: Option<(u64, JoinHandle<()>)>,
    event_tx: mpsc::Sender<QueueEvent>,
    event_rx: mpsc::Receiver<QueueEvent>,
}

impl UploadQueue {
    pub fn new() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("file-exchange-upload")
            .build()
            .context("无法创建上传运行时")?;
        let api = ExchangeApiClient::new()?;
        let (event_tx, event_rx) = mpsc::channel();
        Ok(Self {
            runtime,
            api,
            tasks: Vec::new(),
            options: HashMap::new(),
            next_id: 1,
            current: None,
            event_tx,
            event_rx,
        })
    }

    pub fn tasks(&self) -> &[UploadTask] {
        &self.tasks
    }

    pub fn active_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|task| task.status.is_active())
            .count()
    }

    pub fn add_files(
        &mut self,
        paths: impl IntoIterator<Item = PathBuf>,
        temporary: bool,
        options: UploadOptions,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        for path in paths {
            let id = self.next_id;
            self.next_id += 1;
            match UploadTask::new(id, path.clone(), temporary) {
                Ok(mut task) => {
                    if let Some(file_name) = &options.upload_file_name {
                        task.file_name = file_name.clone();
                    }
                    self.tasks.push(task);
                    self.options.insert(
                        id,
                        TaskOptions {
                            generation: 0,
                            options: options.clone(),
                        },
                    );
                }
                Err(error) => errors.push(format!("{}：{error}", path.display())),
            }
        }
        self.start_next();
        errors
    }

    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.event_rx.try_recv() {
            changed |= self.apply_event(event);
        }
        if self
            .current
            .as_ref()
            .is_some_and(|(_, handle)| handle.is_finished())
        {
            if let Some((id, _)) = self.current.take()
                && let Some(task) = self.tasks.iter_mut().find(|task| task.id == id)
                && task.status.is_active()
            {
                task.status = UploadStatus::Failed;
                task.error = "上传工作线程意外结束".to_owned();
                application_log(
                    "ERROR",
                    &format!("上传工作线程意外结束：file={}", task.file_name),
                );
            }
            changed = true;
        }
        self.start_next();
        changed
    }

    pub fn cancel(&mut self, id: u64) {
        let Some(task) = self.tasks.iter_mut().find(|task| task.id == id) else {
            return;
        };
        if !task.status.is_active() {
            return;
        }
        if self
            .current
            .as_ref()
            .is_some_and(|(current_id, _)| *current_id == id)
            && let Some((_, handle)) = self.current.take()
        {
            handle.abort();
        }
        task.status = UploadStatus::Cancelled;
        task.speed_bytes_per_second = 0.0;
        if task.temporary {
            let _ = fs::remove_file(&task.path);
        }
        self.start_next();
    }

    pub fn retry(&mut self, id: u64) -> Result<()> {
        let task = self
            .tasks
            .iter_mut()
            .find(|task| task.id == id)
            .context("任务不存在")?;
        if !task.status.can_retry() {
            return Ok(());
        }
        let metadata = task.path.metadata().context("文件不存在或已被移动")?;
        if !metadata.is_file() {
            anyhow::bail!("所选路径不是普通文件");
        }
        task.file_size = metadata.len();
        task.reset_for_retry();
        if let Some(options) = self.options.get_mut(&id) {
            options.generation += 1;
        }
        self.start_next();
        Ok(())
    }

    pub fn remove(&mut self, id: u64) {
        let Some(index) = self.tasks.iter().position(|task| task.id == id) else {
            return;
        };
        if self.tasks[index].status.is_active() {
            return;
        }
        if self.tasks[index].temporary {
            let _ = fs::remove_file(&self.tasks[index].path);
        }
        self.tasks.remove(index);
        self.options.remove(&id);
    }

    pub fn cancel_all(&mut self) {
        if let Some((_, handle)) = self.current.take() {
            handle.abort();
        }
        for task in &mut self.tasks {
            if task.status.is_active() {
                task.status = UploadStatus::Cancelled;
            }
            if task.temporary {
                let _ = fs::remove_file(&task.path);
            }
        }
    }

    fn start_next(&mut self) {
        if self.current.is_some() {
            return;
        }
        let Some(task) = self
            .tasks
            .iter_mut()
            .find(|task| task.status == UploadStatus::Waiting)
        else {
            return;
        };
        let Some(task_options) = self.options.get(&task.id) else {
            return;
        };
        task.status = UploadStatus::Uploading;
        task.last_progress_at = Some(Instant::now());
        task.last_progress_bytes = 0;

        let id = task.id;
        let generation = task_options.generation;
        let path = task.path.clone();
        let options = task_options.options.clone();
        let api = self.api.clone();
        let tx = self.event_tx.clone();
        let progress_tx = tx.clone();
        let progress: Arc<dyn Fn(u64, u64) + Send + Sync> = Arc::new(move |sent, total| {
            let _ = progress_tx.send(QueueEvent::Progress {
                id,
                generation,
                sent,
                total,
            });
        });
        let handle = self.runtime.spawn(async move {
            let result = api
                .upload(&path, options, progress)
                .await
                .map(|result| result.file_id)
                .map_err(|error| error.to_string());
            let _ = tx.send(QueueEvent::Finished {
                id,
                generation,
                result,
            });
        });
        self.current = Some((id, handle));
    }

    fn apply_event(&mut self, event: QueueEvent) -> bool {
        match event {
            QueueEvent::Progress {
                id,
                generation,
                sent,
                total,
            } => {
                if self
                    .options
                    .get(&id)
                    .is_none_or(|options| options.generation != generation)
                {
                    return false;
                }
                let Some(task) = self.tasks.iter_mut().find(|task| task.id == id) else {
                    return false;
                };
                if task.status != UploadStatus::Uploading {
                    return false;
                }
                let now = Instant::now();
                let should_publish = sent >= total
                    || task
                        .last_progress_at
                        .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(100));
                if !should_publish {
                    return false;
                }
                if let Some(last) = task.last_progress_at {
                    let elapsed = now.duration_since(last).as_secs_f64();
                    if elapsed > 0.0 {
                        task.speed_bytes_per_second =
                            sent.saturating_sub(task.last_progress_bytes) as f64 / elapsed;
                    }
                }
                task.bytes_sent = sent.min(task.file_size);
                task.last_progress_bytes = sent;
                task.last_progress_at = Some(now);
                if sent >= total {
                    task.status = UploadStatus::Processing;
                    task.speed_bytes_per_second = 0.0;
                }
                true
            }
            QueueEvent::Finished {
                id,
                generation,
                result,
            } => {
                if self
                    .options
                    .get(&id)
                    .is_none_or(|options| options.generation != generation)
                {
                    return false;
                }
                let Some(task) = self.tasks.iter_mut().find(|task| task.id == id) else {
                    return false;
                };
                self.current.take_if(|(current_id, _)| *current_id == id);
                task.speed_bytes_per_second = 0.0;
                match result {
                    Ok(file_id) => {
                        task.bytes_sent = task.file_size;
                        task.status = UploadStatus::Succeeded;
                        task.server_file_id = file_id;
                        if task.temporary {
                            let _ = fs::remove_file(&task.path);
                        }
                    }
                    Err(error) => {
                        task.status = UploadStatus::Failed;
                        task.error = error;
                        application_log(
                            "ERROR",
                            &format!("上传失败：file={}; error={}", task.file_name, task.error),
                        );
                    }
                }
                true
            }
        }
    }
}

impl Drop for UploadQueue {
    fn drop(&mut self) {
        self.cancel_all();
    }
}
