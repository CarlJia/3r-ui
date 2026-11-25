//! Proxy supervisor managing the lifecycle of the Pingora reverse proxy service.

use async_trait::async_trait;
use pingora_core::server::{RunArgs, Server, ShutdownSignal, ShutdownSignalWatch};
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use tokio::sync::{mpsc, watch};

use crate::error::{AppError, AppResult};
use crate::proxy::cert::{cert_path, key_path};
use crate::proxy::config::{get_proxy_settings, update_cert_status};
use crate::proxy::server::PanelProxy;

#[derive(Clone, Debug)]
pub enum ProxyCommand {
    /// 重新加载或应用配置
    Reload,
    /// 停止反向代理服务并退出 supervisor
    Stop,
}

pub struct WatchShutdownSignal(pub watch::Receiver<bool>);

#[async_trait]
impl ShutdownSignalWatch for WatchShutdownSignal {
    async fn recv(&self) -> ShutdownSignal {
        let mut rx = self.0.clone();
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                break;
            }
        }
        ShutdownSignal::FastShutdown
    }
}

struct RunningInstance {
    shutdown_tx: watch::Sender<bool>,
    thread_handle: Option<JoinHandle<()>>,
}

impl RunningInstance {
    fn shutdown(mut self) {
        let _ = self.shutdown_tx.send(true);
        if let Some(h) = self.thread_handle.take() {
            let _ = h.join();
        }
    }
}

pub struct ProxySupervisor {
    pool: SqlitePool,
    data_dir: PathBuf,
    upstream_addr: String,
}

impl ProxySupervisor {
    pub fn new(pool: SqlitePool, data_dir: PathBuf, upstream_addr: String) -> Self {
        Self {
            pool,
            data_dir,
            upstream_addr,
        }
    }

    /// 启动 supervisor 循环，监听来自面板的重载与停机命令。
    pub async fn run(self, mut rx: mpsc::Receiver<ProxyCommand>) -> AppResult<()> {
        let supervisor = Arc::new(self);
        let mut current_instance: Option<RunningInstance> = None;

        // 启动时检查一次是否需要拉起
        match supervisor.start_instance().await {
            Ok(inst) => current_instance = inst,
            Err(e) => {
                tracing::warn!(error = %e, "proxy supervisor failed to start initial instance");
                let _ =
                    update_cert_status(&supervisor.pool, None, None, Some(&e.to_string())).await;
            }
        }

        while let Some(cmd) = rx.recv().await {
            match cmd {
                ProxyCommand::Reload => {
                    tracing::info!("reloading Pingora reverse proxy service");
                    if let Some(inst) = current_instance.take() {
                        tokio::task::spawn_blocking(move || inst.shutdown())
                            .await
                            .ok();
                    }
                    match supervisor.start_instance().await {
                        Ok(inst) => current_instance = inst,
                        Err(e) => {
                            tracing::error!(error = %e, "proxy supervisor failed to reload instance");
                            let _ = update_cert_status(
                                &supervisor.pool,
                                None,
                                None,
                                Some(&e.to_string()),
                            )
                            .await;
                        }
                    }
                }
                ProxyCommand::Stop => {
                    tracing::info!("stopping Pingora reverse proxy service");
                    if let Some(inst) = current_instance.take() {
                        tokio::task::spawn_blocking(move || inst.shutdown())
                            .await
                            .ok();
                    }
                    break;
                }
            }
        }

        Ok(())
    }

    /// 根据当前数据库配置拉起 Pingora 服务
    async fn start_instance(&self) -> AppResult<Option<RunningInstance>> {
        let settings = get_proxy_settings(&self.pool).await?;
        if !settings.is_enabled() {
            tracing::info!("Pingora reverse proxy is disabled in settings");
            return Ok(None);
        }

        // 确定证书路径
        let (cert_file, key_file) = if settings.cert_mode == "custom" {
            (
                PathBuf::from(&settings.custom_cert_path),
                PathBuf::from(&settings.custom_key_path),
            )
        } else {
            (
                cert_path(&self.data_dir, &settings.domain),
                key_path(&self.data_dir, &settings.domain),
            )
        };

        if !cert_file.is_file() || !key_file.is_file() {
            if settings.cert_mode == "custom" {
                let err = format!(
                    "自定义 TLS 证书或私钥文件不存在：{} 和 {}",
                    cert_file.display(),
                    key_file.display()
                );
                tracing::warn!(%err, "cannot start Pingora without valid custom certificate");
                let _ = update_cert_status(&self.pool, None, None, Some(&err)).await;
            } else {
                tracing::info!(
                    domain = %settings.domain,
                    "Pingora 反向代理已启用，等待 TLS 证书签发完成"
                );
                if settings.last_error.is_none() {
                    let _ = update_cert_status(
                        &self.pool,
                        None,
                        None,
                        Some("证书尚未签发或安装，请点击下方「立即申请 / 强制续期证书」"),
                    )
                    .await;
                }
            }
            return Ok(None);
        }

        let cert_str = cert_file.to_str().unwrap_or_default().to_string();
        let key_str = key_file.to_str().unwrap_or_default().to_string();
        let upstream = self.upstream_addr.clone();
        let domain = settings.domain.clone();
        let path = settings.normalized_path();
        let sub_path = settings.normalized_sub_path();
        let listen_port = settings.listen_port;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let watch_signal = WatchShutdownSignal(shutdown_rx);

        let thread_handle = std::thread::Builder::new()
            .name("pingora-service".into())
            .spawn(move || {
                let mut server = match Server::new(None) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to initialize Pingora Server");
                        return;
                    }
                };
                server.bootstrap();

                let proxy_service = PanelProxy::new(upstream, domain, path, sub_path);
                let mut http_service = pingora_proxy::http_proxy_service(
                    &server.configuration,
                    proxy_service,
                );

                let listen_addr = format!("0.0.0.0:{}", listen_port);
                if let Err(e) = http_service.add_tls(&listen_addr, &cert_str, &key_str) {
                    tracing::error!(error = %e, %listen_addr, "failed to add TLS to Pingora service");
                    return;
                }

                tracing::info!(addr = %listen_addr, "Pingora reverse proxy listening");
                server.add_service(http_service);

                server.run(RunArgs {
                    shutdown_signal: Box::new(watch_signal),
                });
                tracing::info!("Pingora reverse proxy service exited cleanly");
            })
            .map_err(AppError::Io)?;

        // 成功启动，清除上次错误记录
        let _ = update_cert_status(&self.pool, None, None, None).await;

        Ok(Some(RunningInstance {
            shutdown_tx,
            thread_handle: Some(thread_handle),
        }))
    }
}
