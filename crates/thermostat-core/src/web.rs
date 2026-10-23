//! The device web UI and HTTP API.
//!
//! | route              | auth  | purpose                                   |
//! |--------------------|-------|-------------------------------------------|
//! | `GET  /`           | yes   | status page                               |
//! | `GET/POST /setup`  | –     | choose the admin password (first boot only) |
//! | `GET/POST /login`  | –     | session login                             |
//! | `POST /logout`     | yes   |                                           |
//! | `GET/POST /settings` | yes | WiFi, thermostat and password settings    |
//! | `GET  /update`     | yes   | firmware upload page                      |
//! | `POST /api/update` | yes   | raw image body; `?reboot=0` to stay up    |
//! | `POST /api/reboot` | yes   |                                           |
//! | `GET  /api/status` | –     | JSON status                               |
//! | `GET  /ble`        | yes   | nearby BLE advertisers (auto-refreshing)  |
//! | `GET  /api/zone`   | –     | this zone and its heater group (JSON)     |
//! | `POST /api/zone/event` | group key | a peer zone's state; replies with ours |
//! | `POST /api/test/sensor` | yes | emulator builds: inject a sensor reading |
//! | `GET  /api/ble`    | yes   | the same as JSON                          |
//!
//! "auth" accepts a session cookie or HTTP Basic credentials (for scripts).
//! Session cookies are `HttpOnly; SameSite=Strict`, which also stops other
//! sites from submitting forms on the user's behalf.

use core::fmt::Write as _;

use embedded_io_async::{Read, Write};
use heapless::String;

use crate::auth::{DEFAULT_ITERATIONS, PasswordHash, TOKEN_LEN, wpa_psk};
use crate::codec::{base64, hex};
use crate::http::{self, BufWriter, Escape, HttpError, Method, Request, Tenths};
use crate::platform::Platform;
use crate::settings::{Settings, Str32};

/// Scratch space a connection needs (request head + response page).
pub const BUF_LEN: usize = http::MAX_HEAD + PAGE_LEN;
// The worst case (dual board, eight sensors, 16-byte names made entirely of
// HTML-escaped characters) renders the settings page in about 9.5 KiB.
const PAGE_LEN: usize = 10 * 1024;
const MAX_FORM: usize = 1024;

enum Auth {
    None,
    Session,
    Basic,
}

/// Serve one request on `sock`. `buf` must be at least [`BUF_LEN`] bytes.
pub async fn handle<S: Read + Write, P: Platform>(
    sock: &mut S,
    p: &mut P,
    buf: &mut [u8],
) -> Result<(), HttpError> {
    let (head_buf, page_buf) = buf.split_at_mut(http::MAX_HEAD);
    let (head_len, read) = http::read_head(sock, head_buf).await?;
    let (head, body_start) = head_buf.split_at_mut(head_len);
    let body_prefix = &body_start[..read - head_len];
    let req = match http::parse(head) {
        Ok(r) => r,
        Err(_) => {
            return respond(sock, page_buf, 400, "text/plain", &[], |w| {
                w.write_str("bad request")
            })
            .await;
        }
    };
    // One copy of the settings per request, shared by everything below (each
    // copy is several hundred bytes of stack and of this task's state).
    let mut s = p.settings();
    let auth = authenticate(&req, p, &s);
    let authed = !matches!(auth, Auth::None);
    log::debug!("{:?} {}", req.method, req.path);

    match (req.method, req.path) {
        (Method::Get, "/api/status") => {
            let st = p.status();
            respond(sock, page_buf, 200, "application/json", &[], |w| {
                    write!(
                        w,
                        "{{\"version\":\"{}\",\"boot\":\"{}\",\"uptime_s\":{},\"provisioned\":{},\"network\":\"{}\",\"setpoint\":{}",
                        p.running_version(),
                        st.boot.as_str(),
                        st.uptime_s,
                        s.is_provisioned(),
                        st.network,
                        Tenths(s.setpoint_dc as i32),
                    )?;
                    match crate::control::average_dc(&st.readings) {
                        Some(t) => write!(w, ",\"temperature\":{}", Tenths(t as i32))?,
                        None => w.write_str(",\"temperature\":null")?,
                    }
                    w.write_str(",\"sensors\":[")?;
                    for (i, r) in st.readings.iter().enumerate() {
                        write!(
                            w,
                            "{}{{\"addr\":\"{}\",\"name\":",
                            if i > 0 { "," } else { "" },
                            crate::ble::format_addr(&r.addr),
                        )?;
                        json_str(w, s.temp_sensor(&r.addr).map_or("", |t| t.name.as_str()))?;
                        write!(
                            w,
                            ",\"zone\":{},\"temperature\":{},\"setpoint\":{},\"band\":\"{}\"}}",
                            r.zone,
                            Tenths(crate::control::cc_to_dc(r.temperature_cc) as i32),
                            Tenths(r.setpoint_dc as i32),
                            r.band(s.hysteresis_dc).as_str()
                        )?;
                    }
                    w.write_str("],\"zones\":[")?;
                    for (i, z) in st.zones.iter().enumerate() {
                        w.write_str(if i > 0 { ",{\"name\":" } else { "{\"name\":" })?;
                        let mut name = String::<24>::new();
                        match s.zone_names.get(i).filter(|n| !n.is_empty()) {
                            Some(n) => {
                                let _ = name.push_str(n);
                            }
                            None => {
                                let _ = write!(name, "Zone {}", i + 1);
                            }
                        }
                        json_str(w, &name)?;
                        write!(
                            w,
                            ",\"heating\":{},\"reason\":\"{}\",\"hold_remaining_s\":",
                            z.heating, z.reason
                        )?;
                        match z.hold_remaining_s {
                            Some(h) => write!(w, "{h}}}")?,
                            None => w.write_str("null}")?,
                        }
                    }
                    write!(
                        w,
                        "],\"sensors_used\":{},\"heating\":{},\"heating_reason\":\"{}\",\"min_on_min\":{}",
                        st.readings.len(), st.heating, st.heating_reason, s.min_on_min
                    )?;
                    write!(
                        w,
                        ",\"stack_used\":{},\"stack_size\":{}",
                        st.stack_used, st.stack_size
                    )?;
                    match st.hold_remaining_s {
                        Some(r) => write!(w, ",\"hold_remaining_s\":{r}")?,
                        None => w.write_str(",\"hold_remaining_s\":null")?,
                    }
                    match (st.unix_time, st.time_synced_ago_s) {
                        (Some(t), Some(ago)) => {
                            let tz = crate::clock::TimeZone::parse(&s.timezone).unwrap_or(crate::clock::TimeZone::UTC);
                            write!(
                                w,
                                ",\"time\":{t},\"local_time\":\"{}\",\"time_synced_ago_s\":{ago}}}",
                                tz.local(t)
                            )
                        }
                        _ => w.write_str(",\"time\":null,\"local_time\":null,\"time_synced_ago_s\":null}"),
                    }
                })
                .await
        }
        (Method::Get, "/api/zone") => {
            let now = p.now_ms();
            let own = own_zone_message(p, &s);
            let snapshot = zone_snapshot(p, now);
            respond(sock, page_buf, 200, "application/json", &[], |w| {
                zone_json(w, own.as_ref(), &snapshot)
            })
            .await
        }
        (Method::Post, "/api/zone/event") => {
            let mut form = [0u8; MAX_FORM];
            let body = read_form(sock, &req, body_prefix, &mut form).await?;
            let now_unix = p.status().unix_time;
            match crate::zones::ZoneMessage::decode(
                body,
                &s.heater_id,
                &s.hostname,
                &s.group_key,
                now_unix,
            ) {
                Ok(msg) => {
                    let now = p.now_ms();
                    p.zones(|g| g.receive(&msg, now));
                    // Reply with our own state so the sender learns it right away.
                    let reply = own_zone_message(p, &s)
                        .map(|m| m.encode(&s.group_key))
                        .unwrap_or_default();
                    respond(sock, page_buf, 200, "text/plain", &[], |w| {
                        w.write_str(&reply)
                    })
                    .await
                }
                Err(e) => {
                    p.zones(|g| g.rejected += 1);
                    let status = match e {
                        crate::zones::ZoneError::BadSignature | crate::zones::ZoneError::Stale => {
                            403
                        }
                        _ => 400,
                    };
                    error(sock, page_buf, status, e.as_str()).await
                }
            }
        }
        (_, "/setup") if s.is_provisioned() => redirect(sock, page_buf, "/", None).await,
        (Method::Get, "/setup") => {
            page(sock, page_buf, "Setup", false, |w| setup_form(w, None)).await
        }
        (Method::Post, "/setup") => {
            let mut form = [0u8; MAX_FORM];
            let form = read_form(sock, &req, body_prefix, &mut form).await?;
            let user: Str32 = http::form_field(form, "user").unwrap_or_default();
            let pass: String<64> = http::form_field(form, "password").unwrap_or_default();
            let pass2: String<64> = http::form_field(form, "password2").unwrap_or_default();
            if let Err(msg) = check_new_password(&pass, &pass2).and(check_username(&user)) {
                return page(sock, page_buf, "Setup", false, |w| setup_form(w, Some(msg))).await;
            }
            s.admin_user = user;
            s.admin_hash = Some(new_hash(p, &pass));
            if p.save_settings(&s).is_err() {
                return error(sock, page_buf, 500, "could not save settings").await;
            }
            let token = new_session(p);
            redirect(sock, page_buf, "/settings", Some(&token)).await
        }
        (_, path) if !s.is_provisioned() && !path.starts_with("/api/") => {
            redirect(sock, page_buf, "/setup", None).await
        }
        (Method::Get, "/login") => {
            page(sock, page_buf, "Login", false, |w| login_form(w, None)).await
        }
        (Method::Post, "/login") => {
            let mut form = [0u8; MAX_FORM];
            let form = read_form(sock, &req, body_prefix, &mut form).await?;
            let user: Str32 = http::form_field(form, "user").unwrap_or_default();
            let pass: String<64> = http::form_field(form, "password").unwrap_or_default();
            if check_credentials(&s, &user, &pass) {
                let token = new_session(p);
                redirect(sock, page_buf, "/", Some(&token)).await
            } else {
                page(sock, page_buf, "Login", false, |w| {
                    login_form(w, Some("Wrong user name or password"))
                })
                .await
            }
        }
        _ if !authed => {
            if req.path.starts_with("/api/") {
                respond(
                    sock,
                    page_buf,
                    401,
                    "text/plain",
                    &["WWW-Authenticate: Basic realm=\"thermostat\""],
                    |w| w.write_str("unauthorized"),
                )
                .await
            } else {
                redirect(sock, page_buf, "/login", None).await
            }
        }
        (Method::Post, "/logout") => {
            if let Some(t) = req.cookie("session") {
                let mut token = [0u8; TOKEN_LEN];
                if hex::decode(t, &mut token).is_some() {
                    p.sessions(|s| s.remove(&token));
                }
            }
            redirect(sock, page_buf, "/login", Some("")).await
        }
        (Method::Get, "/") => {
            let st = p.status();
            let version: Str32 = crate::settings::str32(p.running_version());
            let now = p.now_ms();
            let zone = (!s.heater_id.is_empty()).then(|| zone_snapshot(p, now));
            page(sock, page_buf, "Status", true, |w| {
                status_page(w, &s, &st, &version, zone.as_ref())
            })
            .await
        }
        (Method::Get, "/settings") => {
            let now = p.now_ms();
            let found = sensor_candidates(p);
            let zones = p.zone_count();
            page(sock, page_buf, "Settings", true, |w| {
                settings_form(w, &s, zones, None, &found, now)
            })
            .await
        }
        (Method::Post, "/settings") => {
            let mut form = [0u8; MAX_FORM];
            let form = read_form(sock, &req, body_prefix, &mut form).await?;
            let msg = match apply_settings(p, &mut s, form) {
                Ok(()) => match p.save_settings(&s) {
                    Ok(()) => Notice::Ok("Saved. Network changes apply after a reboot."),
                    Err(_) => Notice::Err("Could not write settings to flash."),
                },
                Err(msg) => {
                    // Show what is stored, not the half-applied form.
                    s = p.settings();
                    Notice::Err(msg)
                }
            };
            let now = p.now_ms();
            let found = sensor_candidates(p);
            let zones = p.zone_count();
            page(sock, page_buf, "Settings", true, |w| {
                settings_form(w, &s, zones, Some(msg), &found, now)
            })
            .await
        }
        (Method::Get, "/ble") => {
            let now = p.now_ms();
            let snapshot =
                p.ble(|t| t.map(|t| (t.sorted(), t.reports, t.scanning, t.error, t.ignored)));
            page(sock, page_buf, "Bluetooth", true, |w| {
                ble_page(w, &s, snapshot.as_ref(), now)
            })
            .await
        }
        (Method::Get, "/api/ble") => {
            let now = p.now_ms();
            let snapshot =
                p.ble(|t| t.map(|t| (t.sorted(), t.reports, t.scanning, t.error, t.ignored)));
            respond(sock, page_buf, 200, "application/json", &[], |w| {
                ble_json(w, snapshot.as_ref(), now)
            })
            .await
        }
        (Method::Get, "/update") => {
            let version: Str32 = crate::settings::str32(p.running_version());
            page(sock, page_buf, "Firmware", true, |w| {
                update_page(w, &version)
            })
            .await
        }
        (Method::Post, "/api/update") => {
            let reboot = req.query_value("reboot") != Some("0");
            match receive_update(sock, p, &req, body_prefix, page_buf).await? {
                Ok(version) => {
                    if reboot {
                        p.request_reboot();
                    }
                    respond(sock, page_buf, 200, "application/json", &[], |w| {
                        write!(
                            w,
                            "{{\"ok\":true,\"version\":\"{}\",\"reboot\":{}}}",
                            Escape(&version),
                            reboot
                        )
                    })
                    .await
                }
                Err(msg) => {
                    respond(sock, page_buf, 400, "application/json", &[], |w| {
                        write!(w, "{{\"ok\":false,\"error\":\"{}\"}}", msg)
                    })
                    .await
                }
            }
        }
        (Method::Post, "/api/test/sensor") => {
            let mut form = [0u8; MAX_FORM];
            let form = read_form(sock, &req, body_prefix, &mut form).await?;
            let addr =
                http::form_field::<24>(form, "addr").and_then(|a| crate::ble::parse_addr(&a));
            let temp = http::form_field::<8>(form, "temp").and_then(|t| http::parse_tenths(&t));
            match (addr, temp) {
                (Some(addr), Some(t)) if p.inject_reading(addr, (t * 10) as i16) => {
                    respond(sock, page_buf, 200, "text/plain", &[], |w| {
                        w.write_str("ok")
                    })
                    .await
                }
                (Some(_), Some(_)) => {
                    error(sock, page_buf, 404, "not supported in this build").await
                }
                _ => {
                    error(
                        sock,
                        page_buf,
                        400,
                        "need addr=AA:BB:CC:DD:EE:FF and temp=21.5",
                    )
                    .await
                }
            }
        }
        (Method::Post, "/api/reboot") => {
            p.request_reboot();
            respond(sock, page_buf, 200, "application/json", &[], |w| {
                w.write_str("{\"ok\":true}")
            })
            .await
        }
        _ => {
            let _ = auth;
            error(sock, page_buf, 404, "not found").await
        }
    }
}

fn authenticate<P: Platform>(req: &Request<'_>, p: &mut P, s: &Settings) -> Auth {
    let now = p.now_ms();
    if let Some(t) = req.cookie("session") {
        let mut token = [0u8; TOKEN_LEN];
        if hex::decode(t, &mut token).is_some() && p.sessions(|s| s.validate(&token, now)) {
            return Auth::Session;
        }
    }
    if let Some(b) = req.authorization.and_then(|a| a.strip_prefix("Basic ")) {
        let mut raw = [0u8; 100];
        if let Some(n) = base64::decode(b.trim(), &mut raw)
            && let Some((user, pass)) = core::str::from_utf8(&raw[..n])
                .ok()
                .and_then(|s| s.split_once(':'))
            && check_credentials(s, user, pass)
        {
            return Auth::Basic;
        }
    }
    Auth::None
}

fn new_session<P: Platform>(p: &mut P) -> String<32> {
    let mut token = [0u8; TOKEN_LEN];
    p.random(&mut token);
    let now = p.now_ms();
    p.sessions(|s| s.create(token, now));
    hex::encode(&token)
}

fn check_credentials(s: &Settings, user: &str, pass: &str) -> bool {
    match &s.admin_hash {
        Some(h) => user == s.admin_user.as_str() && h.verify(pass),
        None => false,
    }
}

fn new_hash<P: Platform>(p: &mut P, pass: &str) -> PasswordHash {
    let mut salt = [0u8; 16];
    p.random(&mut salt);
    PasswordHash::new(pass, salt, DEFAULT_ITERATIONS)
}

fn check_new_password(pass: &str, pass2: &str) -> Result<(), &'static str> {
    if pass.len() < 8 {
        Err("Password must be at least 8 characters.")
    } else if pass != pass2 {
        Err("Passwords do not match.")
    } else {
        Ok(())
    }
}

fn check_username(user: &str) -> Result<(), &'static str> {
    if user.is_empty()
        || !user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        Err("User name may only contain letters, digits, '-' and '_'.")
    } else {
        Ok(())
    }
}

/// Apply a settings form to `s` (the current settings). On error `s` may be
/// partly changed; the caller discards it.
fn apply_settings<P: Platform>(
    p: &mut P,
    s: &mut Settings,
    form: &str,
) -> Result<(), &'static str> {
    let field = |k| http::form_field::<64>(form, k).ok_or("Invalid form data.");
    let hostname = field("hostname")?;
    if hostname.is_empty()
        || hostname.len() > 32
        || !hostname
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err("Hostname: 1-32 letters, digits or '-'.");
    }
    s.hostname = crate::settings::str32(&hostname);

    let ssid = field("ssid")?;
    let wifi_pass = field("wifi_password")?;
    if ssid.len() > 32 {
        return Err("SSID is at most 32 bytes.");
    }
    if ssid.is_empty() {
        s.wifi_ssid.clear();
        s.wifi_psk = None;
    } else if !wifi_pass.is_empty() {
        if !(8..=63).contains(&wifi_pass.len()) {
            return Err("WiFi password must be 8-63 characters.");
        }
        s.wifi_psk = Some(wpa_psk(&wifi_pass, &ssid));
        s.wifi_ssid = crate::settings::str32(&ssid);
    } else if ssid.as_str() != s.wifi_ssid.as_str() {
        return Err("Enter the WiFi password when changing the network.");
    }

    let setpoint = http::parse_tenths(&field("setpoint")?).ok_or("Setpoint must look like 20.5")?;
    if !(50..=350).contains(&setpoint) {
        return Err("Setpoint must be between 5.0 and 35.0 °C.");
    }
    s.setpoint_dc = setpoint as i16;
    let hyst = http::parse_tenths(&field("hysteresis")?).ok_or("Hysteresis must look like 0.5")?;
    if !(1..=50).contains(&hyst) {
        return Err("Hysteresis must be between 0.1 and 5.0 °C.");
    }
    s.hysteresis_dc = hyst as u16;

    let zones = p.zone_count();
    for (i, name) in s.zone_names.iter_mut().enumerate().take(zones) {
        if let Some(raw) = http::form_value(form, &zone_field(i)) {
            let text: String<64> = http::url_decode(raw).ok_or("Zone name is too long.")?;
            *name = crate::settings::ZoneName::try_from(text.trim())
                .map_err(|_| "Zone names can be at most 16 characters.")?;
        }
    }

    // Older forms (and scripts) may omit these fields: keep the stored values.
    // Boards with several zones run their own heater and ignore heater groups.
    if zones > 1 {
        s.heater_id.clear();
    } else if let Some(raw) = http::form_value(form, "heater_id") {
        let id: Str32 = http::url_decode(raw).ok_or("Heater id is too long.")?;
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err("Heater id: letters, digits, '-' and '_' only.");
        }
        s.heater_id = id;
    }
    if http::form_value(form, "clear_group_key").is_some() {
        s.group_key.clear();
    } else if let Some(raw) = http::form_value(form, "group_key") {
        let key: Str32 = http::url_decode(raw).ok_or("Group key is too long.")?;
        if !key.is_empty() {
            if key.len() < 8 {
                return Err("Group key must be at least 8 characters.");
            }
            s.group_key = key;
        }
    }
    if let Some(raw) = http::form_value(form, "min_on") {
        let v: String<8> = http::url_decode(raw).ok_or("Invalid minimum switch time.")?;
        s.min_on_min = v
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|m| *m <= crate::control::MAX_MIN_ON_MIN)
            .ok_or("Minimum on time must be 0 to 1440 minutes.")?;
    }
    if let Some(raw) = http::form_value(form, "ntp_server") {
        let server: String<48> = http::url_decode(raw).ok_or("NTP server is too long.")?;
        crate::sntp::split_server(&server)
            .ok_or("NTP server must be a host name or address, optionally with :port.")?;
        s.ntp_server = server;
    }
    if let Some(raw) = http::form_value(form, "timezone") {
        let tz: String<48> = http::url_decode(raw).ok_or("Timezone is too long.")?;
        crate::clock::TimeZone::parse(&tz)
            .map_err(|_| "Timezone must be a POSIX TZ string such as GMT0BST,M3.5.0/1,M10.5.0.")?;
        s.timezone = tz;
    }

    let previous = core::mem::take(&mut s.temp_sensors);
    for raw in http::form_values(form, "sensor") {
        let text: String<24> = http::url_decode(raw).ok_or("Invalid sensor address.")?;
        let addr = crate::ble::parse_addr(&text).ok_or("Invalid sensor address.")?;
        if s.temp_sensor(&addr).is_some() {
            continue;
        }
        let before = previous.iter().find(|t| t.addr == addr);
        // Its setpoint and name fields; without them, keep the stored values.
        let setpoint_dc = match http::form_value(form, &sensor_field("sp_", &addr)) {
            Some(raw) => {
                let text: String<8> = http::url_decode(raw).ok_or("Invalid sensor setpoint.")?;
                let sp = http::parse_tenths(&text).ok_or("Sensor setpoints must look like 20.5")?;
                if !(50..=350).contains(&sp) {
                    return Err("Sensor setpoints must be between 5.0 and 35.0 °C.");
                }
                sp as i16
            }
            None => before.map_or(s.setpoint_dc, |t| t.setpoint_dc),
        };
        let name = match http::form_value(form, &sensor_field("nm_", &addr)) {
            Some(raw) => {
                let text: String<64> = http::url_decode(raw).ok_or("Sensor name is too long.")?;
                crate::settings::SensorName::try_from(text.trim())
                    .map_err(|_| "Sensor names can be at most 16 characters.")?
            }
            None => before.map(|t| t.name.clone()).unwrap_or_default(),
        };
        let zone = match http::form_value(form, &sensor_field("zn_", &addr)) {
            Some(raw) => raw
                .parse::<u8>()
                .ok()
                .filter(|z| (*z as usize) < zones)
                .ok_or("Invalid zone for a sensor.")?,
            None => before.map_or(0, |t| t.zone).min(zones as u8 - 1),
        };
        if s.temp_sensors
            .push(crate::settings::TempSensor {
                addr,
                setpoint_dc,
                name,
                zone,
            })
            .is_err()
        {
            return Err(SENSOR_LIMIT);
        }
    }

    let new_pass = field("new_password")?;
    if !new_pass.is_empty() {
        check_new_password(&new_pass, &field("new_password2")?)?;
        s.admin_hash = Some(new_hash(p, &new_pass));
    }
    Ok(())
}

async fn read_form<'b, S: Read>(
    sock: &mut S,
    req: &Request<'_>,
    prefix: &[u8],
    buf: &'b mut [u8; MAX_FORM],
) -> Result<&'b str, HttpError> {
    let len = req.content_length;
    if len > MAX_FORM || prefix.len() > len {
        return Err(HttpError::Malformed);
    }
    buf[..prefix.len()].copy_from_slice(prefix);
    let mut got = prefix.len();
    while got < len {
        let n = sock
            .read(&mut buf[got..len])
            .await
            .map_err(|_| HttpError::Io)?;
        if n == 0 {
            return Err(HttpError::Closed);
        }
        got += n;
    }
    core::str::from_utf8(&buf[..len]).map_err(|_| HttpError::Malformed)
}

/// Stream the request body into the OTA sink. The outer `Result` is for I/O
/// failures (connection dropped), the inner one for update errors to report.
async fn receive_update<S: Read, P: Platform>(
    sock: &mut S,
    p: &mut P,
    req: &Request<'_>,
    prefix: &[u8],
    buf: &mut [u8],
) -> Result<Result<String<32>, &'static str>, HttpError> {
    let total = req.content_length as u32;
    if total == 0 {
        return Ok(Err("empty body"));
    }
    if let Err(e) = p.ota_begin(total) {
        return Ok(Err(e.as_str()));
    }
    let mut offset = 0u32;
    let mut pending: &[u8] = &prefix[..prefix.len().min(total as usize)];
    loop {
        if !pending.is_empty() {
            if let Err(e) = p.ota_write(offset, pending) {
                p.ota_abort();
                return Ok(Err(e.as_str()));
            }
            offset += pending.len() as u32;
        }
        if offset >= total {
            break;
        }
        let want = buf.len().min((total - offset) as usize);
        let n = match sock.read(&mut buf[..want]).await {
            Ok(0) | Err(_) => {
                p.ota_abort();
                return Err(HttpError::Closed);
            }
            Ok(n) => n,
        };
        pending = &buf[..n];
    }
    Ok(p.ota_finish().map_err(|e| e.as_str()))
}

async fn respond<S: Write>(
    sock: &mut S,
    buf: &mut [u8],
    status: u16,
    content_type: &str,
    extra_headers: &[&str],
    body: impl FnOnce(&mut BufWriter<'_>) -> core::fmt::Result,
) -> Result<(), HttpError> {
    let reason = match status {
        200 => "OK",
        303 => "See Other",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    // Render the body first to learn its length, then the head after it.
    let (body_buf, head_buf) = buf.split_at_mut(PAGE_LEN - 512);
    let mut b = BufWriter::new(body_buf);
    let _ = body(&mut b);
    let mut h = BufWriter::new(head_buf);
    let _ = write!(
        h,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n",
        b.len()
    );
    for x in extra_headers {
        let _ = write!(h, "{x}\r\n");
    }
    let _ = h.write_str("\r\n");
    http::write_all(sock, h.as_bytes()).await?;
    http::write_all(sock, b.as_bytes()).await?;
    sock.flush().await.map_err(|_| HttpError::Io)
}

async fn redirect<S: Write>(
    sock: &mut S,
    buf: &mut [u8],
    to: &str,
    session: Option<&str>,
) -> Result<(), HttpError> {
    let mut loc: String<64> = String::new();
    let _ = write!(loc, "Location: {to}");
    let mut cookie: String<128> = String::new();
    match session {
        Some("") => {
            let _ = cookie
                .push_str("Set-Cookie: session=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict");
        }
        Some(t) => {
            let _ = write!(
                cookie,
                "Set-Cookie: session={t}; Path=/; HttpOnly; SameSite=Strict"
            );
        }
        None => {}
    }
    let headers: &[&str] = if cookie.is_empty() {
        &[&loc]
    } else {
        &[&loc, &cookie]
    };
    respond(sock, buf, 303, "text/plain", headers, |_| Ok(())).await
}

async fn error<S: Write>(
    sock: &mut S,
    buf: &mut [u8],
    status: u16,
    msg: &str,
) -> Result<(), HttpError> {
    respond(sock, buf, status, "text/plain", &[], |w| w.write_str(msg)).await
}

async fn page<S: Write>(
    sock: &mut S,
    buf: &mut [u8],
    title: &str,
    nav: bool,
    content: impl FnOnce(&mut BufWriter<'_>) -> core::fmt::Result,
) -> Result<(), HttpError> {
    respond(sock, buf, 200, "text/html; charset=utf-8", &[], |w| {
        write!(
            w,
            "<!doctype html><html><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
<title>{title} · Thermostat</title><style>{CSS}</style></head><body><header><b>Thermostat</b>"
        )?;
        if nav {
            w.write_str(
                "<nav><a href=/>Status</a><a href=/settings>Settings</a><a href=/update>Firmware</a><a href=/ble>Bluetooth</a>\
<form method=post action=/logout><button>Log out</button></form></nav>",
            )?;
        }
        write!(w, "</header><main><h1>{title}</h1>")?;
        content(w)?;
        w.write_str("</main></body></html>")
    })
    .await
}

const CSS: &str = "body{font-family:system-ui,sans-serif;margin:0;background:#f4f5f7;color:#222}\
header{display:flex;gap:1em;align-items:center;justify-content:space-between;background:#234;color:#fff;padding:.6em 1em}\
nav{display:flex;gap:1em;align-items:center}nav a{color:#fff}nav form{margin:0}\
main{max-width:34em;margin:1em auto;background:#fff;padding:1em 1.5em;border-radius:6px}\
label{display:block;margin:.6em 0 .2em}input{width:100%;padding:.4em;box-sizing:border-box}\
button{margin-top:1em;padding:.4em 1em}fieldset{margin:1em 0;border:1px solid #ccc}\
.ok{background:#dfd;padding:.5em}.err{background:#fdd;padding:.5em}td{padding:.2em .8em .2em 0}\
input[type=checkbox]{width:auto}.s{margin-left:2em}";

enum Notice {
    Ok(&'static str),
    Err(&'static str),
}

fn notice(w: &mut BufWriter<'_>, n: Option<Notice>) -> core::fmt::Result {
    match n {
        Some(Notice::Ok(m)) => write!(w, "<p class=ok>{m}</p>"),
        Some(Notice::Err(m)) => write!(w, "<p class=err>{m}</p>"),
        None => Ok(()),
    }
}

fn setup_form(w: &mut BufWriter<'_>, err: Option<&'static str>) -> core::fmt::Result {
    notice(w, err.map(Notice::Err))?;
    w.write_str(
        "<p>Choose the administrator account for this device.</p><form method=post action=/setup>\
<label>User name<input name=user value=admin required></label>\
<label>Password<input type=password name=password minlength=8 required></label>\
<label>Repeat password<input type=password name=password2 minlength=8 required></label>\
<button>Save</button></form>",
    )
}

fn login_form(w: &mut BufWriter<'_>, err: Option<&'static str>) -> core::fmt::Result {
    notice(w, err.map(Notice::Err))?;
    w.write_str(
        "<form method=post action=/login><label>User name<input name=user required autofocus></label>\
<label>Password<input type=password name=password required></label><button>Log in</button></form>",
    )
}

fn status_page(
    w: &mut BufWriter<'_>,
    s: &Settings,
    st: &crate::platform::DeviceStatus,
    version: &str,
    zone: Option<&ZoneSnapshot>,
) -> core::fmt::Result {
    write!(
        w,
        "<table><tr><td>Firmware</td><td>{}</td></tr><tr><td>Boot state</td><td>{}</td></tr>\
<tr><td>Uptime</td><td>{} s</td></tr><tr><td>Network</td><td>{}</td></tr>\
<tr><td>Sensors</td><td>",
        Escape(version),
        st.boot.as_str(),
        st.uptime_s,
        Escape(&st.network),
    )?;
    if st.zones.len() > 1 {
        // Several zones: one row each, with its relay state and its sensors.
        w.write_str("</td></tr>")?;
        for (z, zs) in st.zones.iter().enumerate() {
            w.write_str("<tr><td>")?;
            zone_name(w, s, z)?;
            write!(
                w,
                "</td><td><b>{}</b> <small>({}",
                if zs.heating { "ON" } else { "off" },
                zs.reason
            )?;
            hold_text(w, zs.hold_remaining_s)?;
            w.write_str(")</small>")?;
            let mut any = false;
            for t in s.temp_sensors.iter().filter(|t| t.zone as usize == z) {
                any = true;
                w.write_str("<br>")?;
                sensor_line(w, s, st, t)?;
            }
            if !any {
                w.write_str("<br>no sensors in this zone (see Settings)")?;
            }
            w.write_str("</td></tr>")?;
        }
        return status_tail(w, s, st, zone);
    }
    if s.temp_sensors.is_empty() {
        w.write_str("no sensors selected (see Settings)")?;
    }
    for t in &s.temp_sensors {
        sensor_line(w, s, st, t)?;
        w.write_str("<br>")?;
    }
    write!(
        w,
        "</td></tr><tr><td>Heating</td><td><b>{}</b> <small>({}",
        if st.heating { "ON" } else { "off" },
        st.heating_reason
    )?;
    hold_text(w, st.hold_remaining_s)?;
    w.write_str(")</small></td></tr>")?;
    status_tail(w, s, st, zone)
}

fn hold_text(w: &mut BufWriter<'_>, hold_s: Option<u64>) -> core::fmt::Result {
    if let Some(r) = hold_s {
        let m = r.div_ceil(60);
        write!(w, ", {} h {:02} min to go", m / 60, m % 60)?;
    }
    Ok(())
}

fn sensor_line(
    w: &mut BufWriter<'_>,
    s: &Settings,
    st: &crate::platform::DeviceStatus,
    t: &crate::settings::TempSensor,
) -> core::fmt::Result {
    {
        sensor_label(w, s, &t.addr)?;
        w.write_str(" ")?;
        match st.readings.iter().find(|r| r.addr == t.addr) {
            Some(r) => write!(
                w,
                "<b>{} °C</b> · setpoint {} °C · {}",
                Tenths(crate::control::cc_to_dc(r.temperature_cc) as i32),
                Tenths(t.setpoint_dc as i32),
                r.band(s.hysteresis_dc).as_str()
            )?,
            None => write!(
                w,
                "no fresh reading · setpoint {} °C",
                Tenths(t.setpoint_dc as i32)
            )?,
        }
    }
    Ok(())
}

/// Heater group (if any) and the end of the status table.
fn status_tail(
    w: &mut BufWriter<'_>,
    s: &Settings,
    st: &crate::platform::DeviceStatus,
    zone: Option<&ZoneSnapshot>,
) -> core::fmt::Result {
    if let Some(z) = zone {
        write!(
            w,
            "<tr><td>Heater group</td><td><code>{}</code>: ",
            Escape(&s.heater_id)
        )?;
        if z.peers.is_empty() {
            w.write_str("no other zones found yet")?;
        }
        for (i, (name, _, heat, _, active)) in z.peers.iter().enumerate() {
            let state = match (active, heat) {
                (false, _) => "silent",
                (true, true) => "heating",
                (true, false) => "idle",
            };
            write!(
                w,
                "{}{} ({state})",
                if i > 0 { ", " } else { "" },
                Escape(name)
            )?;
        }
        w.write_str("</td></tr>")?;
    }
    w.write_str("<tr><td>Time</td><td>")?;
    match (st.unix_time, st.time_synced_ago_s) {
        (Some(t), Some(ago)) => {
            let tz =
                crate::clock::TimeZone::parse(&s.timezone).unwrap_or(crate::clock::TimeZone::UTC);
            let l = tz.local(t);
            write!(
                w,
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02} <small>(UTC{}{:02}:{:02}{}, synced {} s ago)</small>",
                l.year,
                l.month,
                l.day,
                l.hour,
                l.minute,
                l.second,
                if l.offset_s < 0 { '-' } else { '+' },
                l.offset_s.unsigned_abs() / 3600,
                l.offset_s.unsigned_abs() / 60 % 60,
                if l.dst { ", DST" } else { "" },
                ago
            )?
        }
        _ => write!(
            w,
            "not synchronised yet <small>(NTP: {})</small>",
            Escape(&s.ntp_server)
        )?,
    }
    w.write_str("</td></tr>")?;
    w.write_str(
        "</table><form method=post action=/api/reboot onsubmit=\"return confirm('Reboot now?')\">\
<button>Reboot</button></form>",
    )
}

/// Nearby temperature sensors (the BLE table keeps nothing else), strongest first.
type Candidates = heapless::Vec<crate::ble::BleDevice, { crate::ble::MAX_DEVICES }>;

fn sensor_candidates<P: Platform>(p: &mut P) -> Candidates {
    p.ble(|t| t.map(|t| t.sorted()).unwrap_or_default())
}

const SENSOR_LIMIT: &str = "Select at most 8 temperature sensors.";
const _: () = assert!(
    crate::settings::MAX_TEMP_SENSORS == 8,
    "update SENSOR_LIMIT"
);

/// Form field for one sensor: `prefix` (`sp_` setpoint, `nm_` name) and the address in hex.
fn sensor_field(prefix: &str, addr: &[u8; 6]) -> String<16> {
    let mut f = String::new();
    let _ = f.push_str(prefix);
    for b in addr.iter().rev() {
        let _ = write!(f, "{b:02X}");
    }
    f
}

/// Form field for a zone's name.
fn zone_field(zone: usize) -> String<8> {
    let mut f = String::new();
    let _ = write!(f, "zone{zone}");
    f
}

/// A zone's display name: its configured name or "Zone N".
fn zone_name(w: &mut BufWriter<'_>, s: &Settings, zone: usize) -> core::fmt::Result {
    match s.zone_names.get(zone).filter(|n| !n.is_empty()) {
        Some(n) => write!(w, "{}", Escape(n)),
        None => write!(w, "Zone {}", zone + 1),
    }
}

fn sensor_inputs(
    w: &mut BufWriter<'_>,
    s: &Settings,
    zones: usize,
    addr: &[u8; 6],
    t: Option<&crate::settings::TempSensor>,
) -> core::fmt::Result {
    write!(
        w,
        "<div class=s><label>Name<input name={} value=\"{}\" maxlength={}></label>\
<label>Setpoint (°C)<input name={} value={} inputmode=decimal size=5></label>",
        sensor_field("nm_", addr),
        Escape(t.map_or("", |t| t.name.as_str())),
        crate::settings::MAX_SENSOR_NAME,
        sensor_field("sp_", addr),
        Tenths(t.map_or(s.setpoint_dc, |t| t.setpoint_dc) as i32)
    )?;
    if zones > 1 {
        write!(w, "<label>Zone<select name={}>", sensor_field("zn_", addr))?;
        for z in 0..zones {
            let sel = if t.map_or(0, |t| t.zone as usize) == z {
                " selected"
            } else {
                ""
            };
            write!(w, "<option value={z}{sel}>")?;
            zone_name(w, s, z)?;
            w.write_str("</option>")?;
        }
        w.write_str("</select></label>")?;
    }
    w.write_str("</div>")
}

/// `<b>name</b> <code>addr</code>`, or just the address when unnamed.
fn sensor_label(w: &mut BufWriter<'_>, s: &Settings, addr: &[u8; 6]) -> core::fmt::Result {
    if let Some(t) = s.temp_sensor(addr).filter(|t| !t.name.is_empty()) {
        write!(w, "<b>{}</b> ", Escape(&t.name))?;
    }
    write!(w, "<code>{}</code>", crate::ble::format_addr(addr))
}

fn sensor_fieldset(
    w: &mut BufWriter<'_>,
    s: &Settings,
    zones: usize,
    found: &Candidates,
    now: u64,
) -> core::fmt::Result {
    write!(
        w,
        "<fieldset><legend>Temperature sensors</legend><p><small>Check up to {} sensors and give each a \
name (shown on these pages only; the sensor itself keeps its own name) and a setpoint. Heating comes on when any checked sensor drops to its setpoint minus half the hysteresis, and goes \
off once all are at their setpoint plus half the hysteresis (after the minimum on time). Readings older than \
{} minutes are ignored; with none, heating stays off.</small></p>",
        crate::settings::MAX_TEMP_SENSORS,
        crate::control::READING_MAX_AGE_MS / 60_000
    )?;
    let mut any = false;
    for d in found {
        any = true;
        let selected = s.temp_sensor(&d.addr);
        let checked = if selected.is_some() { " checked" } else { "" };
        write!(
            w,
            "<label><input type=checkbox name=sensor value={}{checked}> ",
            d.addr_string(),
        )?;
        sensor_label(w, s, &d.addr)?;
        write!(
            w,
            " {} · ",
            Escape(d.name.as_deref().or(d.model).unwrap_or(""))
        )?;
        if let Some(r) = &d.sensor {
            sensor_text(w, r)?;
        }
        write!(
            w,
            " · {} dBm · {} s ago</label>",
            d.rssi,
            now.saturating_sub(d.sensor_ms) / 1000
        )?;
        sensor_inputs(w, s, zones, &d.addr, selected)?;
    }
    for t in &s.temp_sensors {
        if !found.iter().any(|d| d.addr == t.addr) {
            any = true;
            let addr = crate::ble::format_addr(&t.addr);
            write!(
                w,
                "<label><input type=checkbox name=sensor value={addr} checked> "
            )?;
            sensor_label(w, s, &t.addr)?;
            w.write_str(" · not heard recently</label>")?;
            sensor_inputs(w, s, zones, &t.addr, Some(t))?;
        }
    }
    if !any {
        w.write_str("<p>No temperature sensors heard yet. Supported: ThermoPro TP35x/TP39x, pvvx/ATC firmware, BTHome v2.</p>")?;
    }
    w.write_str("</fieldset>")
}

fn settings_form(
    w: &mut BufWriter<'_>,
    s: &Settings,
    zones: usize,
    n: Option<Notice>,
    found: &Candidates,
    now: u64,
) -> core::fmt::Result {
    notice(w, n)?;
    write!(
        w,
        "<form method=post action=/settings><fieldset><legend>Network</legend>\
<label>Hostname<input name=hostname value=\"{}\" maxlength=32 required></label>\
<label>WiFi network (SSID); empty = setup access point<input name=ssid value=\"{}\" maxlength=32></label>\
<label>WiFi password{}<input type=password name=wifi_password maxlength=63 autocomplete=new-password></label>\
</fieldset><fieldset><legend>Thermostat</legend>\
<label>Default setpoint for newly checked sensors (°C)<input name=setpoint value=\"{}\" inputmode=decimal></label>\
<label>Hysteresis (°C): on at setpoint − half, off at setpoint + half<input name=hysteresis value=\"{}\" inputmode=decimal></label>\
<label>Minimum heater on time (minutes, 0 = off, default 240 = 4 h). Never delays heating; once on, the heater runs at least this long\
<input name=min_on value=\"{}\" inputmode=numeric></label>\
</fieldset><fieldset><legend>Time</legend>\
<label>NTP server (host or host:port)<input name=ntp_server value=\"{}\" maxlength=48 required></label>\
<label>Timezone (POSIX TZ, e.g. GMT0BST,M3.5.0/1,M10.5.0 · CET-1CEST,M3.5.0,M10.5.0/3 · EST5EDT,M3.2.0,M11.1.0 · UTC0)\
<input name=timezone value=\"{}\" maxlength=48 required></label></fieldset>",
        Escape(&s.hostname),
        Escape(&s.wifi_ssid),
        if s.wifi_psk.is_some() {
            " (stored as a hashed key; leave empty to keep)"
        } else {
            ""
        },
        Tenths(s.setpoint_dc as i32),
        Tenths(s.hysteresis_dc as i32),
        s.min_on_min,
        Escape(&s.ntp_server),
        Escape(&s.timezone),
    )?;
    if zones > 1 {
        w.write_str(
            "<fieldset><legend>Zones</legend><p><small>One relay per zone, both on one heater: it runs \
while any zone requests heat, and the minimum on time applies to the heater as a whole.</small></p>",
        )?;
        for z in 0..zones {
            write!(
                w,
                "<label>Zone {} name<input name={} value=\"{}\" maxlength={} placeholder=\"Zone {}\"></label>",
                z + 1,
                zone_field(z),
                Escape(&s.zone_names[z]),
                crate::settings::MAX_SENSOR_NAME,
                z + 1
            )?;
        }
        w.write_str("</fieldset>")?;
    } else {
        heater_group_fieldset(w, s)?;
    }
    sensor_fieldset(w, s, zones, found, now)?;
    w.write_str(
        "<fieldset><legend>Admin password</legend>\
<label>New password (leave empty to keep)<input type=password name=new_password minlength=8 autocomplete=new-password></label>\
<label>Repeat new password<input type=password name=new_password2 autocomplete=new-password></label>\
</fieldset><button>Save</button></form>",
    )
}

fn heater_group_fieldset(w: &mut BufWriter<'_>, s: &Settings) -> core::fmt::Result {
    write!(
        w,
        "<fieldset><legend>Heater group</legend><p><small>Thermostats with the same heater id share one heater: \
it runs while any zone requests heat, and the minimum on time applies to the heater as a whole. \
Leave empty if this thermostat has its own heater.</small></p>\
<label>Heater id<input name=heater_id value=\"{}\" maxlength=32></label>\
<label>Group key (same on every zone; signs zone messages){}<input type=password name=group_key maxlength=32 autocomplete=new-password></label>\
{}</fieldset>",
        Escape(&s.heater_id),
        if s.group_key.is_empty() {
            " — none set, messages are unsigned"
        } else {
            " — set; leave empty to keep"
        },
        if s.group_key.is_empty() {
            ""
        } else {
            "<label><input type=checkbox name=clear_group_key> Remove the group key</label>"
        },
    )
}

/// This zone's state as a group message (None when not in a heater group).
fn own_zone_message<P: Platform>(p: &mut P, s: &Settings) -> Option<crate::zones::ZoneMessage> {
    if s.heater_id.is_empty() {
        return None;
    }
    let st = p.status();
    let heater_on_since = p.zones(|g| g.heater_on_since);
    Some(crate::zones::ZoneMessage {
        heater: s.heater_id.clone(),
        zone: s.hostname.clone(),
        heat: st.heat_request,
        heater_on_since,
        ts: st.unix_time,
    })
}

/// (zone, address, heating, seconds since heard, active)
type ZonePeerRow = (
    crate::zones::Name,
    Option<([u8; 4], u16)>,
    bool,
    Option<u64>,
    bool,
);
struct ZoneSnapshot {
    peers: heapless::Vec<ZonePeerRow, { crate::zones::MAX_PEERS }>,
    heater_on_since: Option<i64>,
    rejected: u32,
}

fn zone_snapshot<P: Platform>(p: &mut P, now: u64) -> ZoneSnapshot {
    p.zones(|g| {
        let mut peers = heapless::Vec::new();
        for peer in g.peers() {
            let _ = peers.push((
                peer.zone.clone(),
                peer.addr,
                peer.heat,
                peer.heard_ms.map(|t| now.saturating_sub(t) / 1000),
                g.is_fresh(peer, now),
            ));
        }
        ZoneSnapshot {
            peers,
            heater_on_since: g.heater_on_since,
            rejected: g.rejected,
        }
    })
}

fn json_str(w: &mut BufWriter<'_>, s: &str) -> core::fmt::Result {
    w.write_char('"')?;
    for c in s.chars() {
        match c {
            '"' => w.write_str("\\\"")?,
            '\\' => w.write_str("\\\\")?,
            c if c.is_control() => {}
            c => w.write_char(c)?,
        }
    }
    w.write_char('"')
}

fn zone_json(
    w: &mut BufWriter<'_>,
    own: Option<&crate::zones::ZoneMessage>,
    z: &ZoneSnapshot,
) -> core::fmt::Result {
    let Some(own) = own else {
        return w.write_str("{\"heater\":null}");
    };
    w.write_str("{\"heater\":")?;
    json_str(w, &own.heater)?;
    w.write_str(",\"zone\":")?;
    json_str(w, &own.zone)?;
    write!(w, ",\"heat\":{},\"heater_on_since\":", own.heat)?;
    match z.heater_on_since {
        Some(t) => write!(w, "{t}")?,
        None => w.write_str("null")?,
    }
    write!(w, ",\"rejected\":{},\"peers\":[", z.rejected)?;
    for (i, (zone, addr, heat, age, active)) in z.peers.iter().enumerate() {
        if i > 0 {
            w.write_char(',')?;
        }
        w.write_str("{\"zone\":")?;
        json_str(w, zone)?;
        match addr {
            Some((a, port)) => write!(
                w,
                ",\"addr\":\"{}.{}.{}.{}:{port}\"",
                a[0], a[1], a[2], a[3]
            )?,
            None => w.write_str(",\"addr\":null")?,
        }
        write!(w, ",\"heat\":{heat},\"active\":{active},\"age_s\":")?;
        match age {
            Some(a) => write!(w, "{a}}}")?,
            None => w.write_str("null}")?,
        }
    }
    w.write_str("]}")
}

type BleSnapshot = (
    heapless::Vec<crate::ble::BleDevice, { crate::ble::MAX_DEVICES }>,
    u32,
    bool,
    Option<&'static str>,
    u32,
);

fn sensor_text(w: &mut BufWriter<'_>, r: &crate::ble::SensorReading) -> core::fmt::Result {
    if let Some(t) = r.temperature_cc {
        write!(w, "{} °C ", Tenths(t as i32 / 10))?;
    }
    if let Some(h) = r.humidity_cp {
        write!(w, "{} % ", Tenths(h as i32 / 10))?;
    }
    if let Some(b) = r.battery_pct {
        write!(w, "🔋{b}%")?;
    }
    Ok(())
}

fn ble_page(
    w: &mut BufWriter<'_>,
    s: &Settings,
    snap: Option<&BleSnapshot>,
    now: u64,
) -> core::fmt::Result {
    let Some((devices, reports, scanning, error, ignored)) = snap else {
        return w.write_str("<p>This build has no Bluetooth radio (emulator).</p>");
    };
    write!(
        w,
        "<p>{} · {} temperature sensors · {} advertisements received, {} of them from other devices \
(phones, laptops, beacons) and not kept. Refreshes every 5 s.</p>",
        if *scanning {
            "Passive scan running"
        } else {
            "Scanner not running"
        },
        devices.len(),
        reports,
        ignored
    )?;
    if let Some(e) = error {
        write!(w, "<p class=err>{}</p>", Escape(e))?;
    }
    w.write_str(
        "<div style=overflow-x:auto><table style=font-size:.85em><tr><th>Address</th><th>RSSI</th><th>Name</th>\
<th>Maker</th><th>Services</th><th>Reading</th><th>Seen</th></tr>",
    )?;
    for d in devices {
        w.write_str("<tr><td>")?;
        sensor_label(w, s, &d.addr)?;
        if d.addr_kind != crate::ble::AddrKind::Public {
            write!(w, " <small>{}</small>", d.addr_kind.as_str())?;
        }
        write!(
            w,
            "</td><td>{}</td><td>{}</td><td>",
            d.rssi,
            Escape(d.name.as_deref().unwrap_or(""))
        )?;
        if let Some(m) = d.model {
            w.write_str(m)?;
        } else if let Some(c) = d.company {
            match crate::ble::company_name(c) {
                Some(n) => w.write_str(n)?,
                None => write!(w, "0x{c:04X}")?,
            }
        }
        w.write_str("</td><td>")?;
        for (i, s) in d.services.iter().enumerate() {
            write!(w, "{}{s:04X}", if i > 0 { "<br>" } else { "" })?;
            if let Some(n) = crate::ble::service_name(*s) {
                write!(w, " <small>{n}</small>")?;
            }
        }
        w.write_str("</td><td>")?;
        if let Some(r) = &d.sensor {
            sensor_text(w, r)?;
        }
        write!(
            w,
            "</td><td>{} s ago · {}×</td></tr>",
            now.saturating_sub(d.last_seen_ms) / 1000,
            d.packets
        )?;
    }
    w.write_str("</table></div><script>setTimeout(()=>location.reload(),5000)</script>")
}

fn ble_json(w: &mut BufWriter<'_>, snap: Option<&BleSnapshot>, now: u64) -> core::fmt::Result {
    let Some((devices, reports, scanning, _, ignored)) = snap else {
        return w.write_str("{\"supported\":false}");
    };
    write!(
        w,
        "{{\"supported\":true,\"scanning\":{scanning},\"reports\":{reports},\"ignored\":{ignored},\"devices\":["
    )?;
    for (i, d) in devices.iter().enumerate() {
        write!(
            w,
            "{}{{\"addr\":\"{}\",\"addr_kind\":\"{}\",\"rssi\":{},\"packets\":{},\"age_s\":{}",
            if i > 0 { "," } else { "" },
            d.addr_string(),
            d.addr_kind.as_str(),
            d.rssi,
            d.packets,
            now.saturating_sub(d.last_seen_ms) / 1000
        )?;
        if let Some(n) = &d.name {
            // Names are printable (control characters stripped); escape JSON specials.
            w.write_str(",\"name\":\"")?;
            for c in n.chars() {
                match c {
                    '"' => w.write_str("\\\"")?,
                    '\\' => w.write_str("\\\\")?,
                    c => w.write_char(c)?,
                }
            }
            w.write_str("\"")?;
        }
        if let Some(c) = d.company {
            write!(w, ",\"company\":{c}")?;
        }
        if let Some(m) = d.model {
            write!(w, ",\"model\":\"{m}\"")?;
        }
        if let Some(r) = &d.sensor {
            if let Some(t) = r.temperature_cc {
                write!(w, ",\"temperature\":{}", Tenths(t as i32 / 10))?;
            }
            if let Some(h) = r.humidity_cp {
                write!(w, ",\"humidity\":{}", Tenths(h as i32 / 10))?;
            }
            if let Some(b) = r.battery_pct {
                write!(w, ",\"battery\":{b}")?;
            }
        }
        w.write_str("}")?;
    }
    w.write_str("]}")
}

fn update_page(w: &mut BufWriter<'_>, version: &str) -> core::fmt::Result {
    write!(
        w,
        "<p>Running <b>{}</b>. Upload a <code>.img</code> built by <code>cargo xtask build</code>. \
The new firmware is verified, swapped in on reboot and rolled back automatically if it fails to confirm itself.</p>\
<input type=file id=f accept=.img><button id=b>Upload</button><p><progress id=p max=1 value=0></progress> <span id=m></span></p>\
<script>{}</script>",
        Escape(version),
        UPDATE_JS
    )
}

const UPDATE_JS: &str = "b.onclick=()=>{const file=f.files[0];if(!file)return;const x=new XMLHttpRequest();\
x.open('POST','/api/update');x.upload.onprogress=e=>{p.value=e.loaded/e.total};\
x.onload=()=>{const r=JSON.parse(x.responseText);if(!r.ok){m.textContent='Failed: '+r.error;return}\
m.textContent='Installed '+r.version+', rebooting…';const poll=()=>fetch('/api/status').then(r=>r.json())\
.then(s=>{m.textContent='Running '+s.version+' ('+s.boot+')'}).catch(()=>setTimeout(poll,2000));setTimeout(poll,4000)};\
x.onerror=()=>{m.textContent='Upload failed'};x.send(file)}";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{BootState, DeviceStatus, Ota, OtaError, tests::FakeOta};
    use crate::store::StoreError;
    use std::string::String as StdString;
    use std::vec::Vec;

    struct Sock {
        input: Vec<u8>,
        pos: usize,
        output: Vec<u8>,
    }

    impl embedded_io_async::ErrorType for Sock {
        type Error = core::convert::Infallible;
    }
    impl Read for Sock {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            // Small reads exercise the streaming paths.
            let n = buf.len().min(97).min(self.input.len() - self.pos);
            buf[..n].copy_from_slice(&self.input[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }
    impl Write for Sock {
        async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }
        async fn flush(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct Fake {
        settings: Settings,
        ota: FakeOta,
        reboot: bool,
        rng: u8,
        saves: usize,
        sessions: crate::auth::Sessions,
        ble: Option<crate::ble::BleTable>,
        time: Option<i64>,
        hold: Option<u64>,
        /// Zones on the fake board (0 = default single zone) and their status.
        zone_count: usize,
        zone_status: std::vec::Vec<crate::platform::ZoneStatus>,
        readings: crate::control::Readings,
        group: crate::zones::Group,
        injected: Option<([u8; 6], i16)>,
    }

    impl Ota for Fake {
        fn running_version(&self) -> &str {
            self.ota.running_version()
        }
        fn ota_begin(&mut self, len: u32) -> Result<(), OtaError> {
            self.ota.ota_begin(len)
        }
        fn ota_write(&mut self, offset: u32, data: &[u8]) -> Result<(), OtaError> {
            self.ota.ota_write(offset, data)
        }
        fn ota_finish(&mut self) -> Result<String<32>, OtaError> {
            self.ota.ota_finish()
        }
        fn ota_abort(&mut self) {
            self.ota.ota_abort()
        }
    }

    impl Platform for Fake {
        fn settings(&self) -> Settings {
            self.settings.clone()
        }
        fn save_settings(&mut self, s: &Settings) -> Result<(), StoreError> {
            self.settings = s.clone();
            self.saves += 1;
            Ok(())
        }
        fn random(&mut self, buf: &mut [u8]) {
            for b in buf {
                self.rng = self.rng.wrapping_add(17);
                *b = self.rng;
            }
        }
        fn now_ms(&self) -> u64 {
            1000
        }
        fn status(&mut self) -> DeviceStatus {
            DeviceStatus {
                boot: BootState::Booted,
                uptime_s: 5,
                network: String::try_from("qemu 10.0.2.15").unwrap(),
                readings: self.readings.clone(),
                zones: self.zone_status.iter().copied().collect(),
                stack_used: 0,
                stack_size: 0,
                heating: false,
                heat_request: false,
                heating_reason: "no sensor reading",
                hold_remaining_s: self.hold,
                unix_time: self.time,
                time_synced_ago_s: self.time.map(|_| 42),
            }
        }
        fn request_reboot(&mut self) {
            self.reboot = true;
        }
        fn sessions<R>(&mut self, f: impl FnOnce(&mut crate::auth::Sessions) -> R) -> R {
            f(&mut self.sessions)
        }
        fn ble<R>(&mut self, f: impl FnOnce(Option<&crate::ble::BleTable>) -> R) -> R {
            f(self.ble.as_ref())
        }
        fn zones<R>(&mut self, f: impl FnOnce(&mut crate::zones::Group) -> R) -> R {
            f(&mut self.group)
        }
        fn zone_count(&self) -> usize {
            self.zone_count.max(1)
        }
        fn inject_reading(&mut self, addr: [u8; 6], temperature_cc: i16) -> bool {
            self.injected = Some((addr, temperature_cc));
            true
        }
    }

    struct Resp {
        status: u16,
        head: StdString,
        body: Vec<u8>,
    }

    impl Resp {
        fn header(&self, name: &str) -> Option<&str> {
            self.head
                .lines()
                .find_map(|l| l.strip_prefix(name)?.strip_prefix(": "))
        }
        fn text(&self) -> StdString {
            StdString::from_utf8_lossy(&self.body).into_owned()
        }
        fn session(&self) -> StdString {
            let c = self.header("Set-Cookie").expect("cookie");
            c.split(';').next().unwrap().to_string()
        }
    }

    fn send(p: &mut Fake, raw: Vec<u8>) -> Resp {
        let mut sock = Sock {
            input: raw,
            pos: 0,
            output: Vec::new(),
        };
        let mut buf = std::vec![0u8; BUF_LEN];
        futures_executor::block_on(handle(&mut sock, p, &mut buf)).unwrap();
        let out = sock.output;
        let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = StdString::from_utf8(out[..split].to_vec()).unwrap();
        let status = head[9..12].parse().unwrap();
        let resp = Resp {
            status,
            head,
            body: out[split + 4..].to_vec(),
        };
        assert_eq!(
            resp.header("Content-Length")
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            resp.body.len()
        );
        resp
    }

    fn get(p: &mut Fake, path: &str, cookie: &str) -> Resp {
        send(
            p,
            std::format!("GET {path} HTTP/1.1\r\nCookie: {cookie}\r\n\r\n").into_bytes(),
        )
    }

    fn post(p: &mut Fake, path: &str, cookie: &str, body: &[u8]) -> Resp {
        let mut raw = std::format!(
            "POST {path} HTTP/1.1\r\nCookie: {cookie}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        raw.extend_from_slice(body);
        send(p, raw)
    }

    fn provisioned() -> (Fake, StdString) {
        let mut p = Fake::default();
        let r = post(
            &mut p,
            "/setup",
            "",
            b"user=admin&password=secret123&password2=secret123",
        );
        assert_eq!(r.status, 303);
        let cookie = r.session();
        (p, cookie)
    }

    #[test]
    fn first_boot_requires_setup() {
        let mut p = Fake::default();
        let r = get(&mut p, "/", "");
        assert_eq!((r.status, r.header("Location")), (303, Some("/setup")));
        let r = post(
            &mut p,
            "/setup",
            "",
            b"user=admin&password=short&password2=short",
        );
        assert!(r.text().contains("at least 8"));
        assert!(!p.settings.is_provisioned());
        // Status API stays public.
        let r = get(&mut p, "/api/status", "");
        assert!(r.text().contains("\"provisioned\":false"));
    }

    #[test]
    fn setup_then_login_flow() {
        let (mut p, cookie) = provisioned();
        assert!(p.settings.is_provisioned());
        // Setup is closed once provisioned.
        let r = post(
            &mut p,
            "/setup",
            "",
            b"user=evil&password=12345678&password2=12345678",
        );
        assert_eq!(r.header("Location"), Some("/"));
        assert_eq!(p.settings.admin_user, "admin");

        assert_eq!(get(&mut p, "/", &cookie).status, 200);
        assert_eq!(
            get(&mut p, "/", "session=00").header("Location"),
            Some("/login")
        );
        let r = post(&mut p, "/login", "", b"user=admin&password=wrong");
        assert!(r.text().contains("Wrong user name"));
        let r = post(&mut p, "/login", "", b"user=admin&password=secret123");
        assert_eq!(r.status, 303);
        let c2 = r.session();
        assert!(
            get(&mut p, "/settings", &c2)
                .text()
                .contains("name=hostname")
        );
        post(&mut p, "/logout", &c2, b"");
        assert_eq!(get(&mut p, "/settings", &c2).status, 303);
    }

    #[test]
    fn settings_update() {
        let (mut p, cookie) = provisioned();
        let form = b"hostname=livingroom&ssid=Home+Net&wifi_password=wifipass1&setpoint=21.5&hysteresis=0.3&new_password=&new_password2=";
        let r = post(&mut p, "/settings", &cookie, form);
        assert!(r.text().contains("Saved"), "{}", r.text());
        assert_eq!(p.settings.hostname, "livingroom");
        assert_eq!(p.settings.wifi_ssid, "Home Net");
        assert_eq!(p.settings.wifi_psk, Some(wpa_psk("wifipass1", "Home Net")));
        assert_eq!(p.settings.setpoint_dc, 215);
        assert_eq!(p.settings.hysteresis_dc, 3);
        // The stored key is never echoed back.
        assert!(!r.text().contains("wifipass1"));

        // Changing SSID without a password is rejected and nothing is saved.
        let saves = p.saves;
        let form = b"hostname=livingroom&ssid=Other&wifi_password=&setpoint=21.5&hysteresis=0.3";
        let r = post(&mut p, "/settings", &cookie, form);
        assert!(r.text().contains("Enter the WiFi password"));
        assert_eq!(p.saves, saves);

        let form = b"hostname=bad%20name&ssid=&setpoint=21.5&hysteresis=0.3";
        assert!(
            post(&mut p, "/settings", &cookie, form)
                .text()
                .contains("Hostname")
        );
        let form = b"hostname=x&ssid=&setpoint=99&hysteresis=0.3";
        assert!(
            post(&mut p, "/settings", &cookie, form)
                .text()
                .contains("between 5.0")
        );
    }

    #[test]
    fn password_change_and_basic_auth() {
        let (mut p, cookie) = provisioned();
        let form = b"hostname=t&ssid=&setpoint=20.0&hysteresis=0.5&new_password=newsecret1&new_password2=newsecret1";
        assert!(
            post(&mut p, "/settings", &cookie, form)
                .text()
                .contains("Saved")
        );
        let basic = |user_pass: &str| {
            let mut s = StdString::new();
            // Tiny base64 encoder for the test.
            const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            for c in user_pass.as_bytes().chunks(3) {
                let n = (c[0] as u32) << 16
                    | (*c.get(1).unwrap_or(&0) as u32) << 8
                    | *c.get(2).unwrap_or(&0) as u32;
                for i in 0..4 {
                    s.push(if i <= c.len() {
                        T[(n >> (18 - 6 * i) & 63) as usize] as char
                    } else {
                        '='
                    });
                }
            }
            s
        };
        let req = |auth: &str| {
            std::format!(
                "POST /api/reboot HTTP/1.1\r\nAuthorization: Basic {}\r\nContent-Length: 0\r\n\r\n",
                basic(auth)
            )
        };
        let r = send(&mut p, req("admin:secret123").into_bytes());
        assert_eq!(r.status, 401);
        assert!(!p.reboot);
        let r = send(&mut p, req("admin:newsecret1").into_bytes());
        assert_eq!(r.status, 200);
        assert!(p.reboot);
    }

    #[test]
    fn firmware_upload() {
        let (mut p, cookie) = provisioned();
        let image: Vec<u8> = (0..20_000u32).map(|i| (i * 7) as u8).collect();
        let r = post(&mut p, "/api/update", &cookie, &image);
        assert_eq!(r.status, 200, "{}", r.text());
        assert!(r.text().contains("\"version\":\"new\""));
        assert_eq!(p.ota.written, image);
        assert!(p.reboot);

        let mut bad = image.clone();
        bad[0] = 0xEE;
        p.reboot = false;
        let r = post(&mut p, "/api/update?reboot=0", &cookie, &bad);
        assert_eq!(r.status, 400);
        assert!(r.text().contains("verification"));
        assert!(!p.reboot);

        let r = post(&mut p, "/api/update", "", &image);
        assert_eq!(r.status, 401);
    }

    #[test]
    fn page_fits_buffer() {
        let (mut p, cookie) = provisioned();
        p.settings.hostname = crate::settings::str32(&"<".repeat(32));
        for path in ["/", "/settings", "/update"] {
            let r = get(&mut p, path, &cookie);
            assert!(r.text().ends_with("</html>"), "{path} truncated");
            assert!(!r.text().contains("<<"));
        }
        // Worst case: a dual board with four named sensors, all heard nearby.
        p.zone_count = 2;
        p.zone_status = std::vec![Default::default(); 2];
        let mut t = crate::ble::BleTable::new();
        for i in 0..crate::settings::MAX_TEMP_SENSORS as u8 {
            let addr = [0xC0 | i; 6];
            t.inject(addr, 2000, 0);
            let _ = p.settings.temp_sensors.push(crate::settings::TempSensor {
                addr,
                setpoint_dc: 200,
                name: crate::settings::SensorName::try_from("<".repeat(16).as_str()).unwrap(),
                zone: i % 2,
            });
        }
        p.ble = Some(t);
        p.settings.zone_names = core::array::from_fn(|_| {
            crate::settings::ZoneName::try_from("<".repeat(16).as_str()).unwrap()
        });
        for path in ["/", "/settings"] {
            let r = get(&mut p, path, &cookie);
            assert!(
                r.text().ends_with("</html>"),
                "dual {path} truncated ({} bytes)",
                r.text().len()
            );
        }
    }

    #[test]
    fn dual_zone_settings_and_status() {
        use crate::platform::ZoneStatus;
        let (mut p, cookie) = provisioned();
        p.zone_count = 2;
        p.settings.heater_id = crate::settings::str32("leftover");
        let mut t = crate::ble::BleTable::new();
        let (a, b) = ([0xA1; 6], [0xB2; 6]);
        t.inject(a, 1900, 1000);
        t.inject(b, 2200, 1000);
        p.ble = Some(t);

        let html = get(&mut p, "/settings", &cookie).text();
        assert!(html.contains("<legend>Zones</legend>") && html.contains("name=zone1"));
        assert!(
            !html.contains("Heater group"),
            "no heater group on a dual board"
        );
        assert!(html.contains("<select name=zn_A1A1A1A1A1A1><option value=0 selected>Zone 1"));

        let base = "hostname=t&ssid=&setpoint=20.0&hysteresis=0.5&heater_id=boiler";
        let form = std::format!(
            "{base}&zone0=Downstairs&zone1=+Up+stairs+&sensor=A1%3AA1%3AA1%3AA1%3AA1%3AA1\
&sensor=B2%3AB2%3AB2%3AB2%3AB2%3AB2&zn_B2B2B2B2B2B2=1"
        );
        assert!(
            post(&mut p, "/settings", &cookie, form.as_bytes())
                .text()
                .contains("Saved")
        );
        assert_eq!(p.settings.zone_names[0], "Downstairs");
        assert_eq!(p.settings.zone_names[1], "Up stairs");
        assert!(
            p.settings.heater_id.is_empty(),
            "heater groups are off on dual boards"
        );
        let zones: std::vec::Vec<_> = p.settings.temp_sensors.iter().map(|t| t.zone).collect();
        assert_eq!(zones, [0, 1]);
        let html = get(&mut p, "/settings", &cookie).text();
        assert!(
            html.contains("<option value=1 selected>Up stairs</option>"),
            "{html}"
        );
        // A zone this board does not have is refused.
        let bad = std::format!("{base}&sensor=A1%3AA1%3AA1%3AA1%3AA1%3AA1&zn_A1A1A1A1A1A1=2");
        assert!(
            post(&mut p, "/settings", &cookie, bad.as_bytes())
                .text()
                .contains("Invalid zone")
        );
        let long = std::format!("{base}&zone0={}", "x".repeat(17));
        assert!(
            post(&mut p, "/settings", &cookie, long.as_bytes())
                .text()
                .contains("at most 16")
        );

        // Status: one row per zone with its sensors; JSON lists the zones.
        p.zone_status = std::vec![
            ZoneStatus {
                heating: true,
                reason: "a sensor is below its setpoint",
                hold_remaining_s: None,
            },
            ZoneStatus {
                heating: true,
                reason: "keeping the heater on for its minimum on time",
                hold_remaining_s: Some(600),
            },
        ];
        let readings =
            crate::control::readings(p.ble.as_ref().unwrap(), &p.settings.temp_sensors, 2000);
        p.readings = readings;
        let html = get(&mut p, "/", &cookie).text();
        let down = html.find("<tr><td>Downstairs</td>").expect("zone row");
        let up = html.find("<tr><td>Up stairs</td>").expect("zone row");
        assert!(down < up);
        assert!(html[down..up].contains("<code>A1:A1:A1:A1:A1:A1</code> <b>19.0 °C</b>"));
        assert!(html[up..].contains("0 h 10 min to go") && html[up..].contains("22.0 °C"));
        let json = get(&mut p, "/api/status", "").text();
        assert!(
            json.contains("\"zones\":[{\"name\":\"Downstairs\",\"heating\":true,"),
            "{json}"
        );
        assert!(json.contains("{\"name\":\"Up stairs\",\"heating\":true,\"reason\":\"keeping the heater on for its minimum on time\",\"hold_remaining_s\":600}"));
        assert!(json.contains("\"addr\":\"B2:B2:B2:B2:B2:B2\",\"name\":\"\",\"zone\":1,"));
        // Unnamed zones fall back to "Zone N".
        p.settings.zone_names[1].clear();
        assert!(
            get(&mut p, "/api/status", "")
                .text()
                .contains("{\"name\":\"Zone 2\"")
        );
    }

    #[test]
    fn ble_pages() {
        let (mut p, cookie) = provisioned();
        assert_eq!(get(&mut p, "/ble", "").status, 303, "login required");
        assert_eq!(get(&mut p, "/api/ble", "").status, 401);
        assert!(
            get(&mut p, "/ble", &cookie)
                .text()
                .contains("no Bluetooth radio")
        );
        assert_eq!(
            get(&mut p, "/api/ble", &cookie).text(),
            "{\"supported\":false}"
        );

        let mut t = crate::ble::BleTable::new();
        t.scanning = true;
        // Two pvvx thermometers (one with an HTML-ish name) and a phone.
        let mut thermo = std::vec![2, 0x01, 0x06, 18, 0x16, 0x1A, 0x18, 1, 2, 3, 4, 5, 6];
        thermo.extend_from_slice(&2137i16.to_le_bytes());
        thermo.extend_from_slice(&4567u16.to_le_bytes());
        thermo.extend_from_slice(&[0x86, 0x0B, 87, 1, 0]);
        t.observe([6, 5, 4, 3, 2, 1], 0, -55, &thermo, 500);
        let mut named = thermo.clone();
        named.extend_from_slice(&[5, 0x09]);
        named.extend_from_slice(b"<b\"x");
        t.observe([9, 9, 9, 9, 9, 0xC9], 1, -80, &named, 900);
        let phone = [4, 0xFF, 0x4C, 0x00, 0x10];
        t.observe([7, 7, 7, 7, 7, 0x47], 1, -40, &phone, 950); // not a sensor: dropped
        p.ble = Some(t);

        let html = get(&mut p, "/ble", &cookie).text();
        assert!(html.contains("2 temperature sensors"), "{html}");
        assert!(html.contains("3 advertisements received, 1 of them from other devices"));
        assert!(!html.contains("47:07:07"));
        assert!(html.contains("01:02:03:04:05:06"));
        assert!(html.contains("21.3 °C") && html.contains("45.6 %") && html.contains("87%"));
        assert!(html.contains("&lt;b&quot;x") && !html.contains("<b\"x"));
        assert!(html.ends_with("</html>"));
        // Strongest signal first.
        assert!(html.find("01:02:03:04:05:06").unwrap() < html.find("C9:09:09:09:09:09").unwrap());

        let json = get(&mut p, "/api/ble", &cookie).text();
        assert!(
            json.starts_with("{\"supported\":true,\"scanning\":true,\"reports\":3,\"ignored\":1,"),
            "{json}"
        );
        assert!(json.contains("\"temperature\":21.3") && json.contains("\"battery\":87"));
        assert!(json.contains("\"name\":\"<b\\\"x\""), "{json}");
    }

    #[test]
    fn full_ble_table_fits_page() {
        let (mut p, cookie) = provisioned();
        let mut t = crate::ble::BleTable::new();
        for i in 0..crate::ble::MAX_DEVICES as u8 {
            let mut d = std::vec![25, 0x09];
            d.extend_from_slice(&[b'N'; 24]);
            d.extend_from_slice(&[9, 0x03, 0x0F, 0x18, 0x1A, 0x18, 0x0A, 0x18, 0x0D, 0x18]);
            d.extend_from_slice(&[3, 0xFF, 0x34, 0x12]);
            t.observe([i, 0, 0, 0, 0, 0], 1, -100, &d, 0);
        }
        p.ble = Some(t);
        let html = get(&mut p, "/ble", &cookie).text();
        assert!(
            html.ends_with("</html>"),
            "page truncated at {} bytes",
            html.len()
        );
        assert!(get(&mut p, "/api/ble", &cookie).text().ends_with("]}"));
    }

    #[test]
    fn select_temperature_sensors() {
        let (mut p, cookie) = provisioned();
        let mut t = crate::ble::BleTable::new();
        let tp = [0xFF, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA];
        t.observe(
            tp,
            0,
            -60,
            &[7, 0xFF, 0xC2, 0xF1, 0x00, 0x1D, 0x02, 0x2C],
            1000,
        );
        // A later packet with only the name is merged into the known sensor.
        t.observe(
            tp,
            0,
            -60,
            &[
                13, 0x09, b'T', b'P', b'3', b'5', b'7', b' ', b'(', b'2', b'1', b'4', b'2', b')',
            ],
            1000,
        );
        t.observe([1; 6], 1, -40, &[4, 0xFF, 0x4C, 0x00, 0x10], 1000); // no reading: not kept
        p.ble = Some(t);

        let html = get(&mut p, "/settings", &cookie).text();
        assert!(
            html.contains("name=sensor value=AA:BB:CC:DD:EE:FF>"),
            "{html}"
        );
        assert!(html.contains("TP357 (2142)") && html.contains("24.1 °C"));
        assert!(!html.contains("01:01:01:01:01:01"));

        let base = "hostname=t&ssid=&setpoint=20.0&hysteresis=0.5";
        let form = std::format!("{base}&sensor=AA%3ABB%3ACC%3ADD%3AEE%3AFF");
        assert!(
            post(&mut p, "/settings", &cookie, form.as_bytes())
                .text()
                .contains("Saved")
        );
        // No setpoint field: the default setpoint.
        let sensor = |setpoint_dc| crate::settings::TempSensor {
            addr: tp,
            setpoint_dc,
            name: Default::default(),
            zone: 0,
        };
        assert_eq!(p.settings.temp_sensors.as_slice(), &[sensor(200)]);
        assert!(
            !html.contains("name=zn_"),
            "no zone selector on one-zone boards"
        );
        let html = get(&mut p, "/settings", &cookie).text();
        assert!(html.contains("value=AA:BB:CC:DD:EE:FF checked"));
        assert!(html.contains("name=sp_AABBCCDDEEFF value=20.0"), "{html}");
        // Its own setpoint, then kept when a form omits it.
        let form = std::format!("{base}&sensor=AA%3ABB%3ACC%3ADD%3AEE%3AFF&sp_AABBCCDDEEFF=18.5");
        post(&mut p, "/settings", &cookie, form.as_bytes());
        assert_eq!(p.settings.temp_sensors.as_slice(), &[sensor(185)]);
        let form = std::format!("{base}&sensor=AA%3ABB%3ACC%3ADD%3AEE%3AFF");
        post(&mut p, "/settings", &cookie, form.as_bytes());
        assert_eq!(p.settings.temp_sensors.as_slice(), &[sensor(185)]);
        assert!(
            get(&mut p, "/settings", &cookie)
                .text()
                .contains("name=sp_AABBCCDDEEFF value=18.5")
        );
        let form = std::format!("{base}&sensor=AA%3ABB%3ACC%3ADD%3AEE%3AFF&sp_AABBCCDDEEFF=40");
        assert!(
            post(&mut p, "/settings", &cookie, form.as_bytes())
                .text()
                .contains("between 5.0 and 35.0")
        );
        let status = get(&mut p, "/", &cookie).text();
        assert!(
            status.contains("no fresh reading · setpoint 18.5 °C"),
            "{status}"
        );
        // A name of our own, kept when the form omits it.
        let named = crate::settings::TempSensor {
            name: crate::settings::SensorName::try_from("Landing <1>").unwrap(),
            ..sensor(185)
        };
        let form = std::format!(
            "{base}&sensor=AA%3ABB%3ACC%3ADD%3AEE%3AFF&nm_AABBCCDDEEFF=+Landing+%3C1%3E"
        );
        post(&mut p, "/settings", &cookie, form.as_bytes());
        assert_eq!(
            p.settings.temp_sensors.as_slice(),
            std::slice::from_ref(&named)
        );
        let form = std::format!("{base}&sensor=AA%3ABB%3ACC%3ADD%3AEE%3AFF");
        post(&mut p, "/settings", &cookie, form.as_bytes());
        assert_eq!(
            p.settings.temp_sensors.as_slice(),
            std::slice::from_ref(&named)
        );
        assert!(
            get(&mut p, "/settings", &cookie)
                .text()
                .contains("name=nm_AABBCCDDEEFF value=\"Landing &lt;1&gt;\"")
        );
        assert!(
            get(&mut p, "/", &cookie)
                .text()
                .contains("<b>Landing &lt;1&gt;</b> <code>AA:BB:CC:DD:EE:FF</code>")
        );
        let form = std::format!(
            "{base}&sensor=AA%3ABB%3ACC%3ADD%3AEE%3AFF&nm_AABBCCDDEEFF={}",
            "x".repeat(17)
        );
        assert!(
            post(&mut p, "/settings", &cookie, form.as_bytes())
                .text()
                .contains("at most 16 characters")
        );
        // Clearing the name.
        let form = std::format!("{base}&sensor=AA%3ABB%3ACC%3ADD%3AEE%3AFF&nm_AABBCCDDEEFF=");
        post(&mut p, "/settings", &cookie, form.as_bytes());
        assert_eq!(p.settings.temp_sensors.as_slice(), &[sensor(185)]);

        // A selected sensor that is out of range stays listed (and selected).
        p.ble = Some(crate::ble::BleTable::new());
        assert!(
            get(&mut p, "/settings", &cookie)
                .text()
                .contains("not heard recently")
        );

        let too_many: std::string::String = (1..=crate::settings::MAX_TEMP_SENSORS + 1)
            .map(|i| std::format!("&sensor={i:02X}%3A00%3A00%3A00%3A00%3A00"))
            .collect();
        let r = post(
            &mut p,
            "/settings",
            &cookie,
            std::format!("{base}{too_many}").as_bytes(),
        );
        assert!(r.text().contains("at most 8"));
        assert!(
            post(
                &mut p,
                "/settings",
                &cookie,
                std::format!("{base}&sensor=junk").as_bytes()
            )
            .text()
            .contains("Invalid sensor address")
        );
        assert_eq!(
            p.settings.temp_sensors.as_slice(),
            &[sensor(185)],
            "rejected forms change nothing"
        );
        // Unchecking everything clears the selection.
        post(&mut p, "/settings", &cookie, base.as_bytes());
        assert!(p.settings.temp_sensors.is_empty());

        let status = get(&mut p, "/", &cookie).text();
        assert!(status.contains("no sensors selected") && status.contains("Heating"));
        let json = get(&mut p, "/api/status", "").text();
        assert!(
            json.contains("\"heating\":false") && json.contains("\"sensors_used\":0"),
            "{json}"
        );
    }

    #[test]
    fn time_settings_and_display() {
        let (mut p, cookie) = provisioned();
        let base = "hostname=t&ssid=&setpoint=20.0&hysteresis=0.5";
        let html = get(&mut p, "/", &cookie).text();
        assert!(html.contains("not synchronised yet") && html.contains("pool.ntp.org"));
        assert!(
            get(&mut p, "/api/status", "")
                .text()
                .contains("\"local_time\":null")
        );

        let form = std::format!(
            "{base}&ntp_server=10.0.2.2%3A12345&timezone=GMT0BST%2CM3.5.0%2F1%2CM10.5.0"
        );
        assert!(
            post(&mut p, "/settings", &cookie, form.as_bytes())
                .text()
                .contains("Saved")
        );
        assert_eq!(p.settings.ntp_server, "10.0.2.2:12345");
        assert_eq!(p.settings.timezone, "GMT0BST,M3.5.0/1,M10.5.0");
        // 2030-07-01T12:00:00Z is 13:00 BST.
        p.time = Some(1_909_137_600);
        let json = get(&mut p, "/api/status", "").text();
        assert!(
            json.contains("\"local_time\":\"2030-07-01T13:00:00+01:00\""),
            "{json}"
        );
        assert!(
            get(&mut p, "/", &cookie)
                .text()
                .contains("2030-07-01 13:00:00 <small>(UTC+01:00, DST")
        );

        for (field, msg) in [
            ("timezone=Mars", "POSIX TZ"),
            ("ntp_server=bad%20host", "NTP server"),
        ] {
            let r = post(
                &mut p,
                "/settings",
                &cookie,
                std::format!("{base}&{field}").as_bytes(),
            );
            assert!(r.text().contains(msg), "{field}");
        }
        // Forms without the fields keep the stored values.
        post(&mut p, "/settings", &cookie, base.as_bytes());
        assert_eq!(p.settings.timezone, "GMT0BST,M3.5.0/1,M10.5.0");
    }

    #[test]
    fn short_cycle_setting() {
        let (mut p, cookie) = provisioned();
        assert_eq!(p.settings.min_on_min, 240, "default is 4 hours");
        let html = get(&mut p, "/settings", &cookie).text();
        assert!(html.contains("name=min_on value=\"240\""), "{html}");
        let base = "hostname=t&ssid=&setpoint=20.0&hysteresis=1.5";
        assert!(
            post(
                &mut p,
                "/settings",
                &cookie,
                std::format!("{base}&min_on=90").as_bytes()
            )
            .text()
            .contains("Saved")
        );
        assert_eq!((p.settings.min_on_min, p.settings.hysteresis_dc), (90, 15));
        for bad in ["1441", "-5", "abc"] {
            let r = post(
                &mut p,
                "/settings",
                &cookie,
                std::format!("{base}&min_on={bad}").as_bytes(),
            );
            assert!(r.text().contains("0 to 1440"), "{bad}");
        }
        post(&mut p, "/settings", &cookie, base.as_bytes());
        assert_eq!(p.settings.min_on_min, 90, "omitted field keeps the value");

        p.hold = Some(83 * 60 + 10);
        assert!(
            get(&mut p, "/", &cookie)
                .text()
                .contains("1 h 24 min to go")
        );
        let json = get(&mut p, "/api/status", "").text();
        assert!(
            json.contains("\"min_on_min\":90") && json.contains("\"hold_remaining_s\":4990"),
            "{json}"
        );
    }

    #[test]
    fn heater_group_api() {
        let (mut p, cookie) = provisioned();
        assert_eq!(get(&mut p, "/api/zone", "").text(), "{\"heater\":null}");
        let base = "hostname=bedroom&ssid=&setpoint=20.0&hysteresis=0.5";
        let form = std::format!("{base}&heater_id=boiler&group_key=correct-horse");
        assert!(
            post(&mut p, "/settings", &cookie, form.as_bytes())
                .text()
                .contains("Saved")
        );
        assert_eq!(
            (p.settings.heater_id.as_str(), p.settings.group_key.as_str()),
            ("boiler", "correct-horse")
        );
        let html = get(&mut p, "/settings", &cookie).text();
        assert!(
            html.contains("name=heater_id value=\"boiler\"") && !html.contains("correct-horse")
        );
        // Empty key field keeps the key; the checkbox removes it.
        post(
            &mut p,
            "/settings",
            &cookie,
            std::format!("{base}&heater_id=boiler&group_key=").as_bytes(),
        );
        assert_eq!(p.settings.group_key, "correct-horse");
        assert!(
            post(
                &mut p,
                "/settings",
                &cookie,
                std::format!("{base}&heater_id=bad%20id").as_bytes()
            )
            .text()
            .contains("Heater id")
        );
        assert!(
            post(
                &mut p,
                "/settings",
                &cookie,
                std::format!("{base}&heater_id=boiler&group_key=short").as_bytes()
            )
            .text()
            .contains("at least 8")
        );

        // A signed event from the kitchen zone, no login needed.
        p.time = Some(1_800_000_000);
        let msg = crate::zones::ZoneMessage {
            heater: crate::settings::str32("boiler"),
            zone: crate::settings::str32("kitchen"),
            heat: true,
            heater_on_since: Some(1_799_990_000),
            ts: Some(1_800_000_000),
        };
        let r = post(
            &mut p,
            "/api/zone/event",
            "",
            msg.encode("correct-horse").as_bytes(),
        );
        assert_eq!(r.status, 200, "{}", r.text());
        // The reply is our own signed state.
        let reply = crate::zones::ZoneMessage::decode(
            &r.text(),
            "boiler",
            "kitchen",
            "correct-horse",
            Some(1_800_000_000),
        )
        .unwrap();
        assert_eq!(
            (reply.zone.as_str(), reply.heat, reply.heater_on_since),
            ("bedroom", false, Some(1_799_990_000))
        );
        assert!(p.group.others_heating(1000));
        assert_eq!(
            post(
                &mut p,
                "/api/zone/event",
                "",
                msg.encode("wrong-key!").as_bytes()
            )
            .status,
            403
        );
        assert_eq!(p.group.rejected, 1);

        let json = get(&mut p, "/api/zone", "").text();
        assert!(json.starts_with("{\"heater\":\"boiler\",\"zone\":\"bedroom\",\"heat\":false,\"heater_on_since\":1799990000,\"rejected\":1,\"peers\":[{\"zone\":\"kitchen\",\"addr\":null,\"heat\":true,\"active\":true"), "{json}");
        assert!(
            get(&mut p, "/", &cookie)
                .text()
                .contains("kitchen (heating)")
        );

        let r = post(
            &mut p,
            "/api/test/sensor",
            &cookie,
            b"addr=01%3A02%3A03%3A04%3A05%3A06&temp=19.5",
        );
        assert_eq!(r.status, 200);
        assert_eq!(p.injected, Some(([6, 5, 4, 3, 2, 1], 1950)));
        assert_eq!(post(&mut p, "/api/test/sensor", "", b"addr=x").status, 401);
    }
}
