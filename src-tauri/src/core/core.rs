use crate::config::*;
use crate::core::{clash_api, handle, logger::Logger};
use crate::log_err;
use crate::utils::dirs;
use anyhow::{bail, Result};
use once_cell::sync::OnceCell;
use serde_yaml::Mapping;
use std::{sync::Arc, time::Duration};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;
use tokio::sync::Mutex;

use tokio::time::sleep;

#[derive(Debug)]
pub struct CoreManager {
    sidecar: Arc<Mutex<Option<CommandChild>>>,
    running: Arc<Mutex<bool>>,
}

impl CoreManager {
    pub fn global() -> &'static CoreManager {
        static CORE_MANAGER: OnceCell<CoreManager> = OnceCell::new();

        CORE_MANAGER.get_or_init(|| CoreManager {
            sidecar: Arc::new(Mutex::new(None)),
            running: Arc::new(Mutex::new(false)),
        })
    }

    pub async fn init(&self) -> Result<()> {
        log::trace!("run core start");
        // 启动clash
        log_err!(Self::global().start_core().await);
        log::trace!("run core end");
        Ok(())
    }

    /// 检查订阅是否正确
    pub async fn check_config(&self) -> Result<()> {
        let config_path = Config::generate_file(ConfigType::Check)?;
        let config_path = dirs::path_to_str(&config_path)?;

        let clash_core = { Config::verge().latest().clash_core.clone() };
        let clash_core = clash_core.unwrap_or("verge-mihomo".into());

        let test_dir = dirs::app_home_dir()?.join("test");
        let test_dir = dirs::path_to_str(&test_dir)?;
        let app_handle = handle::Handle::global().app_handle().unwrap();

        let output = app_handle
            .shell()
            .sidecar(clash_core)?
            .args(["-t", "-d", test_dir, "-f", config_path])
            .output()
            .await?;

        if !output.status.success() {
            let stdout = String::from_utf8(output.stdout).unwrap_or_default();
            let error = clash_api::parse_check_output(stdout.clone());
            let error = match !error.is_empty() {
                true => error,
                false => stdout.clone(),
            };
            Logger::global().set_log(stdout.clone());
            bail!("{error}");
        }

        Ok(())
    }

    /// 停止核心运行
    pub async fn stop_core(&self) -> Result<()> {
        let mut running = self.running.lock().await;

        if !*running {
            log::debug!("core is not running");
            return Ok(());
        }

        // 关闭tun模式
        let mut disable = Mapping::new();
        let mut tun = Mapping::new();
        tun.insert("enable".into(), false.into());
        disable.insert("tun".into(), tun.into());
        log::debug!(target: "app", "disable tun mode");
        log_err!(clash_api::patch_configs(&disable).await);

        if let Some(sidecar) = self.sidecar.lock().await.take() {
            let _ = sidecar.kill();
        }
        *running = false;

        Ok(())
    }

    /// 启动核心
    pub async fn start_core(&self) -> Result<()> {
        let mut running = self.running.lock().await;
        if *running {
            log::debug!("core is running");
            return Ok(());
        }

        let config_path = Config::generate_file(ConfigType::Run)?;
        let clash_core = { Config::verge().latest().clash_core.clone() };
        let clash_core = clash_core.unwrap_or("verge-mihomo".into());

        let app_dir = dirs::app_home_dir()?;
        let app_dir = dirs::path_to_str(&app_dir)?;
        let config_path = dirs::path_to_str(&config_path)?;
        let args = vec!["-d", app_dir, "-f", config_path];
        let app_handle = handle::Handle::global().app_handle().unwrap();
        let cmd = app_handle.shell().sidecar(clash_core)?;
        let (mut rx, child) = cmd.args(args).spawn()?;

        tauri::async_runtime::spawn(async move {
            while let Some(event) = rx.recv().await {
                match event {
                    CommandEvent::Stdout(line) => {
                        let line = String::from_utf8(line).unwrap_or_default();
                        log::info!(target: "app", "[mihomo]: {line}");
                        Logger::global().set_log(line);
                    }
                    CommandEvent::Stderr(err) => {
                        let err = String::from_utf8(err).unwrap_or_default();
                        log::error!(target: "app", "[mihomo]: {err}");
                        Logger::global().set_log(err);
                    }
                    CommandEvent::Error(err) => {
                        log::error!(target: "app", "[mihomo]: {err}");
                        Logger::global().set_log(err);
                    }
                    CommandEvent::Terminated(_) => {
                        log::info!(target: "app", "mihomo core terminated");
                        break;
                    }
                    _ => {}
                }
            }
        });
        let mut sidecar = self.sidecar.lock().await;
        *sidecar = Some(child);

        *running = true;
        Ok(())
    }

    /// 重启内核
    pub async fn restart_core(&self) -> Result<()> {
        // 重新启动app
        self.stop_core().await?;
        self.start_core().await?;
        Ok(())
    }

    /// 切换核心
    pub async fn change_core(&self, clash_core: Option<String>) -> Result<()> {
        let clash_core = clash_core.ok_or(anyhow::anyhow!("clash core is null"))?;
        const CLASH_CORES: [&str; 2] = ["verge-mihomo", "verge-mihomo-alpha"];

        if !CLASH_CORES.contains(&clash_core.as_str()) {
            bail!("invalid clash core name \"{clash_core}\"");
        }

        log::debug!(target: "app", "change core to `{clash_core}`");

        Config::verge().draft().clash_core = Some(clash_core);

        // 更新订阅
        Config::generate().await?;

        self.check_config().await?;

        // 清掉旧日志
        Logger::global().clear_log();

        match self.restart_core().await {
            Ok(_) => {
                Config::verge().apply();
                Config::runtime().apply();
                log_err!(Config::verge().latest().save_file());
                Ok(())
            }
            Err(err) => {
                Config::verge().discard();
                Config::runtime().discard();
                Err(err)
            }
        }
    }

    /// 更新proxies那些
    /// 如果涉及端口和外部控制则需要重启
    pub async fn update_config(&self) -> Result<()> {
        log::debug!(target: "app", "try to update clash config");
        // 更新订阅
        Config::generate().await?;

        // 检查订阅是否正常
        self.check_config().await?;

        // 更新运行时订阅
        let path = Config::generate_file(ConfigType::Run)?;
        let path = dirs::path_to_str(&path)?;

        // 发送请求 发送5次
        for i in 0..10 {
            match clash_api::put_configs(path).await {
                Ok(_) => break,
                Err(err) => {
                    if i < 9 {
                        log::info!(target: "app", "{err}");
                    } else {
                        bail!(err);
                    }
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
        Ok(())
    }
}
