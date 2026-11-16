//! 内核下载与切换入口；所有文件安装在后台完成，状态提交留在业务实体中串行执行。
use super::*;
use crate::kernel::update::{self, Channel, DownloadSource, ReleaseInfo};
use anyhow::Context as _;

impl PureClash {
    pub(super) fn kernel_update_locked(&self) -> bool {
        self.profile_actions_locked() || self.geodata_updating || self.kernel_rollback.is_some()
    }

    fn save_kernel_options(&mut self, previous: AppConfig, cx: &mut Context<Self>) {
        if let Err(error) = self.config.save(&self.paths.config_file) {
            self.config = previous;
            self.kernel_update_error(&error);
        } else {
            self.kernel_latest = None;
            self.kernel_update_status = None;
            self.kernel_update_last_attempt = None;
        }
        cx.notify();
    }

    pub(super) fn set_kernel_channel(&mut self, channel: Channel, cx: &mut Context<Self>) {
        if self.kernel_update_locked() {
            return;
        }
        let previous = self.config.clone();
        self.config.kernel_channel = channel;
        self.save_kernel_options(previous, cx);
    }

    pub(super) fn set_kernel_source(&mut self, source: DownloadSource, cx: &mut Context<Self>) {
        if self.kernel_update_locked() {
            return;
        }
        let previous = self.config.clone();
        self.config.kernel_download_source = source;
        self.save_kernel_options(previous, cx);
    }

    pub(super) fn toggle_kernel_auto_update(&mut self, cx: &mut Context<Self>) {
        if self.kernel_update_locked() {
            return;
        }
        let previous = self.config.clone();
        self.config.kernel_auto_update = !self.config.kernel_auto_update;
        self.save_kernel_options(previous, cx);
    }

    fn kernel_update_error(&mut self, error: &anyhow::Error) {
        log_warn!("update", "Mihomo 内核更新失败：{error:#}");
        self.kernel_update_status = Some(SharedString::from(
            t!(
                "kernel.failed",
                error = concise_error(&crate::logging::redact(&format!("{error:#}")), 200)
            )
            .into_owned(),
        ));
    }

    /// 检查与下载共用官方发布解析；检查按钮不会自动切换内核。
    pub(super) fn check_kernel_update(&mut self, install: bool, cx: &mut Context<Self>) {
        if self.kernel_update_locked() {
            return;
        }
        self.kernel_update_busy = true;
        self.kernel_update_last_attempt = Some(std::time::Instant::now());
        self.kernel_update_status = Some(tr(if install {
            "kernel.downloading"
        } else {
            "kernel.checking"
        }));
        let channel = self.config.kernel_channel;
        let source = self.config.kernel_download_source;
        let cached = if install {
            self.kernel_latest.clone()
        } else {
            None
        };
        let paths = self.paths.clone();
        let current = self.config.mihomo_version.clone();
        let available = self.kernel_available;
        let bootstrap = !available || self.geodata_info.revision.is_empty();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let release = match cached {
                        Some(release) => release,
                        None => update::latest(channel, source)?,
                    };
                    if install && (!available || release.needs_update(&current)) {
                        update::install(&paths, &release, source)?;
                    }
                    let geodata = if install {
                        Some(crate::mihomo::geodata::ensure_downloaded(&paths, source)?)
                    } else {
                        None
                    };
                    Ok::<_, anyhow::Error>((release, geodata))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.kernel_update_busy = false;
                match result {
                    Ok((release, geodata)) => {
                        if let Some(info) = geodata {
                            this.geodata_info = info;
                        }
                        if this.kernel_available
                            && !release.needs_update(&this.config.mihomo_version)
                        {
                            this.kernel_update_status = Some(tr("kernel.current"));
                            if install && bootstrap {
                                match profile::sync_runtime_file(
                                    &this.paths,
                                    this.baseline.as_ref(),
                                    &this.config.mihomo_version,
                                    this.active_profile.as_deref(),
                                ) {
                                    Ok(()) => this.start_core(cx),
                                    Err(error) => this.kernel_update_error(&error),
                                }
                            }
                        } else if install {
                            if let Err(error) =
                                this.activate_downloaded_kernel(&release, bootstrap, cx)
                            {
                                this.kernel_update_error(&error);
                            }
                        } else {
                            this.kernel_update_status = Some(SharedString::from(
                                t!("kernel.available", version = release.label.clone())
                                    .into_owned(),
                            ));
                        }
                        this.kernel_latest = Some(release);
                    }
                    Err(error) => {
                        this.kernel_latest = None;
                        this.kernel_update_error(&error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn activate_downloaded_kernel(
        &mut self,
        release: &ReleaseInfo,
        bootstrap: bool,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let previous = self.config.mihomo_version.clone();
        let had_kernel = self.kernel_available;
        let restart = self.core_active() || bootstrap;
        // sync_runtime_file 只有新版本 -t 通过后才原子替换 runtime；旧内核此时仍在运行。
        profile::sync_runtime_file(
            &self.paths,
            self.baseline.as_ref(),
            &release.version,
            self.active_profile.as_deref(),
        )?;
        self.config.mihomo_version = release.version.clone();
        if let Err(error) = self.config.save(&self.paths.config_file) {
            self.config.mihomo_version = previous.clone();
            if had_kernel {
                profile::sync_runtime_file(
                    &self.paths,
                    self.baseline.as_ref(),
                    &previous,
                    self.active_profile.as_deref(),
                )
                .context("保存版本失败，且恢复旧 runtime 失败")?;
            }
            return Err(error);
        }
        self.kernel_available = true;
        self.profile_error = None;
        self.mihomo_error = None;
        self.kernel_update_status = Some(tr("kernel.installed"));
        log_info!("update", "已安装 Mihomo {}", release.label);
        if restart {
            if had_kernel {
                self.kernel_rollback = Some(previous);
            }
            self.restart_core(cx);
        }
        Ok(())
    }

    /// 启动或 controller 就绪失败均恢复旧版本；保留代理托管与 TUN 意图。
    pub(super) fn rollback_kernel_update(
        &mut self,
        cause: &anyhow::Error,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(previous) = self.kernel_rollback.take() else {
            return false;
        };
        self.stop_core();
        self.config.mihomo_version = previous;
        let restore = profile::sync_runtime_file(
            &self.paths,
            self.baseline.as_ref(),
            &self.config.mihomo_version,
            self.active_profile.as_deref(),
        )
        .and_then(|_| self.config.save(&self.paths.config_file));
        match restore {
            Ok(()) => {
                self.kernel_update_status = Some(tr("kernel.rolled_back"));
                log_warn!("update", "新内核启动失败，恢复旧版本：{cause:#}");
                self.start_core(cx);
            }
            Err(error) => {
                self.disable_system_proxy();
                self.kernel_update_error(&error);
            }
        }
        cx.notify();
        true
    }

    pub(super) fn spawn_kernel_update_scheduler(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(60))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    // 无内核时让用户先选下载方式；更新任务在关闭窗口后继续执行。
                    if this.config.kernel_auto_update
                        && this.kernel_available
                        && this
                            .kernel_update_last_attempt
                            .is_none_or(|last| last.elapsed().as_secs() >= 24 * 60 * 60)
                    {
                        this.check_kernel_update(true, cx);
                    }
                });
            }
        })
        .detach();
    }
}
