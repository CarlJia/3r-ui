//! In-memory status and log tracking for ACME certificate requests.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use serde::Serialize;
use crate::crypto::session::now_unix;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub enum CertRequestStage {
    Idle,
    Starting,
    LocatingZone,
    PreparingAccount,
    SubmittingOrder,
    CreatingDnsRecord,
    WaitingPropagation,
    ValidatingChallenge,
    GeneratingCsr,
    FinalizingOrder,
    DownloadingCert,
    Success,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct CertLogEntry {
    pub timestamp: i64,
    pub level: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CertRequestStatus {
    pub is_running: bool,
    pub stage: CertRequestStage,
    pub stage_text: String,
    pub domain: String,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
    pub logs: Vec<CertLogEntry>,
}

pub struct CertProgressTracker {
    inner: Arc<Mutex<CertRequestStatus>>,
    running_flag: Arc<AtomicBool>,
}

impl Default for CertProgressTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl CertProgressTracker {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(CertRequestStatus {
                is_running: false,
                stage: CertRequestStage::Idle,
                stage_text: "空闲".to_string(),
                domain: String::new(),
                started_at: None,
                finished_at: None,
                error: None,
                logs: Vec::new(),
            })),
            running_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running_flag.load(Ordering::SeqCst)
    }

    pub async fn get_status(&self) -> CertRequestStatus {
        let guard = self.inner.lock().await;
        guard.clone()
    }

    pub async fn start(&self, domain: &str) -> bool {
        if self.running_flag.swap(true, Ordering::SeqCst) {
            return false; // Already running
        }
        let mut guard = self.inner.lock().await;
        guard.is_running = true;
        guard.stage = CertRequestStage::Starting;
        guard.stage_text = "准备开始".to_string();
        guard.domain = domain.to_string();
        guard.started_at = Some(now_unix());
        guard.finished_at = None;
        guard.error = None;
        guard.logs = vec![CertLogEntry {
            timestamp: now_unix(),
            level: "info".to_string(),
            message: format!("开始为域名 {domain} 申请 Let's Encrypt 证书"),
        }];
        true
    }

    pub async fn log(&self, stage: CertRequestStage, stage_text: &str, level: &str, msg: &str) {
        let mut guard = self.inner.lock().await;
        guard.stage = stage;
        guard.stage_text = stage_text.to_string();
        guard.logs.push(CertLogEntry {
            timestamp: now_unix(),
            level: level.to_string(),
            message: msg.to_string(),
        });
        if guard.logs.len() > 200 {
            guard.logs.remove(0);
        }
    }

    pub async fn success(&self, expires_at: i64) {
        let mut guard = self.inner.lock().await;
        guard.is_running = false;
        guard.stage = CertRequestStage::Success;
        guard.stage_text = "申请成功".to_string();
        guard.finished_at = Some(now_unix());
        let iso = crate::web::logs::timestamp_to_iso(expires_at);
        guard.logs.push(CertLogEntry {
            timestamp: now_unix(),
            level: "success".to_string(),
            message: format!("证书申请并安装成功，有效期至 {iso}"),
        });
        self.running_flag.store(false, Ordering::SeqCst);
    }

    pub async fn failure(&self, err: &str) {
        let mut guard = self.inner.lock().await;
        guard.is_running = false;
        guard.stage = CertRequestStage::Failed;
        guard.stage_text = "申请失败".to_string();
        guard.finished_at = Some(now_unix());
        guard.error = Some(err.to_string());
        guard.logs.push(CertLogEntry {
            timestamp: now_unix(),
            level: "error".to_string(),
            message: format!("错误: {err}"),
        });
        self.running_flag.store(false, Ordering::SeqCst);
    }
}
