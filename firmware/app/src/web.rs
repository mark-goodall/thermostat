//! HTTP server tasks on port 80.

use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_time::{Duration, Instant, with_timeout};
use thermostat_core::web;

use crate::device::Handle;

/// Concurrent connections served. Browsers open a couple in parallel, and other
/// zones post events; a connection arriving while every task is busy is refused.
/// Each task holds ~14 KiB of buffers, and RAM beside the radio is tight.
pub const TASKS: usize = 2;

#[embassy_executor::task(pool_size = TASKS)]
pub async fn task(stack: Stack<'static>, dev: Handle) {
    let mut rx = [0u8; 2048];
    let mut tx = [0u8; 2048];
    let mut buf = [0u8; web::BUF_LEN];
    let mut dev = dev;
    loop {
        let mut sock = TcpSocket::new(stack, &mut rx, &mut tx);
        sock.set_timeout(Some(Duration::from_secs(20)));
        if sock.accept(80).await.is_err() {
            continue;
        }
        let start = Instant::now();
        let peer = sock.remote_endpoint();
        if let Err(e) = web::handle(&mut sock, &mut dev, &mut buf).await {
            log::debug!("http: {e:?}");
        }
        sock.close();
        // Let the response and FIN go out, but don't keep this listener away
        // for long if the client has vanished.
        let _ = with_timeout(Duration::from_secs(2), sock.flush()).await;
        let took = start.elapsed();
        if took > Duration::from_secs(5) {
            log::warn!(
                "http: connection from {peer:?} held a slot for {} ms",
                took.as_millis()
            );
        }
    }
}
