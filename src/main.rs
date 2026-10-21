#![windows_subsystem = "windows"]

// FORK OF EPIK17 LAUNCHER

use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Child};
use std::collections::HashMap;
use std::env;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH, Duration};
use zip::ZipArchive;
use winreg::enums::*;
use winreg::RegKey;
use urlencoding::decode;
use chrono::Utc;
use serde::Deserialize;
use discord_rich_presence::{activity, DiscordIpc, DiscordIpcClient};
use eframe::egui;

use winapi::um::handleapi::CloseHandle;
use winapi::um::synchapi::WaitForSingleObject;
use winapi::um::winbase::INFINITE;
use winapi::um::processthreadsapi::OpenProcess;
use winapi::um::winnt::SYNCHRONIZE;

use std::os::windows::process::CommandExt;

const VERSION: &str = "1.2.8";
const LASTVER_URL: &str = "https://gummy.live/UnsecuredContent/lastVer.json";
const DSC_CLIENT_ID: &str = "1553387026424795186";
const BASE_HOST: &str = "https://gummy.live";
const CREATE_NO_WINDOW: u32 = 0x08000000;


fn exe_dir() -> PathBuf {
    env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())).unwrap_or_else(|| PathBuf::from("."))
}

fn log_init() {
    let path = exe_dir().join("watame-launcher.log");
    let mut file = OpenOptions::new().create(true).append(true).open(&path).or_else(|_| File::create(&path)).ok();
    if let Some(f) = file.as_mut() {
        let _ = writeln!(f, "=== Kaizen Launcher v{} started ===", VERSION);
    }
}

fn log_line(msg: &str) {
    let ts = Utc::now().to_rfc3339();
    let path = exe_dir().join("watame-launcher.log");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{}] {}", ts, msg);
    }
}

fn b64url_decode(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'), b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52), b'-' => Some(62), b'_' => Some(63), _ => None,
        }
    }
    let mut out = Vec::new();
    let mut buf: u32 = 0;
    let mut bits: u8 = 0;
    for &c in input.as_bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' || c == b' ' || c == b'\t' { continue; }
        let v = val(c)?;
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 { bits -= 8; out.push(((buf >> bits) & 0xFF) as u8); }
    }
    Some(out)
}

#[allow(non_snake_case)]
#[derive(Deserialize, Debug)]
struct VersionInfo { lastVersion: String, lastDownload: String }

#[derive(Deserialize, Debug)]
struct LastVer { client: VersionInfo, studio: VersionInfo, launcher: VersionInfo }

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct PlaceDetails {
    #[serde(default)] name: String,
    #[serde(default)] builder: Option<String>,
    #[serde(default)] image_token: Option<String>,
}

fn appdata() -> Result<PathBuf> {
    let mut p: PathBuf = env::var_os("APPDATA").context("APPDATA not found")?.into();
    p.push("Watame"); Ok(p)
}
fn cdir() -> Result<PathBuf> { let mut p = appdata()?; p.push("Client"); Ok(p) }
fn sdir() -> Result<PathBuf> { let mut p = appdata()?; p.push("Studio"); Ok(p) }
fn cexe() -> Result<PathBuf> { let mut p = cdir()?; p.push("RobloxPlayerBeta.exe"); Ok(p) }
fn sexe() -> Result<PathBuf> { let mut p = sdir()?; p.push("RobloxStudioBeta.exe"); Ok(p) }
fn cverfile() -> Result<PathBuf> { let mut p = cdir()?; p.push("client_version.txt"); Ok(p) }
fn sverfile() -> Result<PathBuf> { let mut p = sdir()?; p.push("studio_version.txt"); Ok(p) }
fn eepikexe() -> Result<PathBuf> { cexe() }
fn lpath() -> Result<PathBuf> { let mut p = appdata()?; p.push("WatameLauncherBeta.exe"); Ok(p) }

fn installed_client_version() -> Option<String> { fs::read_to_string(cverfile().ok()?).ok().map(|s| s.trim().to_string()) }
fn installed_studio_version() -> Option<String> { fs::read_to_string(sverfile().ok()?).ok().map(|s| s.trim().to_string()) }
fn write_client_version(v: &str) { if let Ok(p) = cverfile() { let _ = fs::write(p, v); } }
fn write_studio_version(v: &str) { if let Ok(p) = sverfile() { let _ = fs::write(p, v); } }

fn cachelol(url: &str) -> String {
    let ts = Utc::now().timestamp();
    if url.contains('?') { format!("{}&_={}", url, ts) } else { format!("{}?_={}", url, ts) }
}

// ---------------------------------------------------------------------------
// Downloading & Updating
// ---------------------------------------------------------------------------

fn filesdownloader(url: &str, path: &PathBuf) -> Result<()> {
    let client = reqwest::blocking::Client::builder().no_gzip().no_brotli().no_deflate().build()?;
    let mut attempts = 0;
    loop {
        attempts += 1;
        match do_download(&client, url, path) {
            Ok(_) => break Ok(()),
            Err(e) => {
                if attempts >= 3 { return Err(e).context("Download failed after 3 attempts"); }
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
        let mut file = archive.by_index(i)?;
        let outpath = match file.enclosed_name() { Some(path) => out.join(path), None => continue };
        if file.name().ends_with('/') {
            fs::create_dir_all(&outpath)?;
        } else {
            if let Some(p) = outpath.parent() { if !p.exists() { fs::create_dir_all(p)?; } }
            let mut outfile = fs::File::create(&outpath)?;
            std::io::copy(&mut file, &mut outfile)?;
        }
    }
    Ok(())
}

fn fetch_lastver() -> Option<LastVer> {
    let url = cachelol(LASTVER_URL);
    let resp = reqwest::blocking::get(&url).ok()?;
    if !resp.status().is_success() { return None; }
    resp.json::<LastVer>().ok()
}

fn updatechecker(ui_status: &Arc<Mutex<String>>) -> Result<()> {
    let lastver = fetch_lastver();
    let lp = lpath()?;

    let need_launcher = match lastver.as_ref() {
        Some(lv) => lv.launcher.lastVersion.trim() != VERSION || !lp.exists(),
        None => !lp.exists(),
    };

    if need_launcher {
        if lp.exists() {
            let old_lp = lp.with_extension("old");
            let _ = fs::remove_file(&old_lp);
            let _ = fs::rename(&lp, &old_lp);
        }
        let launcher_url = match lastver.as_ref() { Some(lv) => lv.launcher.lastDownload.clone(), None => return Err(anyhow::anyhow!("No launcher URL")) };
        *ui_status.lock().unwrap() = "Downloading Launcher Update...".to_string();
        filesdownloader(&cachelol(&launcher_url), &lp)?;
        Command::new(&lp).spawn()?;
        std::process::exit(0);
    }

    let installed_c = installed_client_version();
    let need_client = match lastver.as_ref() {
        Some(lv) => installed_c.as_deref() != Some(lv.client.lastVersion.trim()) || !cexe()?.exists(),
        None => !cexe()?.exists(),
    };

    if need_client {
        let client_url = match lastver.as_ref() { Some(lv) => lv.client.lastDownload.clone(), None => return Err(anyhow::anyhow!("No client URL")) };
        let client_version = match lastver.as_ref() { Some(lv) => lv.client.lastVersion.clone(), None => String::new() };
        
        *ui_status.lock().unwrap() = "Downloading Kaizen Player...".to_string();
        let zp = appdata()?.join("WatamePlayerBeta.zip");
        filesdownloader(&cachelol(&client_url), &zp)?;
        
        *ui_status.lock().unwrap() = "Extracting Client...".to_string();
        unzip(&zp, &cdir()?)?;
        let _ = fs::remove_file(&zp);
        
        if !client_version.is_empty() { write_client_version(client_version.trim()); }
        delete_protocol("Watame"); delete_protocol("watame"); addprotocol()?;
    }

    let installed_s = installed_studio_version();
    let need_studio = match lastver.as_ref() {
        Some(lv) => installed_s.as_deref() != Some(lv.studio.lastVersion.trim()) || !sexe()?.exists(),
        None => !sexe()?.exists(),
    };

    if need_studio {
        let studio_url = match lastver.as_ref() { Some(lv) => lv.studio.lastDownload.clone(), None => return Err(anyhow::anyhow!("No studio URL")) };
        let studio_version = match lastver.as_ref() { Some(lv) => lv.studio.lastVersion.clone(), None => String::new() };
        
        *ui_status.lock().unwrap() = "Downloading Kaizen Studio...".to_string();
        let zp = appdata()?.join("WatameStudioBeta.zip");
        filesdownloader(&cachelol(&studio_url), &zp)?;
        
        *ui_status.lock().unwrap() = "Extracting Studio...".to_string();
        unzip(&zp, &sdir()?)?;
        let _ = fs::remove_file(&zp);
        
        if !studio_version.is_empty() { write_studio_version(studio_version.trim()); }
    }

    create_desktop_shortcut(&lpath()?, None, "Kaizen Launcher")?;
    create_desktop_shortcut(&lpath()?, Some("--studio"), "Kaizen Studio")?;
    let old_lp = lp.with_extension("old");
    if old_lp.exists() { let _ = fs::remove_file(&old_lp); }
    Ok(())
}

// ---------------------------------------------------------------------------
// Protocols & Bootstrapper
// ---------------------------------------------------------------------------

fn delete_protocol(name: &str) {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let _ = hkcu.delete_subkey_all(format!(r"Software\Classes\{}", name));
}

fn register_protocol(scheme: &str, description: &str) -> Result<()> {
    let lp = lpath()?;
    let exe = lp.to_str().context("Invalid launcher path")?.to_string();
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

fn create_desktop_shortcut(target_exe: &PathBuf, args: Option<&str>, shortcut_name: &str) -> Result<()> {
    if let Ok(user_profile) = env::var("USERPROFILE") {
        let mut shortcut_path = PathBuf::from(user_profile);
        shortcut_path.push("Desktop");
        shortcut_path.push(format!("{}.lnk", shortcut_name));
        if !shortcut_path.exists() {
            let target_str = target_exe.to_str().unwrap_or("");
            let shortcut_str = shortcut_path.to_str().unwrap_or("");
            let args_str = args.unwrap_or("");
            let ps_script = format!(
                "$wshell = New-Object -ComObject WScript.Shell; \
                 $shortcut = $wshell.CreateShortcut('{}'); \
                 $shortcut.TargetPath = '{}'; \
                 $shortcut.Arguments = '{}'; \
                 $shortcut.Save()",
                shortcut_str, target_str, args_str
            );
            let _ = Command::new("powershell").arg("-NoProfile").arg("-WindowStyle").arg("Hidden").arg("-Command").arg(&ps_script).output()?;
        }
    }
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
        match trimmed.find('+') { Some(i) => &trimmed[i + 1..], None => return Err(anyhow::anyhow!("Malformed")) }
    } else { trimmed };
    let mut map = HashMap::new();
    for pair in body.split('+') {
        if pair.is_empty() { continue; }
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
        if k == key { return decode(v).ok().map(|s| s.into_owned()); }
    }
    None
}

fn as_bootstrapper_payload(arg: &str) -> Option<String> {
    if arg.starts_with(':') && arg.contains("+placelauncherurl:") { return Some(arg.to_string()); }
    let schemes = ["watame://", "watame:"];
    if let Some(stripped) = strip_scheme(arg, &schemes) {
        if stripped.contains("+placelauncherurl:") { return Some(stripped.to_string()); }
    }
    None
}

fn get_game_details(gameid: &str, from_bootstrapper: &Option<PlaceDetails>) -> (String, String, Option<String>) {
    if let Some(d) = from_bootstrapper {
        let builder = d.builder.clone().filter(|b| !b.is_empty()).unwrap_or_else(|| "Unknown".to_string());
        return (d.name.clone(), builder, d.image_token.clone());
    }
    let url = format!("{}/apisite/games/v1/games/multiget-place-details?placeIds={}", BASE_HOST, gameid);
    let client = reqwest::blocking::Client::builder().user_agent("Mozilla/5.0").build().unwrap();
    if let Ok(resp) = client.get(&url).send() {
        if resp.status().is_success() {
            if let Ok(entries) = resp.json::<Vec<PlaceDetails>>() {
                if let Some(d) = entries.into_iter().next() {
                    let builder = d.builder.filter(|b| !b.is_empty()).unwrap_or_else(|| "Unknown".to_string());
                    return (d.name, builder, d.image_token);
                }
            }
        }
    }
    (format!("Game ID: {}", gameid), "Unknown".to_string(), None)
}

fn thumbnail_url(gameid: &str, image_token: &Option<String>) -> String {
    match image_token {
        Some(t) if !t.is_empty() => format!("{}/Thumbs/Asset.ashx?width=420&height=420&assetId={}&token={}", BASE_HOST, gameid, urlencoding::encode(t)),
        _ => format!("{}/Thumbs/Asset.ashx?width=420&height=420&assetId={}", BASE_HOST, gameid),
    }
}

fn kill_existing_client() {
    let _ = Command::new("taskkill").args(["/F", "/IM", "RobloxPlayerBeta.exe"]).output();
}

fn lclient_from_bootstrapper(payload: &str, ui_status: &Arc<Mutex<String>>) -> Result<(Child, String, Option<PlaceDetails>)> {
    let params = parse_bootstrapper_payload(payload)?;
    let ticket = params.get("gameinfo").cloned().context("Missing gameinfo")?;
    let join_url = params.get("placelauncherurl").cloned().context("Missing placelauncherurl")?;
    let place_id = extract_query_param(&join_url, "placeId").or_else(|| extract_query_param(&join_url, "place")).unwrap_or_else(|| "unknown".to_string());

    let place_details: Option<PlaceDetails> = params.get("placedetails")
        .and_then(|b64| b64url_decode(b64))
        .and_then(|bytes| serde_json::from_slice::<PlaceDetails>(&bytes).ok());

    let auth_url = format!("{}/Login/Negotiate.ashx", BASE_HOST);
    let game_exe = eepikexe()?;
    
    *ui_status.lock().unwrap() = "Waiting for system verification...".to_string();
    std::thread::sleep(Duration::from_millis(1500));

    if !game_exe.exists() { 
        return Err(anyhow::anyhow!("Client missing at {}. Antivirus might have blocked it.", game_exe.display())); 
    }

    let mut cmd = Command::new(&game_exe);
    cmd.arg("--authenticationUrl").arg(&auth_url)
       .arg("--authenticationTicket").arg(&ticket)
       .arg("--joinScriptUrl").arg(&join_url)
       .creation_flags(CREATE_NO_WINDOW);

    let child = cmd.spawn()?;
    Ok((child, place_id, place_details))
}

fn watch_game_child(child: Child) {
    unsafe {
        let process_handle = OpenProcess(SYNCHRONIZE, 0, child.id());
        if process_handle.is_null() {
            let mut game_arc = child;
            let _ = game_arc.wait();
            std::process::exit(0);
        }
        WaitForSingleObject(process_handle, INFINITE);
        CloseHandle(process_handle);
    }
    std::process::exit(0);
}

fn setup_discord_rpc(gameid: String, place_details: Option<PlaceDetails>) {
    std::thread::spawn(move || {
        let mut client = DiscordIpcClient::new(DSC_CLIENT_ID);
        if client.connect().is_err() { return; }
        let (game_name, creator_name, image_token) = get_game_details(&gameid, &place_details);
        let start_time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
        let state_string = format!("By {}", creator_name);
        let game_url = format!("{}/games/{}", BASE_HOST, gameid);
        let image_url = thumbnail_url(&gameid, &image_token);

        let activity = activity::Activity::new()
            .details(&game_name)
            .state(&state_string)
            .timestamps(activity::Timestamps::new().start(start_time))
            .assets(activity::Assets::new().small_image(&image_url))
            .buttons(vec![activity::Button::new("See game page", &game_url)]);

        let _ = client.set_activity(activity);
        loop { std::thread::sleep(Duration::from_secs(10)); }
    });
}


struct KaizenApp {
    status: Arc<Mutex<String>>,
    texture: Option<egui::TextureHandle>,
    payload: Option<String>,
    thread_started: bool,
}

impl KaizenApp {
    fn new(cc: &eframe::CreationContext<'_>, payload: Option<String>) -> Self {
        let mut fonts = egui::FontDefinitions::default();
        if let Ok(font_data) = std::fs::read("C:\\Windows\\Fonts\\segoeui.ttf") {
            fonts.font_data.insert("segoe".to_owned(), egui::FontData::from_owned(font_data));
            fonts.families.get_mut(&egui::FontFamily::Proportional).unwrap().insert(0, "segoe".to_owned());
        }
        cc.egui_ctx.set_fonts(fonts);

        let mut texture = None;
        for name in &["logo.png", "icon.png"] {
            let path = exe_dir().join(name);
            if let Ok(img_bytes) = fs::read(&path) {
                if let Ok(img) = image::load_from_memory(&img_bytes) {
                    let rgba = img.to_rgba8();
                    let size = [rgba.width() as usize, rgba.height() as usize];
                    let color_image = egui::ColorImage::from_rgba_unmultiplied(size, &rgba);
                    texture = Some(cc.egui_ctx.load_texture("logo", color_image, egui::TextureOptions::LINEAR));
                    break;
                }
            }
        }

        Self {
            status: Arc::new(Mutex::new("Initializing...".to_string())),
            texture,
            payload,
            thread_started: false,
        }
    }

    fn start_work(&mut self, ctx: &egui::Context) {
        let status = self.status.clone();
        let payload = self.payload.clone().unwrap_or_default();
        let ctx = ctx.clone();

        std::thread::spawn(move || {
            let result = (|| -> Result<()> {
                fs::create_dir_all(appdata()?)?;
                fs::create_dir_all(cdir()?)?;

                let is_studio = env::args().any(|a| a == "--studio");

                if !payload.is_empty() {
                    *status.lock().unwrap() = "Checking for updates...".to_string();
                    ctx.request_repaint();
                    
                    updatechecker(&status)?;
                    kill_existing_client();
                    std::thread::sleep(Duration::from_millis(500));

                    *status.lock().unwrap() = "Launching Game...".to_string();
                    ctx.request_repaint();
                    
                    let (child, place_id, place_details) = lclient_from_bootstrapper(&payload, &status)?;
                    setup_discord_rpc(place_id, place_details);

                    std::thread::sleep(Duration::from_millis(1000));
                    
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                    
                    watch_game_child(child);
                    return Ok(());
                }

                if is_studio {
                    *status.lock().unwrap() = "Launching Studio...".to_string();
                    ctx.request_repaint();
                    updatechecker(&status)?;
                    let exe = sexe()?;
                    Command::new(&exe).spawn()?;
                    std::process::exit(0);
                }

                let lp = lpath()?;
                if !lp.exists() || env::current_exe()? != lp {
                    *status.lock().unwrap() = "Bootstrapping...".to_string();
                    ctx.request_repaint();
                    if lp.exists() {
                        let old_lp = lp.with_extension("old");
                        let _ = fs::remove_file(&old_lp);
                        let _ = fs::rename(&lp, &old_lp);
                    }
                    let lastver = fetch_lastver().context("Version Info Error")?;
                    let launcher_url = lastver.launcher.lastDownload.clone();
                    filesdownloader(&cachelol(&launcher_url), &lp)?;
                    addprotocol()?;
                    Command::new(lp).spawn()?;
                    std::process::exit(0);
                }

                *status.lock().unwrap() = "Checking for updates...".to_string();
                ctx.request_repaint();
                updatechecker(&status)?;
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
        visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(24, 24, 27);
        visuals.panel_fill = egui::Color32::from_rgb(24, 24, 27);
        ctx.set_visuals(visuals);

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::from_rgb(24, 24, 27)))
            .show(ctx, |ui| {
                
                ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                    let close_btn = egui::Button::new(egui::RichText::new("✖").color(egui::Color32::from_rgb(150, 150, 150)).size(16.0))
                        .fill(egui::Color32::TRANSPARENT);
                    if ui.add(close_btn).clicked() {
                        std::process::exit(0);
                    }
                });

                ui.vertical_centered(|ui| {
                    ui.add_space(20.0);

                    if let Some(texture) = &self.texture {
                        let aspect = texture.size()[1] as f32 / texture.size()[0] as f32;
                        ui.image((texture.id(), egui::vec2(100.0, 100.0 * aspect)));
                    }

                    ui.add_space(15.0);

                    ui.label(egui::RichText::new("Kaizen Launcher").color(egui::Color32::WHITE).size(24.0).strong());
                    
                    ui.add_space(5.0);

                    let status_text = self.status.lock().unwrap().clone();
                    ui.label(egui::RichText::new(status_text).color(egui::Color32::from_rgb(160, 160, 160)).size(14.0));

                    ui.add_space(25.0);

                    let (rect, _) = ui.allocate_exact_size(egui::vec2(280.0, 4.0), egui::Sense::hover());
                    
                    ui.painter().rect_filled(rect, 0.0, egui::Color32::from_rgb(40, 40, 45));
                    
                    let time = ui.input(|i| i.time);
                    let progress = (time * 1.5) % 2.0;
                    
                    let bar_width = 100.0;
                    let start_x = if progress <= 1.0 {
                        rect.left() + (rect.width() - bar_width) * progress as f32
                    } else {
                        rect.left() + (rect.width() - bar_width) * (2.0 - progress) as f32
                    };
                    
                    let fill_rect = egui::Rect::from_min_size(egui::pos2(start_x, rect.top()), egui::vec2(bar_width, rect.height()));
                    
                    ui.painter().rect_filled(fill_rect, 0.0, egui::Color32::from_rgb(139, 92, 246));
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
    let mut centered_pos = None;
    
    unsafe {
        use winapi::um::winuser::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
        let screen_w = GetSystemMetrics(SM_CXSCREEN) as f32;
        let screen_h = GetSystemMetrics(SM_CYSCREEN) as f32;
        
        let x = (screen_w - win_w) / 2.0;
        let y = (screen_h - win_h) / 2.0;
        centered_pos = Some(egui::pos2(x, y));
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([win_w, win_h])
        .with_decorations(false)
        .with_transparent(true)  
        .with_resizable(false);
        
    if let Some(pos) = centered_pos {
        viewport = viewport.with_position(pos);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Kaizen Launcher",
        options,
        Box::new(|cc| Box::new(KaizenApp::new(cc, payload))),
    ).unwrap();
}