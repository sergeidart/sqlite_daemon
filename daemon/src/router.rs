use crate::protocol::{Request, Response};
use crate::worker::{WorkerCommand, worker_loop};
use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::{error, info};

struct WorkerHandle {
    sender: mpsc::Sender<WorkerCommand>,
}

pub struct Router {
    workers: Arc<RwLock<HashMap<String, WorkerHandle>>>,
    base_path: PathBuf,
}

impl Router {
    pub fn new(base_path: PathBuf) -> Self {
        Self {
            workers: Arc::new(RwLock::new(HashMap::new())),
            base_path,
        }
    }

    pub async fn route_request(&self, req: Request) -> Response {
        let db_name = match Self::extract_db_name(&req) {
            Some(name) => name,
            None => {
                // Shutdown request doesn't need DB name
                if matches!(req, Request::Shutdown) {
                    return Response::ok_shutdown();
                }
                return Response::error("Missing database name in request");
            }
        };

        // Get or create worker for this database
        let worker = match self.get_or_create_worker(&db_name).await {
            Ok(w) => w,
            Err(e) => {
                error!(db = %db_name, error = %e, "Failed to get worker");
                return Response::error(format!("Failed to get worker: {}", e));
            }
        };

        // Send request to worker
        let (reply_tx, reply_rx) = oneshot::channel();
        let cmd = WorkerCommand::Request {
            req,
            reply: reply_tx,
        };

        if let Err(e) = worker.send(cmd).await {
            error!(db = %db_name, error = %e, "Failed to send to worker");
            // Worker might have died, remove it
            self.remove_worker(&db_name).await;
            return Response::error("Worker communication failed");
        }

        match reply_rx.await {
            Ok(response) => response,
            Err(_) => {
                error!(db = %db_name, "Worker reply channel closed");
                self.remove_worker(&db_name).await;
                Response::error("Worker communication failed")
            }
        }
    }

    async fn get_or_create_worker(&self, db_name: &str) -> Result<mpsc::Sender<WorkerCommand>> {
        // Fast path: check if worker exists
        {
            let workers = self.workers.read().await;
            if let Some(handle) = workers.get(db_name) {
                return Ok(handle.sender.clone());
            }
        }

        // Slow path: create new worker
        let mut workers = self.workers.write().await;
        
        // Double-check after acquiring write lock
        if let Some(handle) = workers.get(db_name) {
            return Ok(handle.sender.clone());
        }

        info!(db = %db_name, "Spawning new worker");

        let db_path = self.base_path.join(db_name);
        let (worker_tx, worker_rx) = mpsc::channel(1000);

        let db_name_clone = db_name.to_string();
        let workers_clone = Arc::clone(&self.workers);
        let db_path_clone = db_path.clone();
        
        tokio::spawn(async move {
            worker_loop(worker_rx, db_path_clone, db_name_clone.clone()).await;
            
            // Worker terminated, remove from map
            info!(db = %db_name_clone, "Worker terminated, removing from router");
            let mut workers = workers_clone.write().await;
            workers.remove(&db_name_clone);
        });

        let handle = WorkerHandle {
            sender: worker_tx.clone(),
        };

        workers.insert(db_name.to_string(), handle);

        Ok(worker_tx)
    }

    async fn remove_worker(&self, db_name: &str) {
        let mut workers = self.workers.write().await;
        if workers.remove(db_name).is_some() {
            info!(db = %db_name, "Worker removed from router");
        }
    }

    fn extract_db_name(req: &Request) -> Option<String> {
        match req {
            Request::Ping { db } => Some(db.clone()),
            Request::ExecBatch { db, .. } => Some(db.clone()),
            Request::PrepareForMaintenance { db } => Some(db.clone()),
            Request::CloseDatabase { db } => Some(db.clone()),
            Request::ReopenDatabase { db } => Some(db.clone()),
            Request::Shutdown => None,
        }
    }

    #[allow(dead_code)]
    pub async fn worker_count(&self) -> usize {
        self.workers.read().await.len()
    }
}
