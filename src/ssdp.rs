use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::watch;

use crate::models::HdhrConfig;

const SSDP_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_PORT: u16 = 1900;
const SSDP_BIND: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, SSDP_PORT);
const DEVICE_TYPE: &str = "urn:schemas-upnp-org:device:MediaServer:1";
const BROADCAST_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

fn search_response(config: &HdhrConfig) -> String {
    format!(
        "HTTP/1.1 200 OK\r\n\
         CACHE-CONTROL: max-age=1800\r\n\
         LOCATION: {}/hdhr/device.xml\r\n\
         ST: {}\r\n\
         USN: uuid:{}::{}\r\n\
         SERVER: Transmitarr/1.0 UPnP/1.0\r\n\
         \r\n",
        config.base_url, DEVICE_TYPE, config.device_id, DEVICE_TYPE
    )
}

fn notify_alive(config: &HdhrConfig) -> String {
    format!(
        "NOTIFY * HTTP/1.1\r\n\
         HOST: 239.255.255.250:1900\r\n\
         CACHE-CONTROL: max-age=1800\r\n\
         LOCATION: {}/hdhr/device.xml\r\n\
         NT: {}\r\n\
         NTS: ssdp:alive\r\n\
         USN: uuid:{}::{}\r\n\
         SERVER: Transmitarr/1.0 UPnP/1.0\r\n\
         \r\n",
        config.base_url, DEVICE_TYPE, config.device_id, DEVICE_TYPE
    )
}

fn notify_byebye(config: &HdhrConfig) -> String {
    format!(
        "NOTIFY * HTTP/1.1\r\n\
         HOST: 239.255.255.250:1900\r\n\
         NT: {}\r\n\
         NTS: ssdp:byebye\r\n\
         USN: uuid:{}::{}\r\n\
         \r\n",
        DEVICE_TYPE, config.device_id, DEVICE_TYPE
    )
}

fn is_relevant_search(buf: &[u8]) -> bool {
    let Ok(msg) = std::str::from_utf8(buf) else {
        return false;
    };
    let upper = msg.to_ascii_uppercase();
    upper.contains("M-SEARCH")
        && (upper.contains("SSDP:ALL")
            || upper.contains("MEDIASERVER")
            || upper.contains("ROOTDEVICE"))
}

fn create_multicast_socket() -> std::io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;

    #[cfg(unix)]
    socket.set_reuse_port(true)?;

    socket.set_nonblocking(true)?;
    socket.bind(&socket2::SockAddr::from(SSDP_BIND))?;
    socket.join_multicast_v4(&SSDP_ADDR, &Ipv4Addr::UNSPECIFIED)?;
    socket.set_multicast_loop_v4(true)?;

    Ok(socket.into())
}

pub async fn run(mut rx: watch::Receiver<Option<HdhrConfig>>) {
    // 1. Wait for initial config
    if rx.wait_for(|c| c.is_some()).await.is_err() {
        tracing::warn!("SSDP: config channel closed before receiving initial config");
        return;
    }

    // 2. Create multicast socket
    let std_socket = match create_multicast_socket() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("SSDP: failed to create multicast socket: {}", e);
            return;
        }
    };
    let socket = match UdpSocket::from_std(std_socket) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::error!("SSDP: failed to convert to tokio socket: {}", e);
            return;
        }
    };

    // 3. Log startup
    tracing::info!("SSDP discovery started on 239.255.255.250:1900");

    // 4. Cache last_config for shutdown byebye
    let mut last_config = rx.borrow().clone();

    // 5. Spawn listener task
    let listener_socket = socket.clone();
    let mut listener_rx = rx.clone();
    let listener = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            tokio::select! {
                result = listener_socket.recv_from(&mut buf) => {
                    match result {
                        Ok((len, src)) => {
                            if is_relevant_search(&buf[..len]) {
                                let config = listener_rx.borrow().clone();
                                if let Some(cfg) = config {
                                    let response = search_response(&cfg);
                                    if let Err(e) = listener_socket.send_to(response.as_bytes(), src).await {
                                        tracing::debug!("SSDP: failed to send search response: {}", e);
                                    } else {
                                        tracing::debug!("SSDP: responded to M-SEARCH from {}", src);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("SSDP: recv_from error: {}", e);
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        }
                    }
                }
                _ = listener_rx.changed() => {
                    // Config updated, will pick up on next borrow
                    continue;
                }
            }
        }
    });

    // 6. Spawn broadcaster task
    let broadcaster_socket = socket.clone();
    let mut broadcaster_rx = rx.clone();
    let multicast_dest = std::net::SocketAddrV4::new(SSDP_ADDR, SSDP_PORT);
    let broadcaster = tokio::spawn(async move {
        let mut interval = tokio::time::interval(BROADCAST_INTERVAL);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let config = broadcaster_rx.borrow().clone();
                    if let Some(cfg) = config {
                        let msg = notify_alive(&cfg);
                        if let Err(e) = broadcaster_socket.send_to(msg.as_bytes(), multicast_dest).await {
                            tracing::debug!("SSDP: failed to send NOTIFY alive: {}", e);
                        }
                    }
                }
                _ = broadcaster_rx.changed() => {
                    // Config updated, will pick up on next tick
                    continue;
                }
            }
        }
    });

    // 7. Shutdown detection loop
    while let Ok(()) = rx.changed().await {
        let current = rx.borrow().clone();
        match current {
            Some(cfg) => {
                last_config = Some(cfg);
            }
            None => {
                // None signals shutdown
                break;
            }
        }
    }

    // 8. Send byebye using last known config
    if let Some(cfg) = &last_config {
        let msg = notify_byebye(cfg);
        if let Err(e) = socket.send_to(msg.as_bytes(), multicast_dest).await {
            tracing::debug!("SSDP: failed to send NOTIFY byebye: {}", e);
        } else {
            tracing::info!("SSDP: sent byebye notification");
        }
    }

    // 9. Abort listener and broadcaster tasks
    listener.abort();
    broadcaster.abort();
    tracing::info!("SSDP: shutdown complete");
}
