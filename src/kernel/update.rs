//! 用户目录中的内核安装；发布资源固定到资源 ID，并在解压、执行前验证 SHA-256。
use std::{
    fs,
    io::{Cursor, Read, Write},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::platform::{AppPaths, kernel_binary_name};

const API: &str = "https://api.github.com/repos/MetaCubeX/mihomo/releases";
const MAX_ARCHIVE: u64 = 80 * 1024 * 1024;
const MAX_BINARY: u64 = 160 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Channel {
    #[default]
    Stable,
    Prebuild,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DownloadSource {
    #[default]
    Direct,
    GhProxy,
    Ghfast,
}

impl DownloadSource {
    pub(crate) fn all() -> [Self; 3] {
        [Self::Direct, Self::GhProxy, Self::Ghfast]
    }
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Direct => "GitHub",
            Self::GhProxy => "gh-proxy.com",
            Self::Ghfast => "ghfast.top",
        }
    }
    pub(crate) fn url(self, url: &str) -> String {
        match self {
            Self::Direct => url.to_owned(),
            Self::GhProxy => format!("https://gh-proxy.com/{url}"),
            Self::Ghfast => format!("https://ghfast.top/{url}"),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct Asset {
    id: u64,
    name: String,
    size: u64,
    digest: Option<String>,
    browser_download_url: String,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Clone, Debug)]
pub(crate) struct ReleaseInfo {
    pub(crate) version: String,
    pub(crate) label: String,
    asset: Asset,
}

impl ReleaseInfo {
    pub(crate) fn needs_update(&self, current: &str) -> bool {
        fn stable_version(version: &str) -> Option<(u64, u64, u64)> {
            let mut parts = version.trim_start_matches('v').split('.');
            let result = (
                parts.next()?.parse().ok()?,
                parts.next()?.parse().ok()?,
                parts.next()?.parse().ok()?,
            );
            parts.next().is_none().then_some(result)
        }
        let current_base = current.split('-').next().unwrap_or(current);
        if let (Some(installed), Some(latest)) =
            (stable_version(current_base), stable_version(&self.label))
        {
            if installed > latest {
                return false;
            }
            // 旧版随包目录没有资源 ID，同一正式版本不必重复安装。
            if installed == latest && current == current_base {
                return false;
            }
        }
        self.version != current
    }
}

fn agent() -> ureq::Agent {
    // 首次启动不能依赖失效的环境代理或尚不存在的本机内核。
    ureq::AgentBuilder::new()
        .try_proxy_from_env(false)
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(45))
        .timeout(Duration::from_secs(180))
        .build()
}

fn read_bounded(reader: impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "下载或解压文件超过大小限制");
    Ok(bytes)
}

pub(crate) fn fetch(url: &str, source: DownloadSource, limit: u64) -> Result<Vec<u8>> {
    let url = source.url(url);
    let response = agent()
        .get(&url)
        .set("User-Agent", "pure-clash/kernel-update")
        .call()
        .context("下载失败，请切换 GitHub 下载方式后重试")?;
    read_bounded(response.into_reader(), limit)
}

pub(crate) fn fetch_metadata(url: &str, source: DownloadSource) -> Result<Vec<u8>> {
    fetch(url, DownloadSource::Direct, 4 * 1024 * 1024).or_else(|error| {
        if source == DownloadSource::Direct {
            Err(error)
        } else {
            fetch(url, DownloadSource::GhProxy, 4 * 1024 * 1024)
        }
    })
}

pub(crate) fn latest(channel: Channel, source: DownloadSource) -> Result<ReleaseInfo> {
    let target = target_asset()?;
    let endpoint = match channel {
        Channel::Stable => "latest",
        Channel::Prebuild => "tags/Prerelease-Alpha",
    };
    let url = format!("{API}/{endpoint}");
    // ghfast 仅代理资源下载；元数据优先从官方 API 读取，不可达时使用支持 API 的 GH-Proxy。
    let bytes = fetch_metadata(&url, source)?;
    parse_release(&bytes, channel, target)
}

fn target_asset() -> Result<(&'static str, &'static str)> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Ok(("mihomo-windows-amd64-compatible-", ".zip")),
        ("linux", "x86_64") => Ok(("mihomo-linux-amd64-", ".gz")),
        _ => bail!("在线内核安装目前仅支持 Windows x64 和 Linux x64"),
    }
}

fn parse_release(
    bytes: &[u8],
    channel: Channel,
    (prefix, suffix): (&str, &str),
) -> Result<ReleaseInfo> {
    let release: Release = serde_json::from_slice(bytes).context("GitHub 发布信息无效")?;
    ensure!(!release.draft, "不允许下载草稿发布");
    let asset = release
        .assets
        .into_iter()
        .find(|asset| {
            let Some(version) = asset
                .name
                .strip_prefix(prefix)
                .and_then(|name| name.strip_suffix(suffix))
            else {
                return false;
            };
            match channel {
                Channel::Stable => {
                    !release.prerelease && version == release.tag_name && version.starts_with('v')
                }
                Channel::Prebuild => {
                    release.tag_name == "Prerelease-Alpha"
                        && version.starts_with("alpha-")
                        && super::is_valid_version(version)
                }
            }
        })
        .context("发布中没有当前平台的内核资源")?;
    ensure!(
        asset.id > 0 && asset.size > 0 && asset.size <= MAX_ARCHIVE,
        "内核资源大小或 ID 无效"
    );
    digest(&asset)?;
    let label = asset
        .name
        .strip_prefix(prefix)
        .unwrap()
        .strip_suffix(suffix)
        .unwrap()
        .to_owned();
    ensure!(super::is_valid_version(&label), "内核版本名称无效");
    let expected = format!(
        "https://github.com/MetaCubeX/mihomo/releases/download/{}/{}",
        release.tag_name, asset.name
    );
    ensure!(
        asset.browser_download_url == expected,
        "发布资源不是官方 Mihomo 下载地址"
    );
    // Alpha 标签不变，资源重新上传即是新构建；稳定版也保留资源身份，避免覆盖运行中的文件。
    let version = format!("{}-{}", label.trim_start_matches('v'), asset.id);
    Ok(ReleaseInfo {
        version,
        label,
        asset,
    })
}

fn digest(asset: &Asset) -> Result<&str> {
    let hash = asset
        .digest
        .as_deref()
        .and_then(|value| value.strip_prefix("sha256:"))
        .context("官方资源缺少 SHA-256，已拒绝安装")?;
    ensure!(
        hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
        "官方 SHA-256 无效"
    );
    Ok(hash)
}

fn verify(bytes: &[u8], size: u64, hash: &str) -> Result<()> {
    ensure!(bytes.len() as u64 == size, "下载文件大小不匹配");
    ensure!(
        format!("{:x}", Sha256::digest(bytes)).eq_ignore_ascii_case(hash),
        "下载文件 SHA-256 不匹配"
    );
    Ok(())
}

fn unzip(bytes: &[u8], name: &str) -> Result<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).context("ZIP 文件无效")?;
    // 只读取指定条目，不展开任意路径，避免 ZIP 路径穿越和无关文件落盘。
    let file = archive
        .by_name(name)
        .context("ZIP 中缺少预期的内核或驱动")?;
    ensure!(!file.is_dir(), "ZIP 目标不是普通文件");
    read_bounded(file, MAX_BINARY)
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn validate_version(path: &Path, expected: &str) -> Result<()> {
    let mut command = std::process::Command::new(path);
    command.arg("-v").stdin(std::process::Stdio::null());
    crate::platform::KernelProcessGuard::prepare_command(&mut command);
    let output = command.output().context("无法执行已下载的内核")?;
    ensure!(output.status.success(), "已下载的内核无法报告版本");
    ensure!(
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .any(|part| part == expected),
        "已下载的内核版本与发布不匹配"
    );
    Ok(())
}

pub(crate) fn install(
    paths: &AppPaths,
    release: &ReleaseInfo,
    source: DownloadSource,
) -> Result<()> {
    let root = paths.data_dir.join("kernel");
    fs::create_dir_all(&root)?;
    let destination = root.join(&release.version);
    // 已完成安装的目录不可覆盖；应用可能仍持有其中的可执行文件。
    if destination.is_dir() {
        return verify_installed(&destination, release);
    }
    let staging = root.join(format!(".download-{}", uuid::Uuid::new_v4().simple()));
    fs::create_dir(&staging)?;
    let result = (|| {
        let archive = fetch(&release.asset.browser_download_url, source, MAX_ARCHIVE)?;
        verify(&archive, release.asset.size, digest(&release.asset)?)?;
        let binary = if release.asset.name.ends_with(".zip") {
            unzip(&archive, "mihomo-windows-amd64-compatible.exe")?
        } else {
            read_bounded(flate2::read::GzDecoder::new(archive.as_slice()), MAX_BINARY)?
        };
        ensure!(!binary.is_empty(), "内核文件为空");
        write_synced(&staging.join(kernel_binary_name()), &binary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                staging.join(kernel_binary_name()),
                fs::Permissions::from_mode(0o755),
            )?;
        }
        if cfg!(target_os = "windows") {
            install_wintun(&staging)?;
        }
        validate_version(&staging.join(kernel_binary_name()), &release.label)?;
        write_synced(
            &staging.join("LICENSE"),
            include_bytes!("../../kernel/1.19.30/LICENSE"),
        )?;
        write_synced(
            &staging.join("NOTICE.md"),
            format!(
                "Mihomo {}\nGPL-3.0\nSource: https://github.com/MetaCubeX/mihomo/tree/{}\nDownload: {}\nPure Clash is not affiliated with MetaCubeX.\n",
                release.label, if release.label.starts_with("alpha-") { release.label.trim_start_matches("alpha-").to_owned() } else { release.label.clone() }, release.asset.browser_download_url
            ).as_bytes(),
        )?;
        write_synced(
            &staging.join("installed.json"),
            &serde_json::to_vec(&serde_json::json!({
                "asset_id": release.asset.id, "archive_sha256": digest(&release.asset)?,
                "binary_sha256": format!("{:x}", Sha256::digest(&binary)),
            }))?,
        )?;
        // 整个目录成功落盘后才可被选择；下载、校验或解压失败均不碰旧内核。
        fs::rename(&staging, &destination).context("提交内核安装目录失败")?;
        Ok(())
    })();
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn verify_installed(path: &Path, release: &ReleaseInfo) -> Result<()> {
    let info: serde_json::Value = serde_json::from_slice(&fs::read(path.join("installed.json"))?)?;
    ensure!(
        info["asset_id"].as_u64() == Some(release.asset.id)
            && info["archive_sha256"].as_str() == Some(digest(&release.asset)?),
        "已安装内核的发布身份不匹配"
    );
    let binary = read_bounded(fs::File::open(path.join(kernel_binary_name()))?, MAX_BINARY)?;
    ensure!(
        info["binary_sha256"].as_str() == Some(&format!("{:x}", Sha256::digest(&binary))),
        "已安装内核已损坏"
    );
    if cfg!(target_os = "windows") {
        verify_wintun(&fs::read(path.join("wintun.dll"))?)?;
    }
    Ok(())
}

fn wintun_metadata() -> serde_json::Value {
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../kernel/1.19.30/manifest.json"))
            .expect("构建时已校验 manifest");
    manifest["targets"]["windows-amd64"]["wintun"].clone()
}

fn verify_wintun(bytes: &[u8]) -> Result<()> {
    let metadata = wintun_metadata();
    verify(
        bytes,
        metadata["file_size"].as_u64().context("驱动大小缺失")?,
        metadata["file_sha256"].as_str().context("驱动哈希缺失")?,
    )
}

fn install_wintun(path: &Path) -> Result<()> {
    let metadata = wintun_metadata();
    let archive = fetch(
        metadata["source_url"].as_str().context("驱动来源缺失")?,
        DownloadSource::Direct,
        MAX_ARCHIVE,
    )?;
    ensure!(
        format!("{:x}", Sha256::digest(&archive))
            == metadata["archive_sha256"]
                .as_str()
                .context("驱动哈希缺失")?,
        "Wintun 压缩包 SHA-256 不匹配"
    );
    let binary = unzip(&archive, "wintun/bin/amd64/wintun.dll")?;
    verify_wintun(&binary)?;
    write_synced(&path.join("wintun.dll"), &binary)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn release(tag: &str, name: &str, id: u64) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"tag_name":tag,"draft":false,"prerelease":tag=="Prerelease-Alpha","assets":[{
            "id":id,"name":name,"size":3,"digest":format!("sha256:{:x}",Sha256::digest(b"abc")),
            "browser_download_url":format!("https://github.com/MetaCubeX/mihomo/releases/download/{tag}/{name}")
        }]})).unwrap()
    }
    #[test]
    fn stable_asset_does_not_match_cpu_variants() {
        let info = parse_release(
            &release("v1.2.3", "mihomo-linux-amd64-v1.2.3.gz", 10),
            Channel::Stable,
            ("mihomo-linux-amd64-", ".gz"),
        )
        .unwrap();
        assert_eq!(info.version, "1.2.3-10");
        assert!(
            parse_release(
                &release("v1.2.3", "mihomo-linux-amd64-v3-v1.2.3.gz", 10),
                Channel::Stable,
                ("mihomo-linux-amd64-", ".gz")
            )
            .is_err()
        );
    }
    #[test]
    fn alpha_identity_changes_when_fixed_tag_is_republished() {
        let parse = |id| {
            parse_release(
                &release("Prerelease-Alpha", "mihomo-linux-amd64-alpha-abc123.gz", id),
                Channel::Prebuild,
                ("mihomo-linux-amd64-", ".gz"),
            )
            .unwrap()
        };
        assert_ne!(parse(10).version, parse(11).version);
        assert_eq!(parse(10).label, "alpha-abc123");
    }
    #[test]
    fn rejects_missing_digest_corruption_and_oversized_files() {
        let mut json: serde_json::Value =
            serde_json::from_slice(&release("v1.2.3", "mihomo-linux-amd64-v1.2.3.gz", 10)).unwrap();
        json["assets"][0]["digest"] = serde_json::Value::Null;
        assert!(
            parse_release(
                &serde_json::to_vec(&json).unwrap(),
                Channel::Stable,
                ("mihomo-linux-amd64-", ".gz")
            )
            .is_err()
        );
        assert!(verify(b"abd", 3, &format!("{:x}", Sha256::digest(b"abc"))).is_err());
        assert!(read_bounded(&b"abcd"[..], 3).is_err());
    }
    #[test]
    fn mirror_wraps_only_the_public_download_url() {
        let url = "https://github.com/MetaCubeX/mihomo/releases/download/v1.2.3/a.gz";
        assert_eq!(DownloadSource::Direct.url(url), url);
        assert_eq!(
            DownloadSource::GhProxy.url(url),
            format!("https://gh-proxy.com/{url}")
        );
    }

    #[test]
    fn stable_updates_do_not_downgrade_and_channels_can_switch() {
        let stable = parse_release(
            &release("v1.2.3", "mihomo-linux-amd64-v1.2.3.gz", 10),
            Channel::Stable,
            ("mihomo-linux-amd64-", ".gz"),
        )
        .unwrap();
        assert!(!stable.needs_update("1.2.4-9"));
        assert!(!stable.needs_update("1.2.3"));
        assert!(!stable.needs_update("1.2.3-10"));
        assert!(stable.needs_update("1.2.3-9"));
        assert!(stable.needs_update("alpha-abc123-9"));
        assert!(stable.needs_update("1.2.2-9"));
    }

    #[test]
    fn zip_reads_only_the_expected_entry() {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file("../mihomo.exe", zip::write::FileOptions::default())
            .unwrap();
        zip.write_all(b"bad").unwrap();
        zip.start_file(
            "mihomo-windows-amd64-compatible.exe",
            zip::write::FileOptions::default(),
        )
        .unwrap();
        zip.write_all(b"expected").unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        assert_eq!(
            unzip(&bytes, "mihomo-windows-amd64-compatible.exe").unwrap(),
            b"expected"
        );
        assert!(unzip(&bytes, "mihomo.exe").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn installation_is_verified_and_failures_preserve_old_kernel() {
        use std::net::TcpListener;
        let root =
            std::env::temp_dir().join(format!("pure-clash-install-{}", uuid::Uuid::new_v4()));
        let paths = AppPaths::portable(&root);
        let kernels = paths.data_dir.join("kernel");
        fs::create_dir_all(kernels.join("previous")).unwrap();
        fs::write(
            kernels.join("previous").join(kernel_binary_name()),
            b"old kernel",
        )
        .unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder
            .write_all(b"#!/bin/sh\necho 'Mihomo Meta v1.2.3'\n")
            .unwrap();
        let archive = encoder.finish().unwrap();
        let serve = |body: Vec<u8>| {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/kernel.gz", listener.local_addr().unwrap());
            let handle = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 1024];
                stream.read(&mut request).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(&body).unwrap();
            });
            (url, handle)
        };
        let mut info = parse_release(
            &release("v1.2.3", "mihomo-linux-amd64-v1.2.3.gz", 10),
            Channel::Stable,
            ("mihomo-linux-amd64-", ".gz"),
        )
        .unwrap();
        info.asset.size = archive.len() as u64;
        info.asset.digest = Some(format!("sha256:{:x}", Sha256::digest(&archive)));
        let (url, handle) = serve(archive.clone());
        info.asset.browser_download_url = url;
        install(&paths, &info, DownloadSource::Direct).unwrap();
        handle.join().unwrap();
        // 已安装资源可离线重用，二进制被更改时必须拒绝使用。
        install(&paths, &info, DownloadSource::Direct).unwrap();
        fs::write(
            kernels.join(&info.version).join(kernel_binary_name()),
            b"damaged",
        )
        .unwrap();
        assert!(install(&paths, &info, DownloadSource::Direct).is_err());
        info.version = "candidate".into();
        let (url, handle) = serve(vec![0; archive.len()]);
        info.asset.browser_download_url = url;
        assert!(install(&paths, &info, DownloadSource::Direct).is_err());
        handle.join().unwrap();
        assert!(!kernels.join("candidate").exists());
        assert_eq!(
            fs::read(kernels.join("previous").join(kernel_binary_name())).unwrap(),
            b"old kernel"
        );
        assert!(fs::read_dir(&kernels).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".download-")
        }));
        fs::remove_dir_all(root).unwrap();
    }
}
