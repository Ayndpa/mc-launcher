use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use sha1::{Digest, Sha1};
use std::{fs, path::Path};

use crate::util::{hex_eq, verify_sha1};

/// Fabric profile（内嵌）
pub const EMBEDDED_FABRIC_PROFILE: &str = include_str!("embedded/fabric/profile.json");

/// 内嵌的 fabric-api jar 与元数据
pub const EMBEDDED_FABRIC_API_JAR: &[u8] = include_bytes!("embedded/mods/fabric-api-0.155.3+26.1.2.jar");
pub const EMBEDDED_FABRIC_API_META: &str = include_str!("embedded/mods/fabric-api.json");

#[derive(Deserialize, serde::Serialize)]
pub struct FabricProfile {
    pub id: String,
    #[serde(rename = "mainClass")]
    pub main_class: String,
    pub libraries: Vec<FabricLib>,
}

#[derive(Deserialize, serde::Serialize)]
pub struct FabricLib {
    pub name: String,
    #[serde(default)]
    pub sha1: Option<String>,
}

#[derive(Deserialize)]
struct FabricApiMeta {
    filename: String,
    sha1: String,
}

pub fn fabric_profile() -> Result<FabricProfile> {
    Ok(serde_json::from_str(EMBEDDED_FABRIC_PROFILE)?)
}

/// Maven 坐标转相对路径
pub fn fabric_lib_rel(name: &str) -> Result<String> {
    let mut segs = name.split(':');
    let group = segs.next().ok_or_else(|| anyhow!("bad lib name"))?;
    let artifact = segs.next().ok_or_else(|| anyhow!("bad lib name"))?;
    let version = segs.next().ok_or_else(|| anyhow!("bad lib name"))?;
    let dir = group.replace('.', "/");
    Ok(format!("{dir}/{artifact}/{version}/{artifact}-{version}.jar"))
}

/// 内嵌 Fabric 库文件（Fabric 允许二次分发，无需联网下载）
pub fn fabric_lib_embedded(name: &str) -> Option<&'static [u8]> {
    let rel = fabric_lib_rel(name).ok()?;
    let file = rel.rsplit('/').next()?;
    Some(match file {
        "asm-9.10.1.jar" => include_bytes!("embedded/fabric/libs/asm-9.10.1.jar"),
        "asm-analysis-9.10.1.jar" => include_bytes!("embedded/fabric/libs/asm-analysis-9.10.1.jar"),
        "asm-commons-9.10.1.jar" => include_bytes!("embedded/fabric/libs/asm-commons-9.10.1.jar"),
        "asm-tree-9.10.1.jar" => include_bytes!("embedded/fabric/libs/asm-tree-9.10.1.jar"),
        "asm-util-9.10.1.jar" => include_bytes!("embedded/fabric/libs/asm-util-9.10.1.jar"),
        "sponge-mixin-0.17.4+mixin.0.8.7.jar" => include_bytes!("embedded/fabric/libs/sponge-mixin-0.17.4+mixin.0.8.7.jar"),
        "fabric-loader-0.19.5.jar" => include_bytes!("embedded/fabric/libs/fabric-loader-0.19.5.jar"),
        _ => return None,
    })
}

/// 写出内嵌 Fabric 库到 libraries/
pub async fn install_fabric(libs_dir: &Path) -> Result<FabricProfile> {
    let profile = fabric_profile()?;
    eprintln!("Fabric {}: 写入内嵌库文件…", profile.id);
    for lib in &profile.libraries {
        let rel = fabric_lib_rel(&lib.name)?;
        let dest = libs_dir.join(&rel);
        if let Some(data) = fabric_lib_embedded(&lib.name) {
            // 校验已存在的文件，不合法则重写
            if dest.is_file() {
                let ok = match &lib.sha1 {
                    Some(w) => verify_sha1(&dest, w),
                    None => true,
                };
                if ok {
                    continue;
                }
            }
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&dest, data)?;
        } else {
            bail!("缺少内嵌 Fabric 库: {}", lib.name);
        }
    }
    Ok(profile)
}

/// 从编译时内嵌的 jar 安装 fabric-api 到 mods/
pub async fn ensure_fabric_api(instance: &Path) -> Result<()> {
    let mods_dir = instance.join("mods");
    if fs::read_dir(&mods_dir)
        .map(|rd| {
            rd.flatten()
                .any(|e| e.file_name().to_string_lossy().to_lowercase().starts_with("fabric-api"))
        })
        .unwrap_or(false)
    {
        eprintln!("mods/ 中已有 fabric-api，跳过");
        return Ok(());
    }
    fs::create_dir_all(&mods_dir)?;
    let meta: FabricApiMeta = serde_json::from_str(EMBEDDED_FABRIC_API_META)?;
    let dest = mods_dir.join(&meta.filename);
    let data = fs::read(&dest).unwrap_or_default();
    let hash = Sha1::digest(&data);
    if dest.is_file() && hex_eq(&hash, &meta.sha1) {
        eprintln!("fabric-api 已存在且校验通过，跳过");
        return Ok(());
    }
    eprintln!("安装内嵌 fabric-api: {} …", meta.filename);
    fs::write(&dest, EMBEDDED_FABRIC_API_JAR)?;
    Ok(())
}
