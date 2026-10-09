use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use sha1::{Digest, Sha1};
use std::{
    fs,
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};

use anyhow::{bail, Result};

pub static PROGRESS_BAR: OnceLock<Arc<ProgressBar>> = OnceLock::new();

/// 在进度条存在时通过它输出（挂起重绘），否则直接 eprintln
pub fn log_msg(msg: &str) {
    if let Some(pb) = PROGRESS_BAR.get() {
        pb.println(msg);
    } else {
        eprintln!("{msg}");
    }
}

pub fn hex_eq(hash: &[u8], want: &str) -> bool {
    let got: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    got.eq_ignore_ascii_case(want)
}

pub fn verify_sha1(path: &Path, want: &str) -> bool {
    let data = match fs::read(path) {
        Ok(d) => d,
        Err(_) => return false,
    };
    let hash = Sha1::digest(&data);
    hex_eq(&hash, want)
}

pub async fn http_text(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()?;
    let body = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(body)
}

/// 检测用户是否位于中国大陆（结果缓存，检测失败视为否）
pub async fn is_cn_user() -> bool {
    static IN_CN: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();
    *IN_CN
        .get_or_init(|| async {
            let client = match reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(10))
                .build()
            {
                Ok(c) => c,
                Err(_) => return false,
            };
            // ip-api 免费接口仅 http，ipapi.co 支持 https，二者任一成功即可
            for url in ["http://ip-api.com/json/?fields=countryCode", "https://ipapi.co/json/"] {
                if let Ok(resp) = client.get(url).send().await {
                    if let Ok(v) = resp.json::<serde_json::Value>().await {
                        let code = v
                            .get("countryCode")
                            .or_else(|| v.get("country_code"))
                            .and_then(|c| c.as_str())
                            .unwrap_or("");
                        eprintln!("位置检测: {code}");
                        return code == "CN";
                    }
                }
            }
            eprintln!("位置检测失败，默认使用官方源");
            false
        })
        .await
}

pub async fn http_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()?;
    let body = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json::<T>()
        .await
        .map_err(anyhow::Error::from)?;
    Ok(body)
}

pub struct DownloadItem {
    pub url: String,
    pub dest: std::path::PathBuf,
    pub sha1: Option<String>,
    /// 镜像失败时的官方源 URL
    pub fallback_url: Option<String>,
    /// 已知大小，未知填 0
    pub size: u64,
}

/// 并行下载：reqwest + tokio + futures::buffer_unordered
pub async fn parallel_download(items: Vec<DownloadItem>) -> Result<()> {
    // 已存在且校验通过的跳过；文件缺失或校验失败的重下
    let items: Vec<DownloadItem> = items
        .into_iter()
        .filter(|it| match &it.sha1 {
            Some(w) => !it.dest.exists() || !verify_sha1(&it.dest, w),
            None => true,
        })
        .collect();
    if items.is_empty() {
        eprintln!("所有文件已存在，跳过下载");
        return Ok(());
    }
    let total = items.len();
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let concurrency = (cpus * 16).clamp(64, 256);
    log_msg(&format!("并行下载 {total} 个文件（并发 {concurrency}）…"));

    let total_bytes: u64 = items.iter().map(|it| it.size).sum();
    let pb = if total_bytes > 0 {
        ProgressBar::new(total_bytes)
    } else {
        // 总大小未知（如资源清单无 size）：不定长度样式
        ProgressBar::new_spinner()
    };
    let pb = pb.with_style(
        ProgressStyle::with_template(
            "[{elapsed_precise}] {bar:24.cyan/blue} {pos} {bytes_per_sec} {msg}",
        )
        .unwrap()
        .progress_chars("█░ "),
    )
    .with_message(format!("0/{total} 个文件"));
    let pb = Arc::new(pb);
    let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let total_started = std::time::Instant::now();
    PROGRESS_BAR.get_or_init(|| pb.clone());
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(60))
        .user_agent("mc-launcher/0.1")
        // 强制 HTTP/1.1：H2 单连接多路复用在镜像站上会被限速且易触发块超时
        .http1_only()
        .build()?;

    let results: Vec<(String, Result<(), String>)> =
        futures::stream::iter(items.into_iter().map(|it| {
            let client = client.clone();
            let done = done.clone();
            let pb = pb.clone();
            async move {
                let name = it.dest.display().to_string();
                let res = download_one(&client, &it).await;
                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                pb.inc(it.size);
                pb.set_message(format!("{n}/{total} 个文件"));
                (name, res)
            }
        }))
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;
    pb.finish_and_clear();

    let failed: Vec<&(String, Result<(), String>)> =
        results.iter().filter(|(_, r)| r.is_err()).collect();
    let elapsed = total_started.elapsed().as_secs_f64();
    eprintln!(
        "下载完成：{total} 个，失败 {}，耗时 {elapsed:.1}s（{:.1} 个/秒）",
        failed.len(),
        total as f64 / elapsed.max(0.1)
    );
    for (name, err) in failed.iter().take(10) {
        eprintln!("  失败: {name}: {}", err.as_ref().unwrap_err());
    }
    if !failed.is_empty() {
        bail!("有 {} 个文件下载失败", failed.len());
    }
    Ok(())
}

async fn download_one(client: &reqwest::Client, it: &DownloadItem) -> Result<(), String> {
    if let Some(parent) = it.dest.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut last_err = String::new();
    for attempt in 0..3 {
        match try_download(client, it).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = e;
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_millis(500 * (attempt as u64 + 1))).await;
                }
            }
        }
    }
    // 镜像失败，回退官方源
    if let Some(fallback) = &it.fallback_url {
        eprintln!("\r镜像失败，切换官方源重试: {}", it.dest.display());
        let it2 = DownloadItem {
            url: fallback.clone(),
            fallback_url: None,
            dest: it.dest.clone(),
            sha1: it.sha1.clone(),
            size: it.size,
        };
        for attempt in 0..3 {
            match try_download(client, &it2).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last_err = e;
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(500 * (attempt as u64 + 1)))
                            .await;
                    }
                }
            }
        }
    }
    Err(last_err)
}

async fn try_download(client: &reqwest::Client, it: &DownloadItem) -> Result<(), String> {
    let resp = client
        .get(&it.url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?;
    let total = resp.content_length();
    // > 8MB 用 Range 分块
    if total.map(|n| n > 8 * 1024 * 1024).unwrap_or(false) {
        drop(resp);
        return try_download_multipart(client, it, total.unwrap()).await;
    }
    // > 1MB 流式写盘
    if total.map(|n| n > 1024 * 1024).unwrap_or(false) {
        let _total = total.unwrap();
        let mut file = fs::File::create(&it.dest).map_err(|e| e.to_string())?;
        let mut stream = resp.bytes_stream();
        use futures::StreamExt;
        use std::io::Write;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            file.write_all(&chunk).map_err(|e| e.to_string())?;
            if let Some(pb) = PROGRESS_BAR.get() {
                pb.inc(chunk.len() as u64);
            }
        }
        drop(file);
        return verify_and_finish(it).await;
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if let Some(want) = &it.sha1 {
        let got = Sha1::digest(&bytes);
        if !hex_eq(&got, want) {
            return Err("SHA1 校验失败".into());
        }
    }
    fs::write(&it.dest, &bytes).map_err(|e| e.to_string())?;
    Ok(())
}

/// 大文件分块并行下载（Range 请求）
async fn try_download_multipart(
    client: &reqwest::Client,
    it: &DownloadItem,
    total: u64,
) -> Result<(), String> {
    const CHUNK: u64 = 8 * 1024 * 1024;
    const CONCURRENCY: usize = 8;

    // 先探测是否支持 Range（BMCLAPI 可能不支持）
    let probe = client
        .get(&it.url)
        .header("Range", "bytes=0-0")
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let range_ok = probe.status() == reqwest::StatusCode::PARTIAL_CONTENT
        && probe
            .headers()
            .get("content-range")
            .map(|v| v.to_str().unwrap_or_default().starts_with("bytes"))
            .unwrap_or(false);
    if !range_ok {
        // 不支持 Range 则整文件流式下载
        let resp = if probe.status().is_success() {
            probe
        } else {
            drop(probe);
            client
                .get(&it.url)
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .map_err(|e| e.to_string())?
        };
        let mut file = fs::File::create(&it.dest).map_err(|e| e.to_string())?;
        let mut stream = resp.bytes_stream();
        use futures::StreamExt;
        use std::io::Write;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            file.write_all(&chunk).map_err(|e| e.to_string())?;
            if let Some(pb) = PROGRESS_BAR.get() {
                pb.inc(chunk.len() as u64);
            }
        }
        drop(file);
        return verify_and_finish(it).await;
    }
    drop(probe);

    // 预分配文件
    let file = fs::File::create(&it.dest).map_err(|e| e.to_string())?;
    file.set_len(total).map_err(|e| e.to_string())?;
    drop(file);

    // 计算分块区间
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    let mut start = 0;
    while start < total {
        let end = (start + CHUNK - 1).min(total - 1);
        ranges.push((start, end));
        start = end + 1;
    }
    let total_chunks = ranges.len();

    let got = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let dest = it.dest.clone();
    let url = it.url.clone();
    const CHUNK_DEADLINE: Duration = Duration::from_secs(30);

    let results: Vec<Result<(), String>> = futures::stream::iter(ranges.into_iter().enumerate())
        .map(|(idx, (s, e))| {
            let client = client.clone();
            let url = url.clone();
            let dest = dest.clone();
            let got = got.clone();
            async move {
                let mut last_err = String::new();
                for attempt in 0..2 {
                    let resp = client
                        .get(&url)
                        .header("Range", format!("bytes={s}-{e}"))
                        .timeout(CHUNK_DEADLINE)
                        .send()
                        .await
                        .and_then(|r| r.error_for_status());
                    let resp = match resp {
                        Ok(r) => r,
                        Err(e) => {
                            last_err = e.to_string();
                            continue;
                        }
                    };
                    match tokio::time::timeout(
                        CHUNK_DEADLINE,
                        stream_to_file_at(resp, &dest, s),
                    )
                    .await
                    {
                        Ok(Ok(n)) => {
                            got.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
                            if let Some(pb) = PROGRESS_BAR.get() {
                                pb.inc(n);
                            }
                            return Ok(());
                        }
                        Ok(Err(err)) => {
                            last_err = err;
                        }
                        Err(_) => {
                            last_err = "块超时（30s），线路过慢".into();
                        }
                    }
                    if attempt < 1 {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
                Err(format!("块 {idx} ({s}-{e}): {last_err}"))
            }
        })
        .buffer_unordered(CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

    let failed: Vec<&Result<(), String>> = results.iter().filter(|r| r.is_err()).collect();
    if !failed.is_empty() {
        return Err(format!(
            "{}/{} 块下载失败: {}",
            failed.len(),
            total_chunks,
            failed[0].as_ref().unwrap_err()
        ));
    }
    verify_and_finish(it).await
}

/// 从 offset 位置开始流式写入文件
async fn stream_to_file_at(
    resp: reqwest::Response,
    dest: &Path,
    offset: u64,
) -> Result<u64, String> {
    use futures::StreamExt;
    use std::io::{Seek, SeekFrom, Write};
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(dest)
        .map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
    let mut stream = resp.bytes_stream();
    let mut n: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        file.write_all(&chunk).map_err(|e| e.to_string())?;
        n += chunk.len() as u64;
    }
    Ok(n)
}

/// 下载完成后校验 SHA1
async fn verify_and_finish(it: &DownloadItem) -> Result<(), String> {
    if let Some(want) = &it.sha1 {
        let data = tokio::task::spawn_blocking({
            let path = it.dest.clone();
            move || fs::read(&path)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
        let got = Sha1::digest(&data);
        if !hex_eq(&got, want) {
            return Err("SHA1 校验失败".into());
        }
    }
    Ok(())
}
