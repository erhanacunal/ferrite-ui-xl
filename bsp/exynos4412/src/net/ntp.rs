//! SNTP client (CPU3): sets the clock from an NTP server (`system/settings.txt` `ntp=`,
//! default `pool.ntp.org`) once DHCP gave an address, then every [`EVERY_S`], sooner when a
//! program asks (`SYS_NTP_SYNC`), after a failure every [`RETRY_S`]. The time goes to CPU0
//! through `rtc::set_from_ntp` (the RTC is on I2C0, which CPU0 owns); the state through
//! `settings::NTP_*`.

use core::sync::atomic::Ordering;

use embassy_net::dns::DnsQueryType;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpEndpoint, Stack};
use embassy_time::{Duration, Timer, with_timeout};
use exynos_4412_rs::println;
use ferrite_core::settings as fs;

use crate::settings::{NTP_REQUEST, NTP_STATE, NTP_SYNCED_AT};
use crate::systick;

const PORT: u16 = 123;
/// 1900-01-01 → 1970-01-01. NTP era 1 (from 2036) wraps to the right value in `u32`.
const NTP_TO_UNIX: u32 = 2_208_988_800;
const TRIES: usize = 3;
const ANSWER: Duration = Duration::from_secs(3);
/// Between syncs (the RTC drifts by seconds a day at most).
const EVERY_S: u32 = 6 * 3600;
const RETRY_S: u32 = 60;

fn set_state(s: i32) {
    NTP_STATE.store(s as u32, Ordering::Release);
}

pub async fn client(stack: Stack<'static>) {
    let Some(server) = crate::settings::ntp_server() else {
        set_state(fs::NTP_OFF);
        return;
    };
    loop {
        set_state(fs::NTP_WAITING);
        stack.wait_config_up().await;
        set_state(fs::NTP_RUNNING);
        NTP_REQUEST.store(false, Ordering::Relaxed);
        let wait = match sync(stack, server).await {
            Ok(unix) => {
                println!("ferrite: clock set from NTP ({}), unix {}", server, unix);
                NTP_SYNCED_AT.store(systick::millis().max(1), Ordering::Release);
                set_state(fs::NTP_SYNCED);
                EVERY_S
            }
            Err(e) => {
                println!("ferrite: NTP ({}) failed: {}", server, e);
                set_state(fs::NTP_FAILED);
                RETRY_S
            }
        };
        // Until the next sync, a program asks for one, or the address is lost.
        for _ in 0..wait {
            if NTP_REQUEST.swap(false, Ordering::AcqRel) || !stack.is_config_up() {
                break;
            }
            Timer::after(Duration::from_secs(1)).await;
        }
    }
}

/// One query (up to [`TRIES`]): Unix seconds given to the RTC.
async fn sync(stack: Stack<'static>, server: &str) -> Result<u32, &'static str> {
    let addrs = stack.dns_query(server, DnsQueryType::A).await.map_err(|_| "no DNS answer")?;
    let addr = *addrs.first().ok_or("no DNS answer")?;
    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut tx_meta = [PacketMetadata::EMPTY; 2];
    let mut rx = [0u8; 128];
    let mut tx = [0u8; 128];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
    socket.bind(0).map_err(|_| "bind")?;
    for _ in 0..TRIES {
        // LI 0, version 4, mode 3 (client); the transmit time is a nonce the reply's
        // originate time must repeat.
        let mut req = [0u8; 48];
        req[0] = 0x23;
        let sent = systick::millis();
        req[40..44].copy_from_slice(&sent.to_be_bytes());
        req[44..48].copy_from_slice(&embassy_time::Instant::now().as_ticks().to_be_bytes()[4..]);
        socket.send_to(&req, IpEndpoint::new(addr, PORT)).await.map_err(|_| "send")?;
        let mut buf = [0u8; 128];
        loop {
            let Ok(Ok((n, meta))) = with_timeout(ANSWER, socket.recv_from(&mut buf)).await else { break };
            let recv = systick::millis();
            if n < 48 || meta.endpoint.port != PORT || buf[24..32] != req[40..48] {
                continue;
            }
            let (li, mode, stratum) = (buf[0] >> 6, buf[0] & 7, buf[1]);
            // Mode 4 = server; stratum 0 = "kiss of death"; LI 3 = not synchronised.
            if mode != 4 || stratum == 0 || stratum > 15 || li == 3 {
                return Err("server not synchronised");
            }
            let secs = u32::from_be_bytes([buf[40], buf[41], buf[42], buf[43]]);
            let frac = u32::from_be_bytes([buf[44], buf[45], buf[46], buf[47]]);
            let frac_ms = ((u64::from(frac) * 1000) >> 32) as u32;
            // The server's transmit time is half the round trip before the reply arrived;
            // `at` is our clock at its whole second.
            let at = recv.wrapping_sub(recv.wrapping_sub(sent) / 2).wrapping_sub(frac_ms);
            let unix = secs.wrapping_sub(NTP_TO_UNIX);
            crate::rtc::set_from_ntp(unix, at);
            return Ok(unix);
        }
    }
    Err("no reply")
}
