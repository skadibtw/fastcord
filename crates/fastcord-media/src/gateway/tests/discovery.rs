//! Independent M94 join-campaign checks. These exercise the real signaling
//! driver with a deterministic datagram server and a port allocator that would
//! immediately reuse an unretained port. They model datagrams, not TCP segments
//! or evidence that a laboratory prelude bypasses a real network filter.

use super::*;

const PRELUDE_BYTES: usize = 1_200;

#[derive(Default)]
struct PortAudit {
    bound: Vec<u16>,
    held: Vec<u16>,
    sent: Vec<(Instant, u16, Vec<u8>)>,
    assisted: Vec<(u16, u8)>,
}

#[derive(Clone)]
struct AuditedNetwork {
    peers: mpsc::UnboundedSender<Udp>,
    binds: Arc<Mutex<Vec<SocketAddr>>>,
    audit: Arc<Mutex<PortAudit>>,
    stall_prelude: bool,
    stall_bind: bool,
    experimental: bool,
    port_recovery: bool,
}

struct AuditedLink {
    inner: FakeLink,
    port: u16,
    audit: Arc<Mutex<PortAudit>>,
    stall_prelude: bool,
}

impl Drop for AuditedLink {
    fn drop(&mut self) {
        self.audit.lock().held.retain(|port| *port != self.port);
    }
}

impl UdpLink for AuditedLink {
    async fn send_discovery(&self, datagram: &[u8], to: SocketAddr, variant: u8) -> io::Result<()> {
        self.audit.lock().assisted.push((self.port, variant));
        self.send_to(datagram, to).await
    }
    fn local_port(&self) -> Option<u16> {
        Some(self.port)
    }

    async fn send_to(&self, datagram: &[u8], to: SocketAddr) -> io::Result<()> {
        if self.stall_prelude && datagram.len() == PRELUDE_BYTES {
            return std::future::pending().await;
        }
        self.audit
            .lock()
            .sent
            .push((Instant::now(), self.port, datagram.to_vec()));
        self.inner.send_to(datagram, to).await
    }

    async fn recv_from(&self, buffer: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.inner.recv_from(buffer).await
    }
}

impl Network for AuditedNetwork {
    type Udp = AuditedLink;

    fn experimental_udp(&self) -> bool {
        self.experimental
    }

    fn udp_port_recovery(&self) -> bool {
        self.port_recovery
    }

    async fn bind(&self, server: SocketAddr) -> io::Result<AuditedLink> {
        if self.stall_bind {
            return std::future::pending().await;
        }
        self.binds.lock().push(server);
        let port = {
            let mut audit = self.audit.lock();
            // A discarded old flow is deliberately reused immediately. The
            // driver must retain old sockets until the campaign is finished.
            let port = (40_000..40_010)
                .find(|port| !audit.held.contains(port))
                .unwrap();
            audit.bound.push(port);
            audit.held.push(port);
            port
        };
        let (to_server, from_client) = mpsc::unbounded_channel();
        let (to_client, from_server) = mpsc::unbounded_channel();
        self.peers
            .send(Udp {
                from_client,
                to_client,
                log: Vec::new(),
            })
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        Ok(AuditedLink {
            inner: FakeLink {
                to_server,
                from_server: tokio::sync::Mutex::new(from_server),
            },
            port,
            audit: self.audit.clone(),
            stall_prelude: self.stall_prelude,
        })
    }
}

fn audited(stall_prelude: bool) -> (Harness, FakeSignaling, Arc<Mutex<PortAudit>>) {
    audited_with(stall_prelude, false)
}

fn audited_with(
    stall_prelude: bool,
    stall_bind: bool,
) -> (Harness, FakeSignaling, Arc<Mutex<PortAudit>>) {
    audited_policy(stall_prelude, stall_bind, true, false)
}

fn audited_policy(
    stall_prelude: bool,
    stall_bind: bool,
    experimental: bool,
    port_recovery: bool,
) -> (Harness, FakeSignaling, Arc<Mutex<PortAudit>>) {
    let signaling = FakeSignaling::default();
    let (peers_tx, peers) = mpsc::unbounded_channel();
    let binds = Arc::new(Mutex::new(Vec::new()));
    let audit = Arc::new(Mutex::new(PortAudit::default()));
    let network = AuditedNetwork {
        peers: peers_tx,
        binds: binds.clone(),
        audit: audit.clone(),
        stall_prelude,
        stall_bind,
        experimental,
        port_recovery,
    };
    let session = start(
        signaling.clone(),
        network,
        credentials("fixture.discord.media:443"),
        options(),
    );
    (
        Harness {
            session,
            peers,
            binds,
        },
        signaling,
        audit,
    )
}

async fn begin(ws: &mut Ws, interval_ms: f64) {
    ws.hello(interval_ms).await;
    assert_eq!(ws.recv_op(0).await["op"], 0);
    ws.send(ready(&BOTH_MODES)).await;
}

fn assert_discovery(datagram: &[u8]) {
    assert_eq!(datagram.len(), DISCOVERY_LEN);
    assert_eq!(&datagram[..8], &[0, 1, 0, 70, 0, 0, 0x32, 0x47]);
}

async fn read_round_start(udp: &mut Udp, preludes: usize) -> Vec<u8> {
    for _ in 0..preludes {
        let prelude = udp.recv().await;
        assert_eq!(prelude.len(), PRELUDE_BYTES);
        assert_eq!(
            prelude.as_slice(),
            &include_bytes!("../../../../../fixtures/voice/udp-quic-v2.bin")[..]
        );
    }
    let discovery = udp.recv().await;
    assert_discovery(&discovery);
    discovery
}

async fn finish_unanswered_round(udp: &mut Udp, first: &[u8]) {
    for _ in 1..5 {
        assert_eq!(udp.recv().await, first);
    }
}

fn assert_campaign_caps(audit: &PortAudit) {
    assert!(audit.bound.len() <= 3, "socket budget was replenished");
    let discovery = audit
        .sent
        .iter()
        .filter(|(_, _, packet)| packet.len() == DISCOVERY_LEN)
        .count();
    let prelude_bytes = audit
        .sent
        .iter()
        .filter(|(_, _, packet)| packet.len() == PRELUDE_BYTES)
        .map(|(_, _, packet)| packet.len())
        .sum::<usize>();
    assert!(discovery <= 15, "discovery budget was replenished");
    assert!(prelude_bytes <= 10_800, "prelude budget was replenished");
    assert!(
        audit.assisted.len() <= 9,
        "packet helper budget was replenished"
    );
}

#[tokio::test(start_paused = true)]
async fn port_recovery_uses_fresh_flows_without_preludes_or_extra_identifies() {
    let (mut harness, signaling, audit) = audited_policy(false, false, false, true);
    let mut ws = signaling.accept().await;
    begin(&mut ws, QUIET_HEARTBEAT_MS).await;
    let mut old_flows = Vec::new();
    for _ in 0..2 {
        let mut udp = harness.udp().await;
        let first = read_round_start(&mut udp, 0).await;
        finish_unanswered_round(&mut udp, &first).await;
        old_flows.push(udp);
    }
    let mut current = harness.udp().await;
    let first = read_round_start(&mut current, 0).await;
    current.answer_discovery(&first);
    ws.recv_op(1).await;
    assert_eq!(harness.session.status(), VoiceStatus::Negotiating);
    assert_eq!(signaling.urls().len(), 1);
    {
        let audit = audit.lock();
        assert_eq!(audit.bound, [40_000, 40_001, 40_002]);
        assert_eq!(audit.held.len(), 3);
        assert_eq!(audit.sent.len(), 11);
        assert_eq!(
            audit.assisted.iter().map(|(_, v)| *v).collect::<Vec<_>>(),
            [1, 1, 1, 2, 2, 2, 3]
        );
        assert!(
            audit
                .sent
                .iter()
                .all(|(_, _, packet)| packet.len() == DISCOVERY_LEN)
        );
        for pair in audit.sent.windows(2) {
            assert_eq!(pair[1].0 - pair[0].0, Duration::from_secs(1));
        }
    }
    harness.session.leave().await;
}

async fn check_selected_path_recovery(experimental: bool, port_recovery: bool) {
    let (mut harness, signaling, audit) = audited_policy(false, false, experimental, port_recovery);
    let mut ws = signaling.accept().await;
    let mut second = signaling.accept().await;
    let (mut udp, _) = negotiate(&mut harness, &mut ws, &BOTH_MODES).await;
    ws.send(session_description("aead_aes256_gcm_rtpsize"))
        .await;
    let _fixture = install_test_dave_group(&harness, &mut ws).await;
    let ping = loop {
        let packet = udp.recv_any().await;
        if is_ping(&packet) {
            break packet;
        }
    };
    let mut answer = ping;
    answer[..4].copy_from_slice(&crate::udp::PING_RESPONSE.to_be_bytes());
    udp.send(answer);
    second.hello(QUIET_HEARTBEAT_MS).await;
    timeout(Duration::from_secs(60), second.recv_op(0))
        .await
        .expect("silent experimental connection did not re-identify");
    second.send(ready(&BOTH_MODES)).await;
    let mut replacement = timeout(Duration::from_secs(5), harness.udp())
        .await
        .expect("silent experimental connection did not bind a new socket");
    let discovery = timeout(Duration::from_secs(5), replacement.recv())
        .await
        .expect("replacement did not send discovery");
    assert_discovery(&discovery);
    replacement.answer_discovery(&discovery);
    timeout(Duration::from_secs(5), second.recv_op(1))
        .await
        .expect("replacement did not select the new transport");
    assert!(
        audit
            .lock()
            .sent
            .iter()
            .all(|(_, _, packet)| packet.len() != PRELUDE_BYTES)
    );
    harness.session.leave().await;
}

#[tokio::test(start_paused = true)]
async fn selected_paths_can_reconnect_without_refilling_preludes() {
    check_selected_path_recovery(true, false).await;
    check_selected_path_recovery(false, true).await;
}

#[tokio::test(start_paused = true)]
async fn three_variants_use_fifteen_spaced_requests_and_one_prelude_per_fresh_flow() {
    let (mut harness, signaling, audit) = audited(false);
    let mut ws = signaling.accept().await;
    begin(&mut ws, QUIET_HEARTBEAT_MS).await;
    let started = Instant::now();
    let mut old_flows = Vec::new();
    for (index, preludes) in [0, 3, 6].into_iter().enumerate() {
        let mut udp = harness.udp().await;
        let first = read_round_start(&mut udp, preludes).await;
        assert_eq!(
            harness.session.status(),
            VoiceStatus::DiscoveringExperimental {
                variant: (index + 1) as u8
            }
        );
        assert_eq!(
            Instant::now() - started,
            Duration::from_secs(index as u64 * 5) + Duration::from_millis(index as u64 * 50)
        );
        assert!(
            old_flows.iter().all(|old: &Udp| !old.to_client.is_closed()),
            "old ports were released during rotation"
        );
        finish_unanswered_round(&mut udp, &first).await;
        old_flows.push(udp);
    }
    assert_eq!(harness.closed().await, CloseReason::UdpUnreachable);
    assert_eq!(Instant::now() - started, Duration::from_millis(15_100));
    assert_eq!(
        signaling.urls().len(),
        1,
        "rotation must not Identify through another WebSocket"
    );
    while let Some(message) = ws.try_recv() {
        assert_ne!(message["op"], 0, "extra voice Identify");
        assert_ne!(message["op"], 1, "unanswered discovery selected a protocol");
    }
    let audit = audit.lock();
    assert_campaign_caps(&audit);
    assert_eq!(audit.bound, [40_000, 40_001, 40_002]);
    assert!(audit.held.is_empty(), "closed campaign retained sockets");
    let discoveries = audit
        .sent
        .iter()
        .filter(|(_, _, packet)| packet.len() == DISCOVERY_LEN)
        .collect::<Vec<_>>();
    assert_eq!(discoveries.len(), 15);
    for pair in discoveries.windows(2) {
        let expected = if pair[0].1 == pair[1].1 {
            Duration::from_secs(1)
        } else {
            Duration::from_millis(1050)
        };
        assert_eq!(pair[1].0 - pair[0].0, expected);
    }
    for port in [40_001, 40_002] {
        let prelude_at = audit
            .sent
            .iter()
            .filter(|(_, p, packet)| *p == port && packet.len() == PRELUDE_BYTES)
            .map(|(at, _, _)| *at)
            .max()
            .unwrap();
        let discovery_at = discoveries.iter().find(|(_, p, _)| *p == port).unwrap().0;
        assert_eq!(discovery_at - prelude_at, Duration::from_millis(50));
    }
    assert_eq!(
        audit
            .sent
            .iter()
            .filter(|(_, _, packet)| packet.len() == PRELUDE_BYTES)
            .count(),
        9
    );
}

#[tokio::test(start_paused = true)]
async fn old_flow_and_wrong_source_responses_do_not_select_and_selection_happens_once() {
    let (mut harness, signaling, audit) = audited(false);
    let mut ws = signaling.accept().await;
    begin(&mut ws, QUIET_HEARTBEAT_MS).await;
    let mut old_flows = Vec::new();
    for preludes in [0, 3] {
        let mut udp = harness.udp().await;
        let first = read_round_start(&mut udp, preludes).await;
        finish_unanswered_round(&mut udp, &first).await;
        old_flows.push((udp, first));
    }
    let mut current = harness.udp().await;
    let first = read_round_start(&mut current, 6).await;
    for (old, request) in &old_flows {
        assert!(!old.to_client.is_closed());
        old.answer_discovery(request);
    }
    let mut wrong_ssrc = first.clone();
    wrong_ssrc[1] = 2;
    wrong_ssrc[7] ^= 1;
    current.send(wrong_ssrc);
    let mut wrong_source = first.clone();
    wrong_source[1] = 2;
    current.send_from(wrong_source, "198.51.100.99:50001".parse().unwrap());
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        harness.session.status(),
        VoiceStatus::DiscoveringExperimental { variant: 3 }
    );
    while let Some(message) = ws.try_recv() {
        assert_ne!(message["op"], 1);
        assert_ne!(message["op"], 0);
    }
    current.answer_discovery(&first);
    let select = ws.recv_op(1).await;
    assert_eq!(
        select["d"]["data"]["address"],
        external_addr().ip().to_string()
    );
    assert_eq!(select["d"]["data"]["port"], external_addr().port());
    assert_eq!(harness.session.status(), VoiceStatus::Negotiating);
    current.answer_discovery(&first);
    current.answer_discovery(&first);
    ws.send(ready(&BOTH_MODES)).await;
    tokio::time::advance(Duration::from_secs(5)).await;
    tokio::task::yield_now().await;
    assert_eq!(harness.binds.lock().len(), 3);
    assert!(
        current.drain().iter().all(|packet| is_ping(packet)),
        "discovery/prelude continued after Select Protocol"
    );
    while let Some(message) = ws.try_recv() {
        assert_ne!(message["op"], 1, "Select Protocol was sent twice");
        assert_ne!(message["op"], 0, "rotation caused another Identify");
    }
    let stats = harness.session.stats();
    assert!(stats.rejected_source >= 1);
    assert!(stats.rejected_malformed >= 1);
    assert_campaign_caps(&audit.lock());
    harness.session.leave().await;
}

async fn recv_while_acking(udp: &mut Udp, ws: &mut Ws) -> Vec<u8> {
    loop {
        tokio::select! {
            packet=udp.recv()=>return packet,
            heartbeat=ws.recv_raw()=>{
                assert_eq!(heartbeat["op"],3,"unexpected extra Identify/Select Protocol");
                ws.send(json!({"op":6,"d":{"t":heartbeat["d"]["t"]}})).await;
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_pending_prelude_does_not_block_heartbeats_or_leave() {
    let (mut harness, signaling, audit) = audited(true);
    let mut ws = signaling.accept().await;
    begin(&mut ws, 1_000.0).await;
    let mut direct = harness.udp().await;
    for _ in 0..5 {
        assert_discovery(&recv_while_acking(&mut direct, &mut ws).await);
    }
    let _second = loop {
        tokio::select! {
            udp=harness.udp()=>break udp,
            heartbeat=ws.recv_raw()=>{
                assert_eq!(heartbeat["op"],3);
                ws.send(json!({"op":6,"d":{"t":heartbeat["d"]["t"]}})).await;
            }
        }
    };
    tokio::task::yield_now().await;
    let started = Instant::now();
    ws.send(json!({"op":3,"d":null})).await;
    let heartbeat = timeout(Duration::from_millis(100), ws.recv_raw())
        .await
        .expect("pending prelude blocked heartbeat handling");
    assert_eq!(heartbeat["op"], 3);
    ws.send(json!({"op":6,"d":{"t":heartbeat["d"]["t"]}})).await;
    assert!(Instant::now() - started < Duration::from_millis(100));
    let status = harness.session.watch_status();
    let started = Instant::now();
    harness.session.leave().await;
    assert!(
        Instant::now() - started < Duration::from_millis(100),
        "leave waited for the blocked UDP send"
    );
    assert_eq!(*status.borrow(), VoiceStatus::Closed(CloseReason::Left));
    assert_eq!(signaling.urls().len(), 1);
    let audit = audit.lock();
    assert_campaign_caps(&audit);
    assert_eq!(audit.bound.len(), 2);
    assert!(audit.held.is_empty());
    assert!(
        !audit
            .sent
            .iter()
            .any(|(_, _, packet)| packet.len() == PRELUDE_BYTES)
    );
}

#[tokio::test(start_paused = true)]
async fn repeated_websocket_failures_cannot_refill_the_user_join_discovery_budget() {
    let (mut harness, signaling, audit) = audited(false);
    let mut sockets = VecDeque::new();
    for _ in 0..6 {
        sockets.push_back(signaling.accept().await);
    }
    let mut ws = sockets.pop_front().unwrap();
    begin(&mut ws, QUIET_HEARTBEAT_MS).await;
    let started = Instant::now();
    let mut old_flows = Vec::new();
    for preludes in [0, 0, 0] {
        let mut udp = harness.udp().await;
        read_round_start(&mut udp, preludes).await;
        assert_eq!(
            harness.session.status(),
            VoiceStatus::DiscoveringExperimental { variant: 1 },
            "a WebSocket failure must not advance the UDP strategy"
        );
        old_flows.push(udp);
        ws.close(4015).await;
        ws = sockets.pop_front().unwrap();
        ws.hello(QUIET_HEARTBEAT_MS).await;
        assert_eq!(ws.recv_op(0).await["op"], 0);
        ws.send(ready(&BOTH_MODES)).await;
    }
    assert_eq!(harness.closed().await, CloseReason::UdpUnreachable);
    assert!(Instant::now() - started <= Duration::from_secs(16));
    assert_eq!(
        signaling.urls().len(),
        4,
        "discovery exhaustion must not enter MAX_RECONNECTS again"
    );
    let audit = audit.lock();
    assert_campaign_caps(&audit);
    assert_eq!(audit.bound.len(), 3);
    assert_eq!(
        audit
            .sent
            .iter()
            .filter(|(_, _, packet)| packet.len() == DISCOVERY_LEN)
            .count(),
        3
    );
    assert_eq!(
        audit
            .sent
            .iter()
            .filter(|(_, _, packet)| packet.len() == PRELUDE_BYTES)
            .count(),
        0
    );
}

#[tokio::test(start_paused = true)]
async fn the_default_network_never_emits_a_laboratory_prelude() {
    let (mut harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    begin(&mut ws, QUIET_HEARTBEAT_MS).await;
    let mut udp = harness.udp().await;
    let first = udp.recv().await;
    assert_discovery(&first);
    finish_unanswered_round(&mut udp, &first).await;
    assert_eq!(harness.closed().await, CloseReason::UdpUnreachable);
    assert_eq!(harness.binds.lock().len(), 1);
    assert!(
        udp.log
            .iter()
            .all(|packet| packet.len() == DISCOVERY_LEN || is_ping(packet))
    );
    assert_eq!(signaling.urls().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_stalled_bind_or_prelude_still_ends_at_the_join_deadline() {
    for stall_bind in [true, false] {
        let (mut harness, signaling, audit) = audited_with(!stall_bind, stall_bind);
        let mut ws = signaling.accept().await;
        begin(&mut ws, QUIET_HEARTBEAT_MS).await;
        let started = Instant::now();
        if !stall_bind {
            let mut direct = harness.udp().await;
            let first = read_round_start(&mut direct, 0).await;
            finish_unanswered_round(&mut direct, &first).await;
            let _blocked = harness.udp().await;
        }
        assert_eq!(harness.closed().await, CloseReason::UdpUnreachable);
        assert_eq!(Instant::now() - started, Duration::from_secs(16));
        assert_eq!(signaling.urls().len(), 1);
        assert_campaign_caps(&audit.lock());
        assert!(audit.lock().held.is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn a_signaling_drop_after_select_protocol_cannot_restart_discovery() {
    let (mut harness, signaling) = Harness::start_experimental(options());
    let mut ws = signaling.accept().await;
    let _unused_next = signaling.accept().await;
    let (mut udp, _) = negotiate(&mut harness, &mut ws, &BOTH_MODES).await;
    ws.close(4015).await;
    assert_eq!(harness.closed().await, CloseReason::Network);
    assert_eq!(signaling.urls().len(), 1);
    assert_eq!(harness.binds.lock().len(), 1);
    assert!(udp.drain().iter().all(|packet| is_ping(packet)));
}
