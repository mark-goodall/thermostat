//! HTTP server tasks on port 80.

use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_time::Duration;
use thermostat_core::web;

use crate::device::Handle;

/// Concurrent connections served. Browsers open a couple in parallel.
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
        if let Err(e) = web::handle(&mut sock, &mut dev, &mut buf).await {
            log::debug!("http: {e:?}");
        }
        sock.close();
        let _ = sock.flush().await;
    }
}
