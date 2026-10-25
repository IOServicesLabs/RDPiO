//! Windows 365 sign-in and the user's Cloud PCs, for the app.
//!
//! Sign-in is the desktop's (`w365`): Microsoft's sign-in page in the app's
//! WebView, the authorization code it ends with exchanged for tokens. The app
//! keeps the refresh token (encrypted by the Android Keystore) and hands it
//! back at start, so a sign-in lasts. The Cloud PCs come from the Azure
//! Virtual Desktop web feed: each resource's signed `.rdp` file (the same file
//! the desktop reads from the Windows App's cache) drives the connection.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::w365::{self, AccessToken};

/// The web feed: Remote Desktop's RADC workspace feed for the signed-in user.
const WEBFEED: &str = "https://rdweb.wvd.microsoft.com/api/arm/feeddiscovery/webfeed.aspx";
/// Feed discovery: answers with (or redirects to) the account's feed.
const DISCOVERY: &str = "https://rdweb.wvd.microsoft.com/api/arm/feeddiscovery";
const TENANT: &str = "common";

struct Account {
    token: AccessToken,
    got: Instant,
}

static ACCOUNT: Mutex<Option<Account>> = Mutex::new(None);
/// The last list: (name, .rdp file, key: an imported one's, else "").
static CLOUD_PCS: Mutex<Vec<(String, String, String)>> = Mutex::new(Vec::new());
/// Cloud PCs imported from `.rdp` files (the app keeps the files and hands
/// them back at start): (key, name, .rdp). Microsoft's feed answers only its
/// own apps now ("INCOMPATIBLE_CLIENT_VERSION"), so these stand in for it.
static IMPORTED: Mutex<Vec<(String, String, String)>> = Mutex::new(Vec::new());

/// Add a Cloud PC from a file's bytes: a `.rdp` (UTF-8 or UTF-16) or one of
/// the Windows App's cache files. Returns (key, name); the same Cloud PC
/// imported again replaces the old one (same key).
pub fn import(bytes: &[u8]) -> Result<(String, String), String> {
    let e = crate::w365::cached_cloud_pc(&text(bytes)).ok_or("not a Windows 365 / Azure Virtual Desktop (ARM) .rdp file")?;
    let lbi = e.load_balance_info.unwrap_or_default();
    // FNV-1a of the load-balance info: the same Cloud PC, the same key.
    let key = format!("{:016x}", lbi.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100_0000_01b3)));
    let rdp = e.rdp_file.unwrap_or_default();
    let mut imported = IMPORTED.lock().unwrap();
    imported.retain(|i| i.0 != key);
    imported.push((key.clone(), e.display_name.clone(), rdp));
    imported.sort_by(|a, b| a.1.cmp(&b.1));
    log::info!("Windows 365: imported {}", e.display_name);
    Ok((key, e.display_name))
}

pub fn forget(key: &str) {
    IMPORTED.lock().unwrap().retain(|i| i.0 != key);
}

/// `name`, told apart from imported Cloud PCs of the same name (the same plan
/// gets the same name) by the start of its resource id: "… · 822a".
fn shown_name(imported: &[(String, String, String)], name: &str, rdp: &str) -> String {
    if imported.iter().filter(|i| i.1 == name).count() < 2 {
        return name.to_string();
    }
    let id = rdp
        .lines()
        .find_map(|l| l.trim().strip_prefix("loadbalanceinfo:s:"))
        .and_then(|lbi| lbi.trim().rsplit('/').next())
        .unwrap_or_default();
    format!("{name} · {}", id.chars().take(4).collect::<String>())
}

pub fn authorize_url() -> String {
    w365::build_authorize_url(TENANT, None, None)
}

pub fn redirect_prefix() -> &'static str {
    w365::NATIVE_REDIRECT_URI
}

/// Finish the sign-in with the code from the redirect. Returns the account
/// name and the refresh token (for the app to keep).
pub fn sign_in(code: &str) -> Result<(String, String), String> {
    let token = w365::exchange_auth_code(TENANT, None, None, code).map_err(|e| e.to_string())?;
    keep(token)
}

/// Sign in again with a kept refresh token.
pub fn restore(refresh: &str) -> Result<(String, String), String> {
    let token = w365::refresh_token(TENANT, None, refresh).map_err(|e| e.to_string())?;
    keep(token)
}

fn keep(token: AccessToken) -> Result<(String, String), String> {
    let user = token.username.clone().unwrap_or_default();
    let refresh = token.refresh_token.clone().unwrap_or_default();
    log::info!("Windows 365: signed in as {user}");
    *ACCOUNT.lock().unwrap() = Some(Account { token, got: Instant::now() });
    Ok((user, refresh))
}

pub fn sign_out() {
    *ACCOUNT.lock().unwrap() = None;
    CLOUD_PCS.lock().unwrap().clear();
    IMPORTED.lock().unwrap().clear();
}

/// A current access token (refreshed when it's near its end, or with `fresh`).
fn token(fresh: bool) -> Result<AccessToken, String> {
    let mut a = ACCOUNT.lock().unwrap();
    let acc = a.as_mut().ok_or("not signed in")?;
    if fresh || acc.got.elapsed() + Duration::from_secs(300) > acc.token.expires_in {
        if let Some(r) = acc.token.refresh_token.clone() {
            let mut t = w365::refresh_token(TENANT, None, &r).map_err(|e| e.to_string())?;
            if t.username.is_none() {
                t.username = acc.token.username.clone();
            }
            if t.refresh_token.is_none() {
                t.refresh_token = Some(r);
            }
            *acc = Account { token: t, got: Instant::now() };
        }
    }
    Ok(acc.token.clone())
}

pub fn user() -> Option<String> {
    ACCOUNT.lock().unwrap().as_ref().and_then(|a| a.token.username.clone())
}

/// A failed GET: the HTTP status (0 = no answer) and what to show.
struct GetError {
    status: u16,
    message: String,
}

fn get(url: &str, token: &str) -> Result<String, GetError> {
    let resp = match ureq::get(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/x-msts-radc+xml, application/xml, application/json, text/*")
        .timeout(Duration::from_secs(20))
        .call()
    {
        Ok(r) => r,
        // The service says why in the body: show and log that, not just the code.
        Err(ureq::Error::Status(status, r)) => {
            let body = r.into_string().unwrap_or_default();
            let body: String = body.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect();
            log::warn!("{url}: HTTP {status}: {body}");
            return Err(GetError { status, message: format!("{url}: HTTP {status} {body}") });
        }
        Err(e) => return Err(GetError { status: 0, message: format!("{url}: {e}") }),
    };
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut resp.into_reader(), &mut bytes).map_err(|e| GetError { status: 0, message: e.to_string() })?;
    Ok(text(&bytes))
}

/// The feed, with a fresh token once if the service refuses the request (a
/// kept token can be stale or minted for another resource), then through
/// feed discovery (which redirects to the account's feed).
fn feed() -> Result<(String, AccessToken), String> {
    let mut t = token(false)?;
    let mut last = String::new();
    for (attempt, url) in [WEBFEED, WEBFEED, DISCOVERY].into_iter().enumerate() {
        if attempt > 0 {
            t = token(true)?;
        }
        match get(url, &t.token) {
            Ok(body) => return Ok((body, t)),
            Err(e) if matches!(e.status, 400 | 401 | 403 | 404) => last = e.message,
            Err(e) => return Err(e.message),
        }
    }
    Err(last)
}

/// UTF-8, or UTF-16 (with its byte-order mark: how `.rdp` files often come).
fn text(bytes: &[u8]) -> String {
    match bytes {
        [0xFF, 0xFE, rest @ ..] => String::from_utf16_lossy(&rest.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()),
        [0xFE, 0xFF, rest @ ..] => String::from_utf16_lossy(&rest.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect::<Vec<_>>()),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// `name="value"` in an XML start tag.
fn attr(tag: &str, name: &str) -> Option<String> {
    let at = tag.find(&format!(" {name}=\""))? + name.len() + 3;
    let end = tag[at..].find('"')? + at;
    Some(tag[at..end].replace("&amp;", "&").replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">"))
}

/// The RADC feed's resources: (title, `.rdp` file URL).
fn resources(xml: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for part in xml.split("<Resource ").skip(1) {
        let tag = &part[..part.find('>').unwrap_or(part.len())];
        let title = attr(&format!(" {tag}"), "Title").unwrap_or_else(|| "Cloud PC".into());
        let rdp = part.split("<ResourceFile ").skip(1).find_map(|f| {
            let t = format!(" {}", &f[..f.find('>').unwrap_or(f.len())]);
            let ext = attr(&t, "FileExtension").unwrap_or_default();
            (ext.eq_ignore_ascii_case(".rdp")).then(|| attr(&t, "URL")).flatten()
        });
        if let Some(url) = rdp {
            out.push((title, url));
        }
    }
    out
}

/// The user's Cloud PCs: (name, key — "" unless imported). The imported ones
/// when there are any, else the feed's. Each one's `.rdp` is kept for [`config`].
pub fn list() -> Result<Vec<(String, String)>, String> {
    let imported = IMPORTED.lock().unwrap().clone();
    if !imported.is_empty() {
        let out = imported.iter().map(|i| (shown_name(&imported, &i.1, &i.2), i.0.clone())).collect();
        *CLOUD_PCS.lock().unwrap() = imported.into_iter().map(|(k, n, r)| (n, r, k)).collect();
        return Ok(out);
    }
    let (body, t) = feed().map_err(|e| {
        if e.contains("INCOMPATIBLE_CLIENT_VERSION") {
            "Microsoft's Cloud PC list only answers Microsoft's own apps. Import the Cloud PC's .rdp file instead.".to_string()
        } else {
            e
        }
    })?;
    log::info!("Windows 365 feed: {} bytes: {}", body.len(), body.chars().take(600).collect::<String>());
    let mut pcs = Vec::new();
    for (name, url) in resources(&body) {
        match get(&url, &t.token) {
            Ok(rdp) => pcs.push((name, rdp, String::new())),
            Err(e) => log::warn!("Windows 365: {name}: no .rdp file: {}", e.message),
        }
    }
    if pcs.is_empty() {
        // The desktop's feed reader (the JSON form, and .rdp files inline).
        if let Ok(entries) = crate::feed::parse(&body) {
            pcs.extend(entries.into_iter().filter_map(|e| Some((e.display_name, e.rdp_file?, String::new()))));
        }
    }
    if pcs.is_empty() {
        return Err("no Cloud PCs in the feed for this account".into());
    }
    let names = pcs.iter().map(|p| (p.0.clone(), String::new())).collect();
    *CLOUD_PCS.lock().unwrap() = pcs;
    Ok(names)
}

/// The connection to Cloud PC `index` (of the last [`list`]), signing in to
/// it as the signed-in user with their `password` (Windows 365 logs on with
/// the real password, encrypted for the Cloud PC alone: RDSTLS).
pub fn config(index: usize, password: &str, width: u32, height: u32) -> Result<rdp_core::ClientConfig, String> {
    let rdp = CLOUD_PCS.lock().unwrap().get(index).map(|p| p.1.clone()).ok_or("no such Cloud PC (list them again)")?;
    let t = token(false)?;
    let mut config = rdp_core::ClientConfig::default();
    crate::feed::apply_rdp_file(&mut config, &rdp);
    let rc = config.reverse_connect.as_mut().ok_or("this Cloud PC's .rdp isn't a Windows 365 / AVD (ARM) one")?;
    rc.access_token = t.token.clone();
    rc.rdstls_password = password.to_string();
    rc.rdstls_domain = "AzureAD".into();
    config.credentials.username = t.username.clone().unwrap_or_default();
    config.credentials.password = password.to_string();
    config.credentials.domain = "AzureAD".into();
    config.width = (width.clamp(640, 8192) & !1) as u16;
    config.height = (height.clamp(480, 8192) & !1) as u16;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_rdp_files_from_a_radc_feed() {
        let xml = r#"<ResourceCollection><Publisher Name="W"><Resources>
            <Resource ID="1" Alias="a" Title="My Cloud PC" Type="Desktop">
              <ResourceFiles><ResourceFile FileExtension=".rdp" URL="https://x/rdpfile?a=1&amp;b=2" /></ResourceFiles>
            </Resource></Resources></Publisher></ResourceCollection>"#;
        assert_eq!(resources(xml), vec![("My Cloud PC".to_string(), "https://x/rdpfile?a=1&b=2".to_string())]);
    }

    #[test]
    fn decodes_utf16_rdp_files() {
        let bytes: Vec<u8> = [0xFF, 0xFE].into_iter().chain("full address:s:x".encode_utf16().flat_map(u16::to_le_bytes)).collect();
        assert_eq!(text(&bytes), "full address:s:x");
    }

    #[test]
    fn tells_cloud_pcs_of_the_same_name_apart() {
        let pc = |key: &str, name: &str, id: &str| {
            (key.to_string(), name.to_string(), format!("full address:s:x\r\nloadbalanceinfo:s:mth://localhost/aa/{id}\r\n"))
        };
        let imported = [pc("1", "Plan", "822a5819-09ed"), pc("2", "Plan", "52a6451f-2b04"), pc("3", "Other", "4f41f804")];
        let shown: Vec<_> = imported.iter().map(|i| shown_name(&imported, &i.1, &i.2)).collect();
        assert_eq!(shown, ["Plan · 822a", "Plan · 52a6", "Other"]);
    }
}
