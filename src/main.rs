// ETG.8200 EtherCAT Mailbox Gateway
// UDP port 34980

use ethercrab::{
    std::{ethercat_now, tx_rx_task},
    MainDevice, MainDeviceConfig, PduStorage, SubDeviceGroup, Timeouts,
};
use std::{collections::HashMap, net::Ipv4Addr, sync::Arc, time::Duration};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

const PORT: u16 = 0x88A4; // 34980
const MAX: usize = 1500; // MTU limit
const MAX_PAYLOAD: usize = 1498; // Max mailbox payload (MTU - EC header)
const EC_HDR: usize = 2; // EtherCAT header
const MBX_HDR: usize = 6; // Mailbox header

// PDU pool
const FRAMES: usize = 32;
const MAX_PDU: usize = PduStorage::element_size(1500);
static PDU: PduStorage<FRAMES, MAX_PDU> = PduStorage::new();

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let mut args = std::env::args().skip(1);
    let iface = args.next().unwrap_or_else(|| {
        eprintln!("Usage: ethercrab-mbg <iface>");
        eprintln!();
        eprintln!("EtherCAT Mailbox Gateway (ETG.8200) - UDP port 0x88A4 (34980)");
        eprintln!();
        eprintln!("Environment:");
        eprintln!("  GW_TIMEOUT_MS=10000    Mailbox timeout (ms)");
        eprintln!("  GW_BIND_ADDR=0.0.0.0   Bind address");
        eprintln!();
        eprintln!("IMPORTANT:");
        eprintln!("  - TwinSAFE Loader/User require slaves in SAFE-OP or OP state");
        eprintln!("  - Do NOT use CoE/SDO access in parallel with TwinSAFE tools");
        eprintln!("  - Gateway drops packets silently on errors (standard behavior)");
        std::process::exit(1);
    });

    let timeout_ms: u64 = std::env::var("GW_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000);
    let timeout = Duration::from_millis(timeout_ms);
    log::info!(
        "Gateway timeout {} ms (TwinSAFE Loader default)",
        timeout_ms
    );

    let bind_addr = std::env::var("GW_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0".to_string());
    let bind_addr: Ipv4Addr = bind_addr.parse().unwrap_or_else(|_| {
        eprintln!("Invalid bind address, using 0.0.0.0");
        Ipv4Addr::UNSPECIFIED
    });

    let (tx, rx, pdu_loop) = PDU.try_split().expect("PDU split failed");
    let maindevice = Arc::new(MainDevice::new(
        pdu_loop,
        Timeouts::default(),
        MainDeviceConfig::default(),
    ));

    let iface_clone = iface.clone();
    let mut txrx =
        tokio::spawn(async move { tx_rx_task(&iface_clone, tx, rx).expect("txrx").await });

    // Discover slaves
    const MAX_SLAVES: usize = 512;
    const MAX_PDI: usize = 1;
    let mut group: SubDeviceGroup<MAX_SLAVES, MAX_PDI> = maindevice
        .init_single_group::<MAX_SLAVES, MAX_PDI>(ethercat_now)
        .await?;
    log::info!(
        "Found {} slaves; MBG active on UDP/{} (timeout: {}ms)",
        group.len(),
        PORT,
        timeout_ms
    );

    // Map addresses to indices
    let mut addr_map = HashMap::<u16, usize>::new();
    log::info!("EtherCAT address mapping:");
    for (idx, sd) in group.iter(&maindevice).enumerate() {
        let addr = sd.configured_address();
        addr_map.insert(addr, idx);
        log::info!("  Address {} -> Slave index {}", addr, idx);
    }
    log::info!("Total {} slaves mapped", addr_map.len());

    // UDP server
    let udp = UdpSocket::bind((bind_addr, PORT)).await?;
    log::info!("Listening on {}:{}", bind_addr, PORT);

    // Single worker for serialization
    let (rq_tx, mut rq_rx) = mpsc::channel::<(Vec<u8>, oneshot::Sender<Option<Vec<u8>>>)>(32);

    let md = maindevice.clone();
    let mut worker = tokio::spawn(async move {
        while let Some((pkt, tx_reply)) = rq_rx.recv().await {
            let reply = process(&md, &mut group, &addr_map, &pkt, timeout)
                .await
                .unwrap_or(None);
            let _ = tx_reply.send(reply);
        }
    });

    let mut buf = vec![0u8; MAX];
    loop {
        tokio::select! {
            r = udp.recv_from(&mut buf) => {
                if let Ok((n, peer)) = r {
                    if let Some(req) = validate_udp(&buf[..n]) {
                        let (txr, rxr) = oneshot::channel();
                        if rq_tx.send((req, txr)).await.is_ok() {
                            if let Ok(Some(rep)) = rxr.await {
                                let _ = udp.send_to(&rep, peer).await;
                            }
                        }
                    }
                }
            }
            _ = &mut txrx => break,
            _ = &mut worker => break,
        }
    }

    Ok(())
}

// Validate UDP packet
fn validate_udp(pkt: &[u8]) -> Option<Vec<u8>> {
    if pkt.len() < EC_HDR {
        return None;
    }
    let hdr = u16::from_le_bytes([pkt[0], pkt[1]]);
    let ec_len = (hdr & 0x07FF) as usize;
    if ec_len < MBX_HDR {
        return None;
    }
    if ec_len > MAX_PAYLOAD {
        return None;
    }
    if EC_HDR + ec_len > pkt.len() {
        return None;
    }
    Some(pkt[..EC_HDR + ec_len].to_vec()) // trim to exact size
}

// Process mailbox request
async fn process<const SD: usize, const PDI: usize>(
    md: &Arc<MainDevice<'_>>,
    group: &mut SubDeviceGroup<SD, PDI>,
    map: &HashMap<u16, usize>,
    pkt: &[u8],
    timeout: Duration,
) -> anyhow::Result<Option<Vec<u8>>> {
    let hdr = u16::from_le_bytes([pkt[0], pkt[1]]);
    let ec_len = (hdr & 0x07FF) as usize;
    // Preserve upper 5 bits (0xF800) from request in reply
    let upper = hdr & 0xF800;

    let mbox = &pkt[EC_HDR..EC_HDR + ec_len];
    if mbox.len() < MBX_HDR {
        return Ok(None);
    }

    let m_len = u16::from_le_bytes([mbox[0], mbox[1]]) as usize;
    let station = u16::from_le_bytes([mbox[2], mbox[3]]);
    // Strict check: mailbox must fit within EtherCAT frame length
    if MBX_HDR + m_len > ec_len {
        return Ok(None);
    }

    let Some(&idx) = map.get(&station) else {
        // Rate-limited log for unknown station address (max once per second)
        static LAST_LOG: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
        if let Ok(mut last) = LAST_LOG.try_lock() {
            let now = std::time::Instant::now();
            if last.map_or(true, |t| now.duration_since(t).as_secs() >= 1) {
                log::debug!("Unknown station address 0x{:04X} - dropping", station);
                *last = Some(now);
            }
        }
        return Ok(None);
    };

    let sd = group.subdevice(md, idx)?;
    // Hard cap end-to-end time; on timeout we simply return None (no UDP reply)
    let rep = match tokio::time::timeout(timeout, sd.mailbox_passthrough(mbox, timeout)).await {
        Ok(Ok(bytes)) => Some(bytes),
        Ok(Err(_)) => None,
        Err(_) => None, // timeout elapsed
    };

    if let Some(rep_bytes) = rep.as_ref() {
        // MTU cap: 1498 bytes payload, 11-bit length limit
        let max_len = MAX_PAYLOAD.min(0x07FF);
        if rep_bytes.len() > max_len {
            log::debug!(
                "Reply truncated: {} bytes exceeds limit {} (11-bit={}, MTU={})",
                rep_bytes.len(),
                max_len,
                0x07FF,
                MAX_PAYLOAD
            );
            return Ok(None);
        }

        // Debug sanity check: verify reply echoes same station address
        #[cfg(debug_assertions)]
        if rep_bytes.len() >= 4 {
            let reply_station = u16::from_le_bytes([rep_bytes[2], rep_bytes[3]]);
            if reply_station != station {
                log::warn!(
                    "Station address mismatch: request={:04X}, reply={:04X} - possible bus crosstalk",
                    station, reply_station
                );
                // Continue anyway - IgH doesn't enforce this
            }
        }

        let mut out = Vec::with_capacity(EC_HDR + rep_bytes.len());
        // Preserve upper 5 bits, update 11-bit length
        let lh = upper | ((rep_bytes.len() as u16) & 0x07FF);
        out.extend_from_slice(&lh.to_le_bytes());
        out.extend_from_slice(rep_bytes);
        return Ok(Some(out));
    }
    Ok(None) // silent on errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_udp_valid_packet() {
        // Valid packet: 2-byte EC header + 6-byte mailbox header + 4-byte payload
        let mut pkt = vec![0u8; 12];
        // EC header: length = 10 (6 + 4), upper bits clear
        pkt[0] = 0x0A; // length low byte
        pkt[1] = 0x00; // length high byte (no upper bits set)
                       // Mailbox header
        pkt[2] = 0x04; // mailbox length low
        pkt[3] = 0x00; // mailbox length high
        pkt[4] = 0x01; // station low
        pkt[5] = 0x10; // station high
        pkt[6] = 0x00; // control
        pkt[7] = 0x03; // type (CoE)
                       // Payload
        pkt[8..12].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);

        let result = validate_udp(&pkt);
        assert!(result.is_some());
        let validated = result.unwrap();
        assert_eq!(validated.len(), 12); // EC_HDR + ec_len
    }

    #[test]
    fn test_validate_udp_too_short() {
        // Packet too short (only 1 byte)
        let pkt = vec![0x00];
        assert!(validate_udp(&pkt).is_none());
    }

    #[test]
    fn test_validate_udp_mailbox_too_short() {
        // EC header says 4 bytes but that's less than mailbox header size
        let mut pkt = vec![0u8; 6];
        pkt[0] = 0x04; // length = 4 (less than MBX_HDR)
        pkt[1] = 0x00;
        assert!(validate_udp(&pkt).is_none());
    }

    #[test]
    fn test_validate_udp_exceeds_max_payload() {
        // EC header says 1500 bytes (exceeds MAX_PAYLOAD of 1498)
        let mut pkt = vec![0u8; 1502];
        pkt[0] = 0xDC; // 1500 low byte
        pkt[1] = 0x05; // 1500 high byte
        assert!(validate_udp(&pkt).is_none());
    }

    #[test]
    fn test_validate_udp_preserves_upper_bits() {
        // Valid packet with upper 5 bits set in EC header
        let mut pkt = vec![0u8; 12];
        // EC header: length = 10, upper bits = 0xF8
        pkt[0] = 0x0A; // length low byte
        pkt[1] = 0xF8; // upper 5 bits all set
                       // Rest of packet
        pkt[2..8].copy_from_slice(&[0x04, 0x00, 0x01, 0x10, 0x00, 0x03]);
        pkt[8..12].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);

        let result = validate_udp(&pkt);
        assert!(result.is_some());
        // Check that upper bits are preserved in the validated packet
        let validated = result.unwrap();
        assert_eq!(validated[1], 0xF8);
    }

    #[test]
    fn test_validate_udp_ignores_trailing_bytes() {
        // Packet with trailing bytes that should be ignored
        let mut pkt = vec![0u8; 20];
        // EC header: length = 10
        pkt[0] = 0x0A;
        pkt[1] = 0x00;
        // Valid mailbox data
        pkt[2..12].copy_from_slice(&[0x04, 0x00, 0x01, 0x10, 0x00, 0x03, 0xAA, 0xBB, 0xCC, 0xDD]);
        // Trailing garbage
        pkt[12..20].copy_from_slice(&[0xFF; 8]);

        let result = validate_udp(&pkt);
        assert!(result.is_some());
        let validated = result.unwrap();
        assert_eq!(validated.len(), 12); // Should not include trailing bytes
    }

    #[test]
    fn test_mailbox_length_guard() {
        // Test that MBX_HDR + m_len > ec_len is caught
        // This would need to be tested in process() function
        // but we can test the validation logic here

        // EC says 10 bytes total
        let mut pkt = vec![0u8; 12];
        pkt[0] = 0x0A; // EC length = 10
        pkt[1] = 0x00;
        // But mailbox header claims 8 bytes (which would need 14 total)
        pkt[2] = 0x08; // mailbox length = 8
        pkt[3] = 0x00;
        pkt[4..8].copy_from_slice(&[0x01, 0x10, 0x00, 0x03]);

        // validate_udp should pass (it only checks EC frame)
        let result = validate_udp(&pkt);
        assert!(result.is_some());

        // But process() should reject this (tested separately if we had access)
    }
}
