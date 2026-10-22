//! End-to-end tests against the real firmware running in (patched) QEMU.
//!
//! One emulated device goes through the whole lifecycle: first-boot setup,
//! settings, HTTP update with trial boot and confirmation, rejection of a
//! corrupt image, automatic rollback of an image that never confirms itself,
//! and an update over the serial console protocol.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::{BuildArgs, mdns, ota, qemu, workspace_root};

const PASSWORD: &str = "e2e-secret-pw";

struct Image {
    img: PathBuf,
    flash: PathBuf,
}

fn build_variant(dir: &Path, release: bool, version: &str, features: &str) -> Result<Image> {
    eprintln!("== building firmware {version}");
    let a = crate::build(&BuildArgs {
        release,
        target: "qemu".into(),
        version: Some(version.into()),
        features: features.into(),
    })?;
    let img = dir.join(format!("{version}.img"));
    let flash = dir.join(format!("{version}-flash.bin"));
    std::fs::copy(&a.app, &img)?;
    std::fs::copy(&a.merged, &flash)?;
    Ok(Image { img, flash })
}

/// Unix time served by the test SNTP server at start-up: 2030-07-01T12:00:00Z,
/// a date when UK summer time applies.
const SNTP_EPOCH: i64 = 1_909_137_600;

/// A tiny SNTP server on localhost for the emulated device to sync from
/// (QEMU's NAT exposes the host as 10.0.2.2). Its clock starts at
/// `SNTP_EPOCH` and advances in real time.
fn start_sntp_server() -> Result<u16> {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0")?;
    let port = sock.local_addr()?.port();
    let start = Instant::now();
    std::thread::spawn(move || {
        let mut buf = [0u8; 128];
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            let now_us = SNTP_EPOCH * 1_000_000 + start.elapsed().as_micros() as i64;
            if let Some(reply) = thermostat_core::sntp::server_reply(&buf[..n], now_us) {
                let _ = sock.send_to(&reply, from);
            }
        }
    });
    Ok(port)
}

fn free_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// Console bytes, kept twice: the full log for pattern matching and a queue
/// drained by the serial protocol client.
#[derive(Default)]
struct ConsoleBuf {
    log: Vec<u8>,
    queue: VecDeque<u8>,
}

struct Emulator {
    child: Child,
    console: Arc<Mutex<ConsoleBuf>>,
    writer: TcpStream,
    http: String,
    log_path: PathBuf,
    tap: Option<qemu::FrameTap>,
}

impl Emulator {
    fn start(flash: &Path, log_path: &Path) -> Result<Self> {
        let http_port = free_port()?;
        let serial_port = free_port()?;
        let tap = qemu::FrameTap {
            local: free_port()?,
            remote: free_port()?,
        };
        let cmd = qemu::command(
            flash,
            http_port,
            &qemu::Console::Tcp(serial_port),
            Some(&tap),
        )?;
        Self::spawn(
            cmd,
            serial_port,
            log_path,
            format!("127.0.0.1:{http_port}"),
            Some(tap),
        )
    }

    /// Start QEMU from a prepared command whose console is a TCP server on `serial_port`.
    fn spawn(
        mut cmd: std::process::Command,
        serial_port: u16,
        log_path: &Path,
        http: String,
        tap: Option<qemu::FrameTap>,
    ) -> Result<Self> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = cmd.spawn().context("starting qemu")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match TcpStream::connect(("127.0.0.1", serial_port)) {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100))
                }
                Err(e) => return Err(e).context("connecting to qemu console"),
            }
        };
        let console = Arc::new(Mutex::new(ConsoleBuf::default()));
        let mut reader = stream.try_clone()?;
        let sink = console.clone();
        let mut file = std::fs::File::create(log_path)?;
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let _ = file.write_all(&buf[..n]);
                let mut c = sink.lock().unwrap();
                c.log.extend_from_slice(&buf[..n]);
                c.queue.extend(&buf[..n]);
            }
        });
        Ok(Self {
            child,
            console,
            writer: stream,
            http,
            log_path: log_path.to_path_buf(),
            tap,
        })
    }

    fn mark(&self) -> usize {
        self.console.lock().unwrap().log.len()
    }

    /// Console output since `mark`.
    fn log_since(&self, mark: usize) -> String {
        let c = self.console.lock().unwrap();
        String::from_utf8_lossy(&c.log[mark.min(c.log.len())..]).into_owned()
    }

    /// Wait for `pattern` to appear in the console output after `mark`.
    fn wait_log(&self, mark: usize, pattern: &str, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let c = self.console.lock().unwrap();
                if String::from_utf8_lossy(&c.log[mark.min(c.log.len())..]).contains(pattern) {
                    return Ok(());
                }
            }
            if Instant::now() > deadline {
                bail!(
                    "timed out waiting for {pattern:?} on the console (see {})",
                    self.log_path.display()
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<(u16, String)> {
        ota::http_request(&self.http, method, path, headers, body)
    }

    fn status(&self) -> Result<Value> {
        let (code, body) = self.request("GET", "/api/status", &[], b"")?;
        if code != 200 {
            bail!("status returned {code}");
        }
        Ok(serde_json::from_str(&body)?)
    }

    /// Poll the status API until `pred` holds (the device may be rebooting).
    fn wait_status(
        &self,
        what: &str,
        timeout: Duration,
        pred: impl Fn(&Value) -> bool,
    ) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        let mut last = None;
        loop {
            if let Ok(s) = self.status() {
                if pred(&s) {
                    return Ok(s);
                }
                last = Some(s);
            }
            if Instant::now() > deadline {
                bail!(
                    "timed out waiting for {what}; last status {last:?} (see {})",
                    self.log_path.display()
                );
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn mdns(&self, name: &str, qtype: u16) -> Result<mdns::Response> {
        let tap = self.tap.as_ref().context("no frame tap on this emulator")?;
        mdns::query(tap.remote, tap.local, name, qtype, Duration::from_secs(10))
    }

    /// The device must resolve as `<hostname>.local` and advertise its services.
    fn check_mdns(&self, hostname: &str, version: &str) -> Result<()> {
        let fqdn = format!("{hostname}.local");
        let r = self.mdns(&fqdn, mdns::TYPE_A)?;
        let addrs: Vec<_> = r
            .answers
            .iter()
            .filter(|a| a.name == fqdn)
            .map(|a| &a.data)
            .collect();
        anyhow::ensure!(
            addrs == [&mdns::Data::A([10, 0, 2, 15].into())],
            "expected exactly one A record 10.0.2.15 for {fqdn}, got {r:?}"
        );

        let r = self.mdns("_thermostat._tcp.local", mdns::TYPE_PTR)?;
        let instance = format!("{hostname}._thermostat._tcp.local");
        anyhow::ensure!(
            r.answers
                .iter()
                .any(|a| a.data == mdns::Data::Ptr(instance.clone())),
            "no PTR to {instance}: {r:?}"
        );
        anyhow::ensure!(
            r.all().any(|a| a.name == instance
                && a.data
                    == mdns::Data::Srv {
                        port: 80,
                        target: fqdn.clone()
                    }),
            "no SRV {instance} -> {fqdn}:80: {r:?}"
        );
        let want = format!("version={version}");
        anyhow::ensure!(
            r.all().any(|a| a.name == instance
                && matches!(&a.data, mdns::Data::Txt(t) if t.contains(&want))),
            "no TXT with {want}: {r:?}"
        );
        let r = self.mdns("_http._tcp.local", mdns::TYPE_PTR)?;
        anyhow::ensure!(
            r.answers
                .iter()
                .any(|a| a.data == mdns::Data::Ptr(format!("{hostname}._http._tcp.local"))),
            "no _http._tcp service: {r:?}"
        );
        Ok(())
    }

    fn port(&self) -> Result<ConsolePort> {
        Ok(ConsolePort {
            console: self.console.clone(),
            writer: self.writer.try_clone()?,
        })
    }
}

impl Drop for Emulator {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The emulated UART as a `Read + Write` port for the serial update client.
struct ConsolePort {
    console: Arc<Mutex<ConsoleBuf>>,
    writer: TcpStream,
}

impl Read for ConsolePort {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let deadline = Instant::now() + Duration::from_millis(50);
        loop {
            {
                let mut c = self.console.lock().unwrap();
                if !c.queue.is_empty() {
                    let n = buf.len().min(c.queue.len());
                    for (i, b) in c.queue.drain(..n).enumerate() {
                        buf[i] = b;
                    }
                    return Ok(n);
                }
            }
            if Instant::now() > deadline {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Write for ConsolePort {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writer.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

fn field<'a>(s: &'a Value, k: &str) -> &'a str {
    s.get(k).and_then(Value::as_str).unwrap_or("")
}

fn step(name: &str, f: impl FnOnce() -> Result<()>) -> Result<()> {
    let t = Instant::now();
    eprintln!("-- {name}");
    f().with_context(|| format!("step '{name}' failed"))?;
    eprintln!("   ok ({:.1}s)", t.elapsed().as_secs_f32());
    Ok(())
}

pub fn run(release: bool, zones_only: bool) -> Result<()> {
    let dir = workspace_root().join("target/e2e");
    std::fs::create_dir_all(&dir)?;
    qemu::binary()?;

    if zones_only {
        let v3 = build_variant(&dir, release, "e2e-3", "")?;
        let sntp_port = start_sntp_server()?;
        run_zones(&dir, &v3.flash, sntp_port)?;
        eprintln!("zone tests passed");
        return Ok(());
    }
    let v1 = build_variant(&dir, release, "e2e-1", "")?;
    let v2 = build_variant(&dir, release, "e2e-2", "")?;
    let v3 = build_variant(&dir, release, "e2e-3", "")?;
    let bad = build_variant(&dir, release, "e2e-bad", "fail-selftest")?;

    let flash = dir.join("device-flash.bin");
    std::fs::copy(&v1.flash, &flash)?;
    let emu = Emulator::start(&flash, &dir.join("console.log"))?;
    let basic = format!(
        "Basic {}",
        ota::base64(format!("admin:{PASSWORD}").as_bytes())
    );
    let auth = [("Authorization", basic.as_str())];
    let started = Instant::now();

    step("first boot through embassy-boot-esp", || {
        emu.wait_log(0, "embassy-boot-esp", Duration::from_secs(20))?;
        let s = emu.wait_status("web server", Duration::from_secs(60), |s| {
            field(s, "version") == "e2e-1"
        })?;
        anyhow::ensure!(field(&s, "boot") == "booted", "unexpected boot state {s}");
        anyhow::ensure!(
            s["provisioned"] == false,
            "fresh device should be unprovisioned"
        );
        Ok(())
    })?;

    step("mDNS: thermostat.local and DNS-SD services", || {
        emu.check_mdns("thermostat", "e2e-1")
    })?;

    step("first-time setup and auth", || {
        let (code, _) = emu.request("GET", "/settings", &[], b"")?;
        anyhow::ensure!(
            code == 303,
            "settings should redirect before setup, got {code}"
        );
        let form = format!("user=admin&password={PASSWORD}&password2={PASSWORD}");
        let (code, _) = emu.request("POST", "/setup", &[], form.as_bytes())?;
        anyhow::ensure!(code == 303, "setup returned {code}");
        let (code, _) = emu.request(
            "POST",
            "/api/reboot?x",
            &[("Authorization", "Basic YWRtaW46d3Jvbmc=")],
            b"",
        )?;
        anyhow::ensure!(code == 401, "wrong password must be rejected, got {code}");
        Ok(())
    })?;

    step("settings are saved to flash", || {
        let form =
            b"hostname=e2e-host&ssid=&setpoint=21.5&hysteresis=0.4&new_password=&new_password2=";
        let (code, body) = emu.request("POST", "/settings", &auth, form)?;
        anyhow::ensure!(
            code == 200 && body.contains("Saved"),
            "settings: {code} {body}"
        );
        let s = emu.status()?;
        anyhow::ensure!(s["setpoint"] == 21.5, "setpoint not applied: {s}");
        Ok(())
    })?;

    let sntp_port = start_sntp_server()?;
    step("NTP time sync with UK timezone", || {
        let form = format!(
            "hostname=e2e-host&ssid=&setpoint=21.5&hysteresis=0.4&ntp_server=10.0.2.2%3A{sntp_port}\
             &timezone=GMT0BST%2CM3.5.0%2F1%2CM10.5.0"
        );
        let (code, body) = emu.request("POST", "/settings", &auth, form.as_bytes())?;
        anyhow::ensure!(
            code == 200 && body.contains("Saved"),
            "settings: {code} {body}"
        );
        // 2030-07-01T12:00Z served by our SNTP server is 13:00 BST.
        let s = emu.wait_status("NTP sync", Duration::from_secs(60), |s| {
            field(s, "local_time").starts_with("2030-07-01T13:0")
        })?;
        anyhow::ensure!(
            field(&s, "local_time").ends_with("+01:00"),
            "expected BST: {s}"
        );
        anyhow::ensure!(
            s["time"]
                .as_i64()
                .is_some_and(|t| (t - SNTP_EPOCH).abs() < 600),
            "unix time: {s}"
        );
        Ok(())
    })?;

    step("HTTP update: trial boot, then confirmation", || {
        let mark = emu.mark();
        let (code, body) = emu.request("POST", "/api/update", &auth, &std::fs::read(&v2.img)?)?;
        anyhow::ensure!(
            code == 200 && body.contains("e2e-2"),
            "upload: {code} {body}"
        );
        emu.wait_log(mark, "swapped (trial)", Duration::from_secs(120))?;
        emu.wait_status("trial boot of e2e-2", Duration::from_secs(60), |s| {
            field(s, "version") == "e2e-2" && field(s, "boot") == "trial"
        })?;
        emu.wait_status("confirmation", Duration::from_secs(60), |s| {
            field(s, "boot") == "booted"
        })?;
        Ok(())
    })?;

    step(
        "settings survive the update; time resyncs after reboot",
        || {
            emu.wait_status("NTP resync", Duration::from_secs(60), |s| {
                field(s, "local_time").starts_with("2030-07-01T13:")
            })?;
            let s = emu.status()?;
            anyhow::ensure!(
                s["provisioned"] == true && s["setpoint"] == 21.5,
                "settings lost: {s}"
            );
            Ok(())
        },
    )?;

    step("mDNS advertises the new hostname and version", || {
        emu.check_mdns("e2e-host", "e2e-2")
    })?;

    step("corrupt image is rejected", || {
        let mut img = std::fs::read(&v3.img)?;
        let mid = img.len() / 2;
        img[mid] ^= 0x55;
        let (code, body) = emu.request("POST", "/api/update?reboot=0", &auth, &img)?;
        anyhow::ensure!(
            code == 400 && body.contains("verification"),
            "corrupt upload: {code} {body}"
        );
        anyhow::ensure!(
            field(&emu.status()?, "version") == "e2e-2",
            "device changed version"
        );
        Ok(())
    })?;

    step("unconfirmed image rolls back", || {
        let mark = emu.mark();
        let (code, body) = emu.request("POST", "/api/update", &auth, &std::fs::read(&bad.img)?)?;
        anyhow::ensure!(code == 200, "upload: {code} {body}");
        // Watch the console rather than HTTP: a trial image may also fail by
        // crashing early, before it ever answers a request.
        emu.wait_log(
            mark,
            "state: swapped (trial), image e2e-bad",
            Duration::from_secs(120),
        )?;
        // It never confirms; the watchdog resets it and the bootloader reverts.
        emu.wait_log(mark, "rst:0x7 (TG0WDT_SYS_RESET)", Duration::from_secs(90))?;
        emu.wait_log(
            mark,
            "state: reverted, image e2e-2",
            Duration::from_secs(120),
        )?;
        emu.wait_status("revert to e2e-2", Duration::from_secs(60), |s| {
            field(s, "version") == "e2e-2"
        })?;
        emu.wait_status("re-confirmation", Duration::from_secs(60), |s| {
            field(s, "boot") == "booted"
        })?;
        Ok(())
    })?;

    step("serial update over the console UART", || {
        let mut port = emu.port()?;
        ota::serial_update(&mut port, &std::fs::read(&v3.img)?, true)?;
        emu.wait_status("e2e-3 confirmed", Duration::from_secs(180), |s| {
            field(s, "version") == "e2e-3" && field(s, "boot") == "booted"
        })?;
        Ok(())
    })?;

    drop(emu);
    run_zones(&dir, &v3.flash, sntp_port)?;

    eprintln!(
        "all end-to-end tests passed in {:.0}s",
        started.elapsed().as_secs_f32()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Two zones sharing a heater.

const GROUP_KEY: &str = "e2e-group-key";
const E2E_HEATER: &str = "e2e-heater-7f3a";

/// A provisioned zone: flash with its own settings (hostname, sensor, group).
fn zone_flash(
    dir: &Path,
    base_flash: &Path,
    name: &str,
    sensor: [u8; 6],
    sntp_port: u16,
) -> Result<PathBuf> {
    use thermostat_core::auth::{DEFAULT_ITERATIONS, PasswordHash};
    use thermostat_core::settings::{Settings, str32, str48};
    let settings = Settings {
        hostname: str32(name),
        admin_hash: Some(PasswordHash::new(
            PASSWORD,
            crate::provision::random_salt()?,
            DEFAULT_ITERATIONS,
        )),
        setpoint_dc: 210,
        hysteresis_dc: 4,
        min_on_min: 2,
        temp_sensors: heapless::Vec::from_slice(&[thermostat_core::settings::TempSensor {
            addr: sensor,
            setpoint_dc: 210,
            name: thermostat_core::settings::SensorName::try_from(name).unwrap(),
        }])
        .unwrap(),
        // Unique id: the emulated zones can reach the real LAN through QEMU's
        // NAT (including mDNS), so avoid joining a real heater group.
        heater_id: str32(E2E_HEATER),
        group_key: str32(GROUP_KEY),
        ntp_server: str48(&format!("10.0.2.2:{sntp_port}")),
        ..Settings::default()
    };
    let mut flash = std::fs::read(base_flash)?;
    let part = crate::provision::settings_partition();
    let image = crate::provision::settings_image(&settings)?;
    flash[part.offset as usize..part.offset as usize + image.len()].copy_from_slice(&image);
    let path = dir.join(format!("{name}-flash.bin"));
    std::fs::write(&path, flash)?;
    Ok(path)
}

/// QEMU for one zone: a single NIC on a UDP datagram link to the test switch.
fn zone_command(
    flash: &Path,
    serial_port: u16,
    link: (u16, u16),
    mac: &str,
) -> Result<std::process::Command> {
    let mut cmd = std::process::Command::new(qemu::binary()?);
    cmd.args(["-nographic", "-machine", "esp32"])
        .arg("-drive")
        .arg(format!("file={},if=mtd,format=raw", flash.display()))
        .args(["-netdev", &dgram("d0", link)])
        .args([
            "-net",
            &format!("nic,model=open_eth,netdev=d0,macaddr={mac}"),
            "-monitor",
            "none",
        ])
        .args([
            "-serial",
            &format!("tcp:127.0.0.1:{serial_port},server=on,wait=on"),
        ]);
    Ok(cmd)
}

fn dgram(id: &str, (local, remote): (u16, u16)) -> String {
    format!(
        "dgram,id={id},local.type=inet,local.host=127.0.0.1,local.port={local},\
         remote.type=inet,remote.host=127.0.0.1,remote.port={remote}"
    )
}

/// A guest-less QEMU acting as the router: user-mode NAT (DHCP, DNS, the
/// host's NTP server, HTTP port forwards) on one switch port.
struct Router {
    child: std::process::Child,
    monitor: u16,
}

impl Router {
    fn start(link: (u16, u16)) -> Result<Self> {
        let monitor = free_port()?;
        let child = std::process::Command::new(qemu::binary()?)
            .args(["-machine", "none", "-nographic", "-serial", "none"])
            .args(["-netdev", "user,id=u0", "-netdev", &dgram("d0", link)])
            .args([
                "-netdev",
                "hubport,id=h0,hubid=0,netdev=u0",
                "-netdev",
                "hubport,id=h1,hubid=0,netdev=d0",
            ])
            .args([
                "-monitor",
                &format!("tcp:127.0.0.1:{monitor},server=on,wait=off"),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("starting router qemu")?;
        Ok(Self { child, monitor })
    }
}

impl Drop for Router {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A learning Ethernet switch over UDP: each port is a socket exchanging raw
/// frames with one QEMU `dgram` netdev. Unicast goes only to the port that
/// owns the destination MAC (so the router's NAT never sees zone-to-zone
/// traffic); broadcast and multicast are flooded.
/// Returns, per port, (switch-side UDP port, QEMU-side UDP port).
fn start_switch(ports: usize) -> Result<std::vec::Vec<(u16, u16)>> {
    use std::collections::HashMap;
    use std::net::UdpSocket;
    let mut sockets = std::vec::Vec::new();
    let mut links = std::vec::Vec::new();
    for _ in 0..ports {
        let sock = UdpSocket::bind("127.0.0.1:0")?;
        let qemu_port = free_port()?;
        links.push((sock.local_addr()?.port(), qemu_port));
        sockets.push((sock, qemu_port));
    }
    let table: Arc<Mutex<HashMap<[u8; 6], usize>>> = Arc::default();
    let senders: Arc<std::vec::Vec<(UdpSocket, u16)>> = Arc::new(
        sockets
            .iter()
            .map(|(s, p)| (s.try_clone().unwrap(), *p))
            .collect(),
    );
    for (i, (sock, _)) in sockets.into_iter().enumerate() {
        let (table, senders) = (table.clone(), senders.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            while let Ok(n) = sock.recv(&mut buf) {
                if n < 14 {
                    continue;
                }
                let frame = &buf[..n];
                let dst: [u8; 6] = frame[0..6].try_into().unwrap();
                let src: [u8; 6] = frame[6..12].try_into().unwrap();
                let out = {
                    let mut t = table.lock().unwrap();
                    t.insert(src, i);
                    if dst[0] & 1 == 0 {
                        t.get(&dst).copied()
                    } else {
                        None
                    }
                };
                for (j, (s, qemu_port)) in senders.iter().enumerate() {
                    if j != i && out.is_none_or(|o| o == j) {
                        let _ = s.send_to(frame, ("127.0.0.1", *qemu_port));
                    }
                }
            }
        });
    }
    Ok(links)
}

fn monitor_command(port: u16, command: &str) -> Result<()> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_millis(500)))?;
    s.write_all(format!("{command}\n").as_bytes())?;
    std::thread::sleep(Duration::from_millis(500));
    let mut out = [0u8; 4096];
    let _ = s.read(&mut out);
    Ok(())
}

/// Wait for "network up: eth 10.0.2.N" and return the address.
fn wait_ip(emu: &Emulator) -> Result<String> {
    emu.wait_log(0, "network up: eth ", Duration::from_secs(90))?;
    let log = String::from_utf8_lossy(&emu.console.lock().unwrap().log).into_owned();
    let line = log
        .lines()
        .find(|l| l.contains("network up: eth "))
        .context("no address line")?;
    let ip = line
        .split("network up: eth ")
        .nth(1)
        .context("no address")?;
    // The line ends with an ANSI colour reset; keep only the address.
    Ok(ip
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect())
}

fn zone_status(emu: &Emulator) -> Result<Value> {
    let (code, body) = emu.request("GET", "/api/zone", &[], b"")?;
    anyhow::ensure!(code == 200, "zone status {code}");
    Ok(serde_json::from_str(&body)?)
}

fn peer_heat(zone: &Value, peer: &str) -> Option<bool> {
    zone["peers"]
        .as_array()?
        .iter()
        .find(|p| p["zone"] == peer && p["active"] == true)?["heat"]
        .as_bool()
}

fn inject(emu: &Emulator, auth: &str, sensor: [u8; 6], temp: &str) -> Result<()> {
    let a = sensor;
    let body = format!(
        "addr={:02X}%3A{:02X}%3A{:02X}%3A{:02X}%3A{:02X}%3A{:02X}&temp={temp}",
        a[5], a[4], a[3], a[2], a[1], a[0]
    );
    let (code, text) = emu.request(
        "POST",
        "/api/test/sensor",
        &[("Authorization", auth)],
        body.as_bytes(),
    )?;
    anyhow::ensure!(code == 200, "inject: {code} {text}");
    Ok(())
}

fn heating(s: &Value) -> bool {
    s["heating"] == true
}

pub fn run_zones(dir: &Path, base_flash: &Path, sntp_port: u16) -> Result<()> {
    eprintln!("== two zones sharing one heater");
    let (sensor_a, sensor_b) = ([0xA1; 6], [0xB2; 6]);
    let flash_a = zone_flash(dir, base_flash, "zone-a", sensor_a, sntp_port)?;
    let flash_b = zone_flash(dir, base_flash, "zone-b", sensor_b, sntp_port)?;
    // Switch ports: 0 = router (NAT), 1 = zone A, 2 = zone B.
    let links = start_switch(3)?;
    let router = Router::start((links[0].1, links[0].0))?;
    let monitor = router.monitor;
    let (serial_a, serial_b) = (free_port()?, free_port()?);
    let mut a = Emulator::spawn(
        zone_command(
            &flash_a,
            serial_a,
            (links[1].1, links[1].0),
            "02:00:00:00:00:0a",
        )?,
        serial_a,
        &dir.join("console-zone-a.log"),
        String::new(),
        None,
    )?;
    let mut b = Emulator::spawn(
        zone_command(
            &flash_b,
            serial_b,
            (links[2].1, links[2].0),
            "02:00:00:00:00:0b",
        )?,
        serial_b,
        &dir.join("console-zone-b.log"),
        String::new(),
        None,
    )?;
    let basic = format!(
        "Basic {}",
        ota::base64(format!("admin:{PASSWORD}").as_bytes())
    );

    step("both zones get addresses, port forwards, NTP time", || {
        for emu in [&mut a, &mut b] {
            let ip = wait_ip(emu)?;
            let port = free_port()?;
            monitor_command(
                monitor,
                &format!("hostfwd_add u0 tcp:127.0.0.1:{port}-{ip}:80"),
            )?;
            emu.http = format!("127.0.0.1:{port}");
        }
        for emu in [&a, &b] {
            emu.wait_status("NTP time", Duration::from_secs(90), |s| {
                !s["time"].is_null()
            })?;
        }
        Ok(())
    })?;

    step("zones discover each other via mDNS", || {
        let deadline = Instant::now() + Duration::from_secs(150);
        loop {
            let (za, zb) = (zone_status(&a)?, zone_status(&b)?);
            let found = |z: &Value, peer: &str| {
                z["peers"]
                    .as_array()
                    .is_some_and(|p| p.iter().any(|p| p["zone"] == peer && !p["addr"].is_null()))
            };
            if found(&za, "zone-b") && found(&zb, "zone-a") {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!("discovery incomplete: A {za} / B {zb}");
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    })?;

    step("zone A requests heat: heater on, B learns it", || {
        inject(&a, &basic, sensor_a, "19.0")?;
        a.wait_status("A heating", Duration::from_secs(30), heating)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        while peer_heat(&zone_status(&b)?, "zone-a") != Some(true) {
            anyhow::ensure!(Instant::now() < deadline, "B never heard A's heat request");
            std::thread::sleep(Duration::from_secs(1));
        }
        Ok(())
    })?;

    step("zone B joins at once (heater already running)", || {
        inject(&b, &basic, sensor_b, "19.0")?;
        let s = b.wait_status("B heating", Duration::from_secs(30), heating)?;
        anyhow::ensure!(s["hold_remaining_s"].is_null(), "B should not be held: {s}");
        Ok(())
    })?;

    step(
        "zone A stops once B confirms it keeps the heater on",
        || {
            inject(&a, &basic, sensor_a, "23.0")?;
            let s = a.wait_status("A off", Duration::from_secs(30), |s| !heating(s))?;
            anyhow::ensure!(s["heating_reason"] == "all sensors above setpoint", "{s}");
            Ok(())
        },
    )?;

    step(
        "zone B is last: heater kept on until 2 min after it started",
        || {
            // Make sure B has heard that A stopped before it decides.
            let deadline = Instant::now() + Duration::from_secs(30);
            while peer_heat(&zone_status(&b)?, "zone-a") != Some(false) {
                anyhow::ensure!(Instant::now() < deadline, "B never heard A stop");
                std::thread::sleep(Duration::from_secs(1));
            }
            inject(&b, &basic, sensor_b, "23.0")?;
            let s = b.wait_status("B kept on", Duration::from_secs(30), |s| {
                s["hold_remaining_s"].as_u64().is_some()
            })?;
            anyhow::ensure!(heating(&s), "B must keep the heater on while held: {s}");
            b.wait_status("B off after the hold", Duration::from_secs(150), |s| {
                !heating(s)
            })?;
            Ok(())
        },
    )?;

    step(
        "zone A heats at once right after the heater stopped (no delay on starting)",
        || {
            let deadline = Instant::now() + Duration::from_secs(30);
            while peer_heat(&zone_status(&a)?, "zone-b") != Some(false) {
                anyhow::ensure!(Instant::now() < deadline, "A never heard B stop");
                std::thread::sleep(Duration::from_secs(1));
            }
            let before = zone_status(&a)?["heater_on_since"].as_i64();
            inject(&a, &basic, sensor_a, "19.0")?;
            let s = a.wait_status("A heating", Duration::from_secs(15), heating)?;
            anyhow::ensure!(
                s["hold_remaining_s"].is_null(),
                "starting must never be held: {s}"
            );
            // A started the heater: both zones learn the new start time.
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let (za, zb) = (zone_status(&a)?, zone_status(&b)?);
                let (on_a, on_b) = (
                    za["heater_on_since"].as_i64(),
                    zb["heater_on_since"].as_i64(),
                );
                if on_a.is_some() && on_a != before && on_a == on_b {
                    return Ok(());
                }
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "zones disagree on the heater start: {za} / {zb}"
                );
                std::thread::sleep(Duration::from_secs(1));
            }
        },
    )?;

    step(
        "both zones warm at the same moment: heater still runs its minimum on time",
        || {
            // A started the heater in the previous step; B joins.
            inject(&b, &basic, sensor_b, "19.0")?;
            b.wait_status("B heating", Duration::from_secs(15), heating)?;
            let deadline = Instant::now() + Duration::from_secs(30);
            while peer_heat(&zone_status(&a)?, "zone-b") != Some(true)
                || peer_heat(&zone_status(&b)?, "zone-a") != Some(true)
            {
                anyhow::ensure!(Instant::now() < deadline, "zones never saw each other heat");
                std::thread::sleep(Duration::from_millis(500));
            }
            let started = zone_status(&a)?["heater_on_since"]
                .as_i64()
                .context("heater start unknown")?;
            let (mark_a, mark_b) = (a.mark(), b.mark());
            // Both decide within milliseconds: each still believes the other heats.
            std::thread::scope(|s| {
                let ja = s.spawn(|| inject(&a, &basic, sensor_a, "23.0"));
                let jb = s.spawn(|| inject(&b, &basic, sensor_b, "23.0"));
                ja.join().unwrap().and(jb.join().unwrap())
            })?;
            // Poll until both outputs are off: the heater must never stop early.
            let deadline = Instant::now() + Duration::from_secs(200);
            loop {
                let (sa, sb) = (a.status()?, b.status()?);
                let now = sa["time"].as_i64().context("no time")?;
                if !heating(&sa) && !heating(&sb) {
                    anyhow::ensure!(
                        now >= started + 120,
                        "heater stopped {} s after starting (minimum 120): A {sa} / B {sb}",
                        now - started
                    );
                    break;
                }
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "heater never stopped: {sa} / {sb}"
                );
                std::thread::sleep(Duration::from_millis(500));
            }
            let held = |log: String| log.contains("release not confirmed");
            eprintln!(
                "   (releases unconfirmed: A {}, B {})",
                held(a.log_since(mark_a)),
                held(b.log_since(mark_b))
            );
            Ok(())
        },
    )?;
    Ok(())
}
