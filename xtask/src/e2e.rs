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
    tap: qemu::FrameTap,
}

impl Emulator {
    fn start(flash: &Path, log_path: &Path) -> Result<Self> {
        let http_port = free_port()?;
        let serial_port = free_port()?;
        let tap = qemu::FrameTap {
            local: free_port()?,
            remote: free_port()?,
        };
        let mut cmd = qemu::command(
            flash,
            http_port,
            &qemu::Console::Tcp(serial_port),
            Some(&tap),
        )?;
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
            http: format!("127.0.0.1:{http_port}"),
            log_path: log_path.to_path_buf(),
            tap,
        })
    }

    fn mark(&self) -> usize {
        self.console.lock().unwrap().log.len()
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
        mdns::query(
            self.tap.remote,
            self.tap.local,
            name,
            qtype,
            Duration::from_secs(10),
        )
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

pub fn run(release: bool) -> Result<()> {
    let dir = workspace_root().join("target/e2e");
    std::fs::create_dir_all(&dir)?;
    qemu::binary()?;

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

    eprintln!(
        "all end-to-end tests passed in {:.0}s",
        started.elapsed().as_secs_f32()
    );
    Ok(())
}
