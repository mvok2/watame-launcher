#![windows_subsystem = "windows"]

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::Utc;
use discord_rich_presence::{activity, DiscordIpc, DiscordIpcClient};
use eframe::egui;
use serde::Deserialize;
use urlencoding::decode;
use winapi::um::handleapi::CloseHandle;
use winapi::um::processthreadsapi::OpenProcess;
use winapi::um::synchapi::WaitForSingleObject;
use winapi::um::winbase::INFINITE;
use winapi::um::winnt::SYNCHRONIZE;
use winreg::enums::*;
use winreg::RegKey;
use zip::ZipArchive;

const VERSION: &str = "1.3.2";
const LASTVER_URL: &str = "https://gummy.live/UnsecuredContent/lastVer.json";
const DSC_CLIENT_ID: &str = "1553387026424795186";
const BASE_HOST: &str = "https://gummy.live";
const CREATE_NO_WINDOW: u32 = 0x08000000;

const BG: egui::Color32 = egui::Color32::from_rgb(0xD3, 0x21, 0x17);
const FG: egui::Color32 = egui::Color32::from_rgb(0xF2, 0xD8, 0xD7);

fn exe_dir() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn log_init() {
    let path = exe_dir().join("watame-launcher.log");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "=== Kaizen Launcher v{} started ===", VERSION);
    }
}

fn log_line(msg: &str) {
    let path = exe_dir().join("watame-launcher.log");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{}] {}", Utc::now().to_rfc3339(), msg);
    }
}

fn b64url_decode(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut buf: u32 = 0;
    let mut bits: u8 = 0;
    for &c in input.as_bytes() {
        if matches!(c, b'=' | b'\n' | b'\r' | b' ' | b'\t') {
            continue;
        }
        let v = val(c)?;
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

#[allow(non_snake_case)]
#[derive(Deserialize, Debug)]
struct VersionInfo {
    lastVersion: String,
    lastDownload: String,
}

#[derive(Deserialize, Debug)]
struct LastVer {
    client: VersionInfo,
    studio: VersionInfo,
    launcher: VersionInfo,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct PlaceDetails {
    #[serde(default)]
    name: String,
    #[serde(default)]
    builder: Option<String>,
    #[serde(default)]
    image_token: Option<String>,
}

// ---------------------------------------------------------------------------
// Paths — LOCALAPPDATA, with migration from old APPDATA\Watame
// ---------------------------------------------------------------------------

fn root() -> Result<PathBuf> {
    let mut p: PathBuf = env::var_os("LOCALAPPDATA")
        .context("LOCALAPPDATA not found")?
        .into();
    p.push("Watame");
    Ok(p)
}

fn old_root() -> Option<PathBuf> {
    env::var_os("APPDATA").map(|v| {
        let mut p: PathBuf = v.into();
        p.push("Watame");
        p
    })
}

fn versions_dir() -> Result<PathBuf> {
    let mut p = root()?;
    p.push("Versions");
    Ok(p)
}

fn launcher_exe_path() -> Result<PathBuf> {
    let mut p = root()?;
    p.push("WatameLauncherBeta.exe");
    Ok(p)
}

fn active_client_version() -> Option<String> {
    let mut p = root().ok()?;
    p.push("client_version.txt");
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn active_studio_version() -> Option<String> {
    let mut p = root().ok()?;
    p.push("studio_version.txt");
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn write_active_client_version(v: &str) {
    if let Ok(mut p) = root() {
        p.push("client_version.txt");
        let _ = fs::write(p, v);
    }
}

fn write_active_studio_version(v: &str) {
    if let Ok(mut p) = root() {
        p.push("studio_version.txt");
        let _ = fs::write(p, v);
    }
}

fn client_dir_for(ver: &str) -> Result<PathBuf> {
    let mut p = versions_dir()?;
    p.push(format!("version-{}", ver));
    Ok(p)
}

fn client_exe_for(ver: &str) -> Result<PathBuf> {
    let mut p = client_dir_for(ver)?;
    p.push("RobloxPlayerBeta.exe");
    Ok(p)
}

fn studio_exe_for(ver: &str) -> Result<PathBuf> {
    let mut p = client_dir_for(ver)?;
    p.push("RobloxStudioBeta.exe");
    Ok(p)
}

fn migrate_legacy_install() -> Result<()> {
    let new_root = root()?;
    if new_root.exists() {
        return Ok(());
    }
    let old = match old_root() {
        Some(o) => o,
        None => return Ok(()),
    };
    if !old.exists() {
        return Ok(());
    }
    log_line(&format!("migrating {} -> {}", old.display(), new_root.display()));
    copy_dir_recursive(&old, &new_root)?;
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

fn cache_bust(url: &str) -> String {
    let ts = Utc::now().timestamp();
    if url.contains('?') {
        format!("{}&_={}", url, ts)
    } else {
        format!("{}?_={}", url, ts)
    }
}

fn filesdownloader(url: &str, path: &PathBuf) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .build()?;
    let mut attempts = 0;
    loop {
        attempts += 1;
        match do_download(&client, url, path) {
            Ok(_) => return Ok(()),
            Err(e) => {
                if attempts >= 3 {
                    return Err(e).context("download failed");
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

fn do_download(client: &reqwest::blocking::Client, url: &str, path: &PathBuf) -> Result<()> {
    let mut r = client.get(url).send()?.error_for_status()?;
    let mut f = File::create(path)?;
    std::io::copy(&mut r, &mut f)?;
    Ok(())
}

fn unzip(path: &PathBuf, out: &PathBuf) -> Result<()> {
    let file = fs::File::open(path)?;
    let mut archive = ZipArchive::new(file)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let outpath = match entry.enclosed_name() {
            Some(p) => out.join(p),
            None => continue,
        };
        if entry.name().ends_with('/') {
            fs::create_dir_all(&outpath)?;
        } else {
            if let Some(parent) = outpath.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut outfile = fs::File::create(&outpath)?;
            std::io::copy(&mut entry, &mut outfile)?;
        }
    }
    Ok(())
}

fn fetch_lastver() -> Option<LastVer> {
    let url = cache_bust(LASTVER_URL);
    let resp = reqwest::blocking::get(&url).ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<LastVer>().ok()
}

fn updatechecker(ui_status: &Arc<Mutex<String>>, passthrough_args: &[String]) -> Result<()> {
    migrate_legacy_install()?;

    let lastver = fetch_lastver();
    let lp = launcher_exe_path()?;

    let need_launcher = match lastver.as_ref() {
        Some(lv) => lv.launcher.lastVersion.trim() != VERSION || !lp.exists(),
        None => !lp.exists(),
    };

    if need_launcher {
        if lp.exists() {
            let old = lp.with_extension("old");
            let _ = fs::remove_file(&old);
            let _ = fs::rename(&lp, &old);
        }
        let url = match lastver.as_ref() {
            Some(lv) => lv.launcher.lastDownload.clone(),
            None => return Err(anyhow::anyhow!("no launcher url")),
        };
        *ui_status.lock().unwrap() = "Downloading Launcher Update...".to_string();
        filesdownloader(&cache_bust(&url), &lp)?;
        let mut cmd = Command::new(&lp);
        for a in passthrough_args {
            cmd.arg(a);
        }
        cmd.spawn()?;
        std::process::exit(0);
    }

    if let Some(lv) = lastver.as_ref() {
        let want = lv.client.lastVersion.trim().to_string();
        let have = active_client_version();
        let exe_ok = client_exe_for(&want).map(|p| p.exists()).unwrap_or(false);
        let need_client = have.as_deref() != Some(&want) || !exe_ok;

        if need_client {
            let url = lv.client.lastDownload.clone();
            *ui_status.lock().unwrap() = "Downloading Kaizen Player...".to_string();
            let zp = root()?.join("WatamePlayerBeta.zip");
            filesdownloader(&cache_bust(&url), &zp)?;
            *ui_status.lock().unwrap() = "Extracting Client...".to_string();
            let dst = client_dir_for(&want)?;
            unzip(&zp, &dst)?;
            let _ = fs::remove_file(&zp);
            write_active_client_version(&want);
            delete_protocol("Watame");
            delete_protocol("watame");
            addprotocol()?;
        }

        let want_s = lv.studio.lastVersion.trim().to_string();
        let have_s = active_studio_version();
        let sexe_ok = studio_exe_for(&want_s).map(|p| p.exists()).unwrap_or(false);
        let need_studio = have_s.as_deref() != Some(&want_s) || !sexe_ok;

        if need_studio {
            let url = lv.studio.lastDownload.clone();
            *ui_status.lock().unwrap() = "Downloading Kaizen Studio...".to_string();
            let zp = root()?.join("WatameStudioBeta.zip");
            filesdownloader(&cache_bust(&url), &zp)?;
            *ui_status.lock().unwrap() = "Extracting Studio...".to_string();
            let dst = client_dir_for(&want_s)?;
            unzip(&zp, &dst)?;
            let _ = fs::remove_file(&zp);
            write_active_studio_version(&want_s);
        }
    }

    create_desktop_shortcut(&launcher_exe_path()?, None, "Kaizen Launcher")?;
    create_desktop_shortcut(&launcher_exe_path()?, Some("--studio"), "Kaizen Studio")?;
    let old = lp.with_extension("old");
    if old.exists() {
        let _ = fs::remove_file(&old);
    }
    Ok(())
}

fn delete_protocol(name: &str) {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let _ = hkcu.delete_subkey_all(format!(r"Software\Classes\{}", name));
}

fn register_protocol(scheme: &str, description: &str) -> Result<()> {
    let lp = launcher_exe_path()?;
    let exe = lp.to_str().context("invalid launcher path")?.to_string();
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (k, _) = hkcu.create_subkey(format!(r"Software\Classes\{}", scheme))?;
    k.set_value("", &format!("URL:{} Protocol", description))?;
    k.set_value("URL Protocol", &"")?;
    let (icon, _) = k.create_subkey("DefaultIcon")?;
    icon.set_value("", &format!("\"{}\",0", exe))?;
    let (cmd, _) = k.create_subkey(r"shell\open\command")?;
    cmd.set_value("", &format!("\"{}\" \"%1\"", exe))?;
    Ok(())
}

fn addprotocol() -> Result<()> {
    register_protocol("Watame", "Watame")?;
    register_protocol("watame", "Watame")?;
    Ok(())
}

fn create_desktop_shortcut(
    target_exe: &PathBuf,
    args: Option<&str>,
    shortcut_name: &str,
) -> Result<()> {
    let user_profile = match env::var("USERPROFILE") {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let mut shortcut_path = PathBuf::from(user_profile);
    shortcut_path.push("Desktop");
    shortcut_path.push(format!("{}.lnk", shortcut_name));
    if shortcut_path.exists() {
        return Ok(());
    }
    let target_str = target_exe.to_str().unwrap_or("");
    let shortcut_str = shortcut_path.to_str().unwrap_or("");
    let args_str = args.unwrap_or("");
    let ps = format!(
        "$w = New-Object -ComObject WScript.Shell; \
         $s = $w.CreateShortcut('{}'); \
         $s.TargetPath = '{}'; \
         $s.Arguments = '{}'; \
         $s.Save()",
        shortcut_str, target_str, args_str
    );
    let _ = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-WindowStyle")
        .arg("Hidden")
        .arg("-Command")
        .arg(&ps)
        .output()?;
    Ok(())
}

fn strip_scheme<'a>(arg: &'a str, schemes: &[&str]) -> Option<&'a str> {
    for scheme in schemes {
        if arg.len() >= scheme.len() && arg[..scheme.len()].eq_ignore_ascii_case(scheme) {
            return Some(&arg[scheme.len()..]);
        }
    }
    None
}

fn parse_bootstrapper_payload(payload: &str) -> Result<HashMap<String, String>> {
    let trimmed = payload.trim();
    let body = if trimmed.starts_with(':') {
        match trimmed.find('+') {
            Some(i) => &trimmed[i + 1..],
            None => return Err(anyhow::anyhow!("malformed payload")),
        }
    } else {
        trimmed
    };
    let mut map = HashMap::new();
    for pair in body.split('+') {
        if pair.is_empty() {
            continue;
        }
        if let Some(i) = pair.find(':') {
            map.insert(pair[..i].trim().to_string(), pair[i + 1..].trim().to_string());
        } else {
            map.insert(pair.trim().to_string(), String::new());
        }
    }
    Ok(map)
}

fn extract_query_param(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    for pair in q.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == key {
            return decode(v).ok().map(|s| s.into_owned());
        }
    }
    None
}

fn as_bootstrapper_payload(arg: &str) -> Option<String> {
    if arg.starts_with(':') && arg.contains("+placelauncherurl:") {
        return Some(arg.to_string());
    }
    let schemes = ["watame://", "watame:"];
    if let Some(stripped) = strip_scheme(arg, &schemes) {
        if stripped.contains("+placelauncherurl:") {
            return Some(stripped.to_string());
        }
    }
    None
}

fn get_game_details(
    gameid: &str,
    from_bootstrapper: &Option<PlaceDetails>,
) -> (String, String, Option<String>) {
    if let Some(d) = from_bootstrapper {
        let builder = d
            .builder
            .clone()
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| "Unknown".to_string());
        return (d.name.clone(), builder, d.image_token.clone());
    }
    let url = format!(
        "{}/apisite/games/v1/games/multiget-place-details?placeIds={}",
        BASE_HOST, gameid
    );
    let client = reqwest::blocking::Client::builder()
        .user_agent("Mozilla/5.0")
        .build()
        .unwrap();
    if let Ok(resp) = client.get(&url).send() {
        if resp.status().is_success() {
            if let Ok(entries) = resp.json::<Vec<PlaceDetails>>() {
                if let Some(d) = entries.into_iter().next() {
                    let builder = d
                        .builder
                        .filter(|b| !b.is_empty())
                        .unwrap_or_else(|| "Unknown".to_string());
                    return (d.name, builder, d.image_token);
                }
            }
        }
    }
    (format!("Game ID: {}", gameid), "Unknown".to_string(), None)
}

fn thumbnail_url(gameid: &str, image_token: &Option<String>) -> String {
    match image_token {
        Some(t) if !t.is_empty() => format!(
            "{}/Thumbs/Asset.ashx?width=420&height=420&assetId={}&token={}",
            BASE_HOST,
            gameid,
            urlencoding::encode(t)
        ),
        _ => format!(
            "{}/Thumbs/Asset.ashx?width=420&height=420&assetId={}",
            BASE_HOST, gameid
        ),
    }
}

fn lclient_from_bootstrapper(
    payload: &str,
    ui_status: &Arc<Mutex<String>>,
) -> Result<(Child, String, Option<PlaceDetails>)> {
    let params = parse_bootstrapper_payload(payload)?;
    let ticket = params.get("gameinfo").cloned().context("missing gameinfo")?;
    let join_url = params
        .get("placelauncherurl")
        .cloned()
        .context("missing placelauncherurl")?;
    let place_id = extract_query_param(&join_url, "placeId")
        .or_else(|| extract_query_param(&join_url, "place"))
        .unwrap_or_else(|| "unknown".to_string());

    let place_details: Option<PlaceDetails> = params
        .get("placedetails")
        .and_then(|b64| b64url_decode(b64))
        .and_then(|bytes| serde_json::from_slice::<PlaceDetails>(&bytes).ok());

    let auth_url = format!("{}/Login/Negotiate.ashx", BASE_HOST);

    let ver = active_client_version()
        .ok_or_else(|| anyhow::anyhow!("client not installed"))?;
    let game_exe = client_exe_for(&ver)?;

    *ui_status.lock().unwrap() = "Waiting for system verification...".to_string();
    std::thread::sleep(Duration::from_millis(1500));

    if !game_exe.exists() {
        return Err(anyhow::anyhow!(
            "client missing at {}",
            game_exe.display()
        ));
    }

    let child = Command::new(&game_exe)
        .arg("--authenticationUrl")
        .arg(&auth_url)
        .arg("--authenticationTicket")
        .arg(&ticket)
        .arg("--joinScriptUrl")
        .arg(&join_url)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;

    Ok((child, place_id, place_details))
}

fn watch_game_child(child: Child) {
    unsafe {
        let handle = OpenProcess(SYNCHRONIZE, 0, child.id());
        if handle.is_null() {
            let mut c = child;
            let _ = c.wait();
            std::process::exit(0);
        }
        WaitForSingleObject(handle, INFINITE);
        CloseHandle(handle);
    }
    std::process::exit(0);
}

fn setup_discord_rpc(gameid: String, place_details: Option<PlaceDetails>) {
    std::thread::spawn(move || {
        let mut client = DiscordIpcClient::new(DSC_CLIENT_ID);
        if client.connect().is_err() {
            return;
        }
        let (game_name, creator_name, image_token) = get_game_details(&gameid, &place_details);
        let start_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let state_string = format!("By {}", creator_name);
        let game_url = format!("{}/games/{}", BASE_HOST, gameid);
        let image_url = thumbnail_url(&gameid, &image_token);

        let act = activity::Activity::new()
            .details(&game_name)
            .state(&state_string)
            .timestamps(activity::Timestamps::new().start(start_time))
            .assets(activity::Assets::new().small_image(&image_url))
            .buttons(vec![activity::Button::new("See game page", &game_url)]);

        let _ = client.set_activity(act);
        loop {
            std::thread::sleep(Duration::from_secs(10));
        }
    });
}

const LOGO_BYTES: &[u8] = include_bytes!("logo.png");

fn load_logo(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(LOGO_BYTES).ok()?;
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let ci = egui::ColorImage::from_rgba_unmultiplied(size, &rgba);
    Some(ctx.load_texture("logo", ci, egui::TextureOptions::LINEAR))
}

struct KaizenApp {
    status: Arc<Mutex<String>>,
    texture: Option<egui::TextureHandle>,
    payload: Option<String>,
    thread_started: bool,
    phase: f32,
    last_pointer: egui::Pos2,
}

impl KaizenApp {
    fn new(cc: &eframe::CreationContext<'_>, payload: Option<String>) -> Self {
        let mut fonts = egui::FontDefinitions::default();
        if let Ok(font_data) = fs::read("C:\\Windows\\Fonts\\segoeui.ttf") {
            fonts
                .font_data
                .insert("segoe".to_owned(), egui::FontData::from_owned(font_data));
            fonts
                .families
                .get_mut(&egui::FontFamily::Proportional)
                .unwrap()
                .insert(0, "segoe".to_owned());
        }
        cc.egui_ctx.set_fonts(fonts);

        Self {
            status: Arc::new(Mutex::new("Initializing...".to_string())),
            texture: load_logo(&cc.egui_ctx),
            payload,
            thread_started: false,
            phase: 0.0,
            last_pointer: egui::Pos2::ZERO,
        }
    }

    fn start_work(&mut self, ctx: &egui::Context) {
        let status = self.status.clone();
        let payload = self.payload.clone().unwrap_or_default();
        let ctx = ctx.clone();

        std::thread::spawn(move || {
            let passthrough_args: Vec<String> = env::args().skip(1).collect();
            let result = (|| -> Result<()> {
                fs::create_dir_all(root()?)?;
                fs::create_dir_all(versions_dir()?)?;
                migrate_legacy_install()?;

                let is_studio = env::args().any(|a| a == "--studio");

                if !payload.is_empty() {
                    *status.lock().unwrap() = "Checking for updates...".to_string();
                    ctx.request_repaint();
                    updatechecker(&status, &passthrough_args)?;
                    std::thread::sleep(Duration::from_millis(500));

                    *status.lock().unwrap() = "Launching Game...".to_string();
                    ctx.request_repaint();
                    let (child, place_id, place_details) =
                        lclient_from_bootstrapper(&payload, &status)?;
                    setup_discord_rpc(place_id, place_details);
                    std::thread::sleep(Duration::from_millis(1000));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                    watch_game_child(child);
                    return Ok(());
                }

                if is_studio {
                    *status.lock().unwrap() = "Launching Studio...".to_string();
                    ctx.request_repaint();
                    updatechecker(&status, &passthrough_args)?;
                    let ver = active_studio_version()
                        .ok_or_else(|| anyhow::anyhow!("studio not installed"))?;
                    Command::new(studio_exe_for(&ver)?).spawn()?;
                    std::process::exit(0);
                }

                let lp = launcher_exe_path()?;
                if !lp.exists() || env::current_exe()? != lp {
                    *status.lock().unwrap() = "Bootstrapping...".to_string();
                    ctx.request_repaint();
                    if lp.exists() {
                        let old = lp.with_extension("old");
                        let _ = fs::remove_file(&old);
                        let _ = fs::rename(&lp, &old);
                    }
                    let lastver = fetch_lastver().context("version info")?;
                    let url = lastver.launcher.lastDownload.clone();
                    filesdownloader(&cache_bust(&url), &lp)?;
                    addprotocol()?;
                    let mut cmd = Command::new(&lp);
                    for a in &passthrough_args {
                        cmd.arg(a);
                    }
                    cmd.spawn()?;
                    std::process::exit(0);
                }

                *status.lock().unwrap() = "Checking for updates...".to_string();
                ctx.request_repaint();
                updatechecker(&status, &passthrough_args)?;
                addprotocol()?;

                *status.lock().unwrap() = "Ready.".to_string();
                std::thread::sleep(Duration::from_secs(1));
                Ok(())
            })();

            if let Err(e) = result {
                log_line(&format!("FATAL: {}", e));
                *status.lock().unwrap() = format!("Error: {}", e);
                ctx.request_repaint();
                std::thread::sleep(Duration::from_secs(5));
            }

            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        });
    }
}

impl eframe::App for KaizenApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut visuals = egui::Visuals::dark();
        visuals.widgets.noninteractive.bg_fill = BG;
        visuals.panel_fill = BG;
        visuals.override_text_color = Some(FG);
        ctx.set_visuals(visuals);

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(BG))
            .show(ctx, |ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                    let close_btn = egui::Button::new(
                        egui::RichText::new("X").color(FG).size(16.0),
                    )
                    .fill(egui::Color32::TRANSPARENT);
                    if ui.add(close_btn).clicked() {
                        std::process::exit(0);
                    }
                });

                ui.vertical_centered(|ui| {
                    ui.add_space(20.0);

                    if let Some(tex) = &self.texture {
                        let aspect = tex.size()[1] as f32 / tex.size()[0] as f32;
                        ui.image((tex.id(), egui::vec2(100.0, 100.0 * aspect)));
                    }

                    ui.add_space(15.0);

                    ui.label(
                        egui::RichText::new("Kaizen Launcher")
                            .color(FG)
                            .size(24.0)
                            .strong(),
                    );

                    ui.add_space(5.0);

                    let status_text = self.status.lock().unwrap().clone();
                    ui.label(egui::RichText::new(status_text).color(FG).size(14.0));

                    ui.add_space(25.0);

                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(280.0, 6.0), egui::Sense::hover());

                    let pointer = ui.input(|i| i.pointer.latest_pos()).unwrap_or(self.last_pointer);
                    let moved = pointer != self.last_pointer;
                    self.last_pointer = pointer;

                    if moved {
                        self.phase = (self.phase + 0.02) % 2.0;
                        ctx.request_repaint();
                    }

                    let round = egui::Rounding::same(3.0);
                    ui.painter().rect_filled(rect, round, BG);

                    let bar_w = 100.0;
                    let t = self.phase;
                    let start_x = if t <= 1.0 {
                        rect.left() + (rect.width() - bar_w) * t
                    } else {
                        rect.left() + (rect.width() - bar_w) * (2.0 - t)
                    };
                    let fill_rect = egui::Rect::from_min_size(
                        egui::pos2(start_x, rect.top()),
                        egui::vec2(bar_w, rect.height()),
                    );
                    ui.painter().rect_filled(fill_rect, round, FG);
                });

                if !self.thread_started {
                    self.thread_started = true;
                    self.start_work(ctx);
                }
            });
    }
}

fn main() {
    log_init();
    let payload = env::args().skip(1).find_map(|a| as_bootstrapper_payload(&a));

    let win_w = 480.0;
    let win_h = 270.0;

    let pos = unsafe {
        use winapi::um::winuser::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
        let sw = GetSystemMetrics(SM_CXSCREEN) as f32;
        let sh = GetSystemMetrics(SM_CYSCREEN) as f32;
        egui::pos2((sw - win_w) / 2.0, (sh - win_h) / 2.0)
    };

    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([win_w, win_h])
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(false)
        .with_position(pos);

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let _ = eframe::run_native(
        "Kaizen Launcher",
        options,
        Box::new(|cc| Box::new(KaizenApp::new(cc, payload))),
    );
}