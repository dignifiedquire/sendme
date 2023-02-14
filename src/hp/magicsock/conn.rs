use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::atomic::{AtomicBool, AtomicU16},
    time::Duration,
};

use tokio::{
    sync::{self, Mutex, RwLock},
    time::{self, Instant},
};

use crate::hp::{
    cfg,
    derp::{self, DerpMap},
    key, monitor, netcheck, netmap, portmapper,
};

use super::{endpoint::PeerMap, ActiveDerp, ConnBind, Endpoint, RebindingUdpConn};

/// Contains options for `Conn::listen`.
pub struct Options {
    /// The port to listen on.
    // Zero means to pick one automatically.
    pub port: u16,

    /// Optionally provides a func to be called when endpoints change.
    pub on_endpoints: Option<Box<dyn Fn(&Endpoint)>>,

    // Optionally provides a func to be called when a connection is made to a DERP server.
    pub on_derp_active: Option<Box<dyn Fn()>>,

    // Optionally provides a func to return how long it's been since a TUN packet was sent or received.
    pub on_idle: Option<Box<dyn Fn() -> Duration>>,

    /// If provided, is a function for magicsock to call
    /// whenever it receives a packet from a a peer if it's been more
    /// than ~10 seconds since the last one. (10 seconds is somewhat
    /// arbitrary; the sole user just doesn't need or want it called on
    /// every packet, just every minute or two for WireGuard timeouts,
    /// and 10 seconds seems like a good trade-off between often enough
    /// and not too often.)
    /// The provided func is likely to call back into
    /// Conn.ParseEndpoint, which acquires Conn.mu. As such, you should
    /// not hold Conn.mu while calling it.
    pub on_note_recv_activity: Option<Box<dyn Fn(&key::NodePublic)>>,

    /// The link monitor to use. With one, the portmapper won't be used.
    pub link_monitor: Option<monitor::Monitor>,
}

/// Routes UDP packets and actively manages a list of its endpoints.
pub struct Conn {
    options: Options,

    // ================================================================
    // No locking required to access these fields, either because
    // they're static after construction, or are wholly owned by a single goroutine.

    // TODO
    // connCtx:       context.Context, // closed on Conn.Close
    // connCtxCancel: func(),          // closes connCtx
    // donec:         <-chan struct{}, // connCtx.Done()'s to avoid context.cancelCtx.Done()'s mutex per call

    // The underlying UDP sockets used to send/rcv packets for wireguard and other magicsock protocols.
    pconn4: RebindingUdpConn,
    pconn6: RebindingUdpConn,

    // TODO:
    // closeDisco4 and closeDisco6 are io.Closers to shut down the raw
    // disco packet receivers. If nil, no raw disco receiver is running for the given family.
    // closeDisco4 io.Closer
    // closeDisco6 io.Closer
    /// The prober that discovers local network conditions, including the closest DERP relay and NAT mappings.
    net_checker: netcheck::Client,

    /// The NAT-PMP/PCP/UPnP prober/client, for requesting port mappings from NAT devices.
    port_mapper: portmapper::Client,

    /// Holds the current STUN packet processing func.
    stun_receive_func: RwLock<Box<dyn Fn(&[u8], SocketAddr)>>, // syncs.AtomicValue[func(p []byte, fromAddr netip.AddrPort)]

    // TODO:
    // Used by receiveDERP to read DERP messages.
    // It must have buffer size > 0; see issue 3736.
    // derpRecvCh chan derpReadResult
    /// The wireguard-go conn.Bind for Conn.
    bind: ConnBind,

    // TODO:
    // owned by receiveIPv4 and receiveIPv6, respectively, to cache an IPPort->endpoint for hot flows.
    // ippEndpoint4, ippEndpoint6 ippEndpointCache

    // ============================================================
    // Fields that must be accessed via atomic load/stores.
    /// Whether IPv4 and IPv6 are known to be missing.
    /// They're only used to suppress log spam. The name
    /// is named negatively because in early start-up, we don't yet
    /// necessarily have a netcheck.Report and don't want to skip logging.
    no_v4: AtomicBool,
    no_v6: AtomicBool,

    /// Whether IPv4 UDP is known to be unable to transmit
    /// at all. This could happen if the socket is in an invalid state
    /// (as can happen on darwin after a network link status change).
    no_v4_send: AtomicBool,

    /// Whether the network is up (some interface is up
    /// with IPv4 or IPv6). It's used to suppress log spam and prevent new connection that'll fail.
    network_up: AtomicBool,

    /// Whether privateKey is non-zero.
    have_private_key: AtomicBool,
    // TODO:
    // public_key_atomic: syncs.AtomicValue[key.NodePublic] // or NodeKey zero value if !havePrivateKey

    // TODO: add if needed
    // derpMapAtomic is the same as derpMap, but without requiring
    // sync.Mutex. For use with NewRegionClient's callback, to avoid
    // lock ordering deadlocks. See issue 3726 and mu field docs.
    // derpMapAtomic atomic.Pointer[tailcfg.DERPMap]
    last_net_check_report: RwLock<netcheck::Report>,

    /// Preferred port from opts.Port; 0 means auto.
    port: AtomicU16,

    // TODO
    // Maintains per-connection counters. (atomic pointer originally)
    // stats: RwLock<connstats.Statistics>
    /// A callback that provides a `cfg::NetInfo` when discovered network conditions change.
    net_info_func: Box<dyn Fn(&cfg::NetInfo)>,

    //     // ============================================================
    //     // mu guards all following fields; see userspaceEngine lock
    //     // ordering rules against the engine. For derphttp, mu must
    //     // be held before derphttp.Client.mu.
    state: Mutex<ConnState>,
    state_notifier: sync::Notify,
}

struct ConnState {
    /// Close was called
    closed: bool,
    /// Close is in progress (or done)
    closing: AtomicBool,

    /// A timer that fires to occasionally clean up idle DERP connections.
    /// It's only used when there is a non-home DERP connection in use.
    derp_cleanup_timer: time::Interval,

    /// Whether derp_cleanup_timer is scheduled to fire within derp_clean_stale_interval.
    derp_cleanup_timer_armed: bool,
    // When set, is an AfterFunc timer that will call Conn::do_periodic_stun.
    periodic_re_stun_timer: Option<time::Interval>,

    /// Indicates that update_endpoints is currently running. It's used to deduplicate
    /// concurrent endpoint update requests.
    endpoints_update_active: bool,
    /// If set, means that a new endpoints update should begin immediately after the currently-running one
    /// completes. It can only be non-empty if `endpoints_update_active == true`.
    want_endpoints_update: Option<String>, // true if non-empty; string is reason
    /// Records the endpoints found during the previous
    /// endpoint discovery. It's used to avoid duplicate endpoint change notifications.
    last_endpoints: Vec<cfg::Endpoint>,

    /// The last time the endpoints were updated, even if there was no change.
    last_endpoints_time: Instant,

    /// Functions to run (in their own tasks) when endpoints are refreshed.
    on_endpoint_refreshed: HashMap<Endpoint, Box<dyn Fn()>>,

    /// The set of peers that are currently configured in
    /// WireGuard. These are not used to filter inbound or outbound
    /// traffic at all, but only to track what state can be cleaned up
    /// in other maps below that are keyed by peer public key.
    peer_set: HashSet<key::NodePublic>,

    /// The private naclbox key used for active discovery traffic. It's created once near
    /// (but not during) construction.
    disco_private: key::DiscoPrivate,
    /// Public key of disco_private.
    disco_public: key::DiscoPublic,

    /// Tracks the networkmap Node entity for each peer discovery key.
    peer_map: PeerMap,

    // The state for an active DiscoKey.
    disco_info: HashMap<key::DiscoPublic, DiscoInfo>,

    /// The `NetInfo` provided in the last call to `net_info_func`. It's used to deduplicate calls to netInfoFunc.
    net_info_last: Option<cfg::NetInfo>,

    /// None (or zero regions/nodes) means DERP is disabled.
    derp_map: Option<DerpMap>,
    net_map: netmap::NetworkMap,
    /// WireGuard private key for this node
    private_key: key::NodePrivate,
    /// Whether we ever had a non-zero private key
    ever_had_key: bool,
    /// Nearest DERP region ID; 0 means none/unknown.
    my_derp: usize,
    // derp_started chan struct{}      // closed on first connection to DERP; for tests & cleaner Close
    /// DERP regionID -> connection to a node in that region
    active_derp: HashMap<usize, ActiveDerp>,
    prev_derp: HashMap<usize, ()>, //    map[int]*syncs.WaitGroupChan

    /// Contains optional alternate routes to use as an optimization instead of
    /// contacting a peer via their home DERP connection.  If they sent us a message
    /// on a different DERP connection (which should really only be on our DERP
    /// home connection, or what was once our home), then we remember that route here to optimistically
    /// use instead of creating a new DERP connection back to their home.
    derp_route: HashMap<key::NodePublic, DerpRoute>,
}

impl Conn {
    /// Removes a DERP route entry previously added by addDerpPeerRoute.
    async fn remove_derp_peer_route(
        &self,
        peer: key::NodePublic,
        derp_id: usize,
        dc: &derp::http::Client,
    ) {
        let mut state = self.state.lock().await;
        match state.derp_route.entry(peer) {
            std::collections::hash_map::Entry::Occupied(r) => {
                if r.get().derp_id == derp_id && &r.get().dc == dc {
                    r.remove();
                }
            }
            _ => {}
        }
    }

    /// Adds a DERP route entry, noting that peer was seen on DERP node `derp_id`, at least on the
    /// connection identified by `dc`.
    async fn add_derp_peer_route(
        &self,
        peer: key::NodePublic,
        derp_id: usize,
        dc: derp::http::Client,
    ) {
        let mut state = self.state.lock().await;
        state.derp_route.insert(peer, DerpRoute { derp_id, dc });
    }

    // // newConn is the error-free, network-listening-side-effect-free based
    // // of NewConn. Mostly for tests.
    // func newConn() *Conn {
    // 	c := &Conn{
    // 		derpRecvCh:   make(chan derpReadResult, 1), // must be buffered, see issue 3736
    // 		derpStarted:  make(chan struct{}),
    // 		peerLastDerp: make(map[key.NodePublic]int),
    // 		peerMap:      newPeerMap(),
    // 		discoInfo:    make(map[key.DiscoPublic]*discoInfo),
    // 	}
    // 	c.bind = &connBind{Conn: c, closed: true}
    // 	c.receiveBatchPool = sync.Pool{New: func() any {
    // 		msgs := make([]ipv6.Message, c.bind.BatchSize())
    // 		for i := range msgs {
    // 			msgs[i].Buffers = make([][]byte, 1)
    // 		}
    // 		batch := &receiveBatch{
    // 			msgs: msgs,
    // 		}
    // 		return batch
    // 	}}
    // 	c.sendBatchPool = sync.Pool{New: func() any {
    // 		ua := &net.UDPAddr{
    // 			IP: make([]byte, 16),
    // 		}
    // 		msgs := make([]ipv6.Message, c.bind.BatchSize())
    // 		for i := range msgs {
    // 			msgs[i].Buffers = make([][]byte, 1)
    // 			msgs[i].Addr = ua
    // 		}
    // 		return &sendBatch{
    // 			ua:   ua,
    // 			msgs: msgs,
    // 		}
    // 	}}
    // 	c.muCond = sync.NewCond(&c.mu)
    // 	c.networkUp.Store(true) // assume up until told otherwise
    // 	return c
    // }

    // // NewConn creates a magic Conn listening on opts.Port.
    // // As the set of possible endpoints for a Conn changes, the
    // // callback opts.EndpointsFunc is called.
    // func NewConn(opts Options) (*Conn, error) {
    // 	c := newConn()
    // 	c.port.Store(uint32(opts.Port))
    // 	c.logf = opts.logf()
    // 	c.epFunc = opts.endpointsFunc()
    // 	c.derpActiveFunc = opts.derpActiveFunc()
    // 	c.idleFunc = opts.IdleFunc
    // 	c.testOnlyPacketListener = opts.TestOnlyPacketListener
    // 	c.noteRecvActivity = opts.NoteRecvActivity
    // 	c.portMapper = portmapper.NewClient(logger.WithPrefix(c.logf, "portmapper: "), c.onPortMapChanged)
    // 	if opts.LinkMonitor != nil {
    // 		c.portMapper.SetGatewayLookupFunc(opts.LinkMonitor.GatewayAndSelfIP)
    // 	}
    // 	c.linkMon = opts.LinkMonitor

    // 	if err := c.rebind(keepCurrentPort); err != nil {
    // 		return nil, err
    // 	}

    // 	c.connCtx, c.connCtxCancel = context.WithCancel(context.Background())
    // 	c.donec = c.connCtx.Done()
    // 	c.netChecker = &netcheck.Client{
    // 		Logf:                logger.WithPrefix(c.logf, "netcheck: "),
    // 		GetSTUNConn4:        func() netcheck.STUNConn { return &c.pconn4 },
    // 		GetSTUNConn6:        func() netcheck.STUNConn { return &c.pconn6 },
    // 		SkipExternalNetwork: inTest(),
    // 		PortMapper:          c.portMapper,
    // 	}

    // 	c.ignoreSTUNPackets()

    // 	if d4, err := c.listenRawDisco("ip4"); err == nil {
    // 		c.logf("[v1] using BPF disco receiver for IPv4")
    // 		c.closeDisco4 = d4
    // 	} else {
    // 		c.logf("[v1] couldn't create raw v4 disco listener, using regular listener instead: %v", err)
    // 	}
    // 	if d6, err := c.listenRawDisco("ip6"); err == nil {
    // 		c.logf("[v1] using BPF disco receiver for IPv6")
    // 		c.closeDisco6 = d6
    // 	} else {
    // 		c.logf("[v1] couldn't create raw v6 disco listener, using regular listener instead: %v", err)
    // 	}

    // 	return c, nil
    // }

    // // ignoreSTUNPackets sets a STUN packet processing func that does nothing.
    // func (c *Conn) ignoreSTUNPackets() {
    // 	c.stunReceiveFunc.Store(func([]byte, netip.AddrPort) {})
    // }

    // // doPeriodicSTUN is called (in a new goroutine) by
    // // periodicReSTUNTimer when periodic STUNs are active.
    // func (c *Conn) doPeriodicSTUN() { c.ReSTUN("periodic") }

    // func (c *Conn) stopPeriodicReSTUNTimerLocked() {
    // 	if t := c.periodicReSTUNTimer; t != nil {
    // 		t.Stop()
    // 		c.periodicReSTUNTimer = nil
    // 	}
    // }

    // // c.mu must NOT be held.
    // func (c *Conn) updateEndpoints(why string) {
    // 	metricUpdateEndpoints.Add(1)
    // 	defer func() {
    // 		c.mu.Lock()
    // 		defer c.mu.Unlock()
    // 		why := c.wantEndpointsUpdate
    // 		c.wantEndpointsUpdate = ""
    // 		if !c.closed {
    // 			if why != "" {
    // 				go c.updateEndpoints(why)
    // 				return
    // 			}
    // 			if c.shouldDoPeriodicReSTUNLocked() {
    // 				// Pick a random duration between 20
    // 				// and 26 seconds (just under 30s, a
    // 				// common UDP NAT timeout on Linux,
    // 				// etc)
    // 				d := tstime.RandomDurationBetween(20*time.Second, 26*time.Second)
    // 				if t := c.periodicReSTUNTimer; t != nil {
    // 					if debugReSTUNStopOnIdle() {
    // 						c.logf("resetting existing periodicSTUN to run in %v", d)
    // 					}
    // 					t.Reset(d)
    // 				} else {
    // 					if debugReSTUNStopOnIdle() {
    // 						c.logf("scheduling periodicSTUN to run in %v", d)
    // 					}
    // 					c.periodicReSTUNTimer = time.AfterFunc(d, c.doPeriodicSTUN)
    // 				}
    // 			} else {
    // 				if debugReSTUNStopOnIdle() {
    // 					c.logf("periodic STUN idle")
    // 				}
    // 				c.stopPeriodicReSTUNTimerLocked()
    // 			}
    // 		}
    // 		c.endpointsUpdateActive = false
    // 		c.muCond.Broadcast()
    // 	}()
    // 	c.dlogf("[v1] magicsock: starting endpoint update (%s)", why)
    // 	if c.noV4Send.Load() && runtime.GOOS != "js" {
    // 		c.mu.Lock()
    // 		closed := c.closed
    // 		c.mu.Unlock()
    // 		if !closed {
    // 			c.logf("magicsock: last netcheck reported send error. Rebinding.")
    // 			c.Rebind()
    // 		}
    // 	}

    // 	endpoints, err := c.determineEndpoints(c.connCtx)
    // 	if err != nil {
    // 		c.logf("magicsock: endpoint update (%s) failed: %v", why, err)
    // 		// TODO(crawshaw): are there any conditions under which
    // 		// we should trigger a retry based on the error here?
    // 		return
    // 	}

    // 	if c.setEndpoints(endpoints) {
    // 		c.logEndpointChange(endpoints)
    // 		c.epFunc(endpoints)
    // 	}
    // }

    // // setEndpoints records the new endpoints, reporting whether they're changed.
    // // It takes ownership of the slice.
    // func (c *Conn) setEndpoints(endpoints []tailcfg.Endpoint) (changed bool) {
    // 	anySTUN := false
    // 	for _, ep := range endpoints {
    // 		if ep.Type == tailcfg.EndpointSTUN {
    // 			anySTUN = true
    // 		}
    // 	}

    // 	c.mu.Lock()
    // 	defer c.mu.Unlock()

    // 	if !anySTUN && c.derpMap == nil && !inTest() {
    // 		// Don't bother storing or reporting this yet. We
    // 		// don't have a DERP map or any STUN entries, so we're
    // 		// just starting up. A DERP map should arrive shortly
    // 		// and then we'll have more interesting endpoints to
    // 		// report. This saves a map update.
    // 		// TODO(bradfitz): this optimization is currently
    // 		// skipped during the e2e tests because they depend
    // 		// too much on the exact sequence of updates.  Fix the
    // 		// tests. But a protocol rewrite might happen first.
    // 		c.dlogf("[v1] magicsock: ignoring pre-DERP map, STUN-less endpoint update: %v", endpoints)
    // 		return false
    // 	}

    // 	c.lastEndpointsTime = time.Now()
    // 	for de, fn := range c.onEndpointRefreshed {
    // 		go fn()
    // 		delete(c.onEndpointRefreshed, de)
    // 	}

    // 	if endpointSetsEqual(endpoints, c.lastEndpoints) {
    // 		return false
    // 	}
    // 	c.lastEndpoints = endpoints
    // 	return true
    // }

    // // setNetInfoHavePortMap updates NetInfo.HavePortMap to true.
    // func (c *Conn) setNetInfoHavePortMap() {
    // 	c.mu.Lock()
    // 	defer c.mu.Unlock()
    // 	if c.netInfoLast == nil {
    // 		// No NetInfo yet. Nothing to update.
    // 		return
    // 	}
    // 	if c.netInfoLast.HavePortMap {
    // 		// No change.
    // 		return
    // 	}
    // 	ni := c.netInfoLast.Clone()
    // 	ni.HavePortMap = true
    // 	c.callNetInfoCallbackLocked(ni)
    // }

    // func (c *Conn) updateNetInfo(ctx context.Context) (*netcheck.Report, error) {
    // 	c.mu.Lock()
    // 	dm := c.derpMap
    // 	c.mu.Unlock()

    // 	if dm == nil || c.networkDown() {
    // 		return new(netcheck.Report), nil
    // 	}

    // 	ctx, cancel := context.WithTimeout(ctx, 2*time.Second)
    // 	defer cancel()

    // 	c.stunReceiveFunc.Store(c.netChecker.ReceiveSTUNPacket)
    // 	defer c.ignoreSTUNPackets()

    // 	report, err := c.netChecker.GetReport(ctx, dm)
    // 	if err != nil {
    // 		return nil, err
    // 	}

    // 	c.lastNetCheckReport.Store(report)
    // 	c.noV4.Store(!report.IPv4)
    // 	c.noV6.Store(!report.IPv6)
    // 	c.noV4Send.Store(!report.IPv4CanSend)

    // 	ni := &tailcfg.NetInfo{
    // 		DERPLatency:           map[string]float64{},
    // 		MappingVariesByDestIP: report.MappingVariesByDestIP,
    // 		HairPinning:           report.HairPinning,
    // 		UPnP:                  report.UPnP,
    // 		PMP:                   report.PMP,
    // 		PCP:                   report.PCP,
    // 		HavePortMap:           c.portMapper.HaveMapping(),
    // 	}
    // 	for rid, d := range report.RegionV4Latency {
    // 		ni.DERPLatency[fmt.Sprintf("%d-v4", rid)] = d.Seconds()
    // 	}
    // 	for rid, d := range report.RegionV6Latency {
    // 		ni.DERPLatency[fmt.Sprintf("%d-v6", rid)] = d.Seconds()
    // 	}
    // 	ni.WorkingIPv6.Set(report.IPv6)
    // 	ni.OSHasIPv6.Set(report.OSHasIPv6)
    // 	ni.WorkingUDP.Set(report.UDP)
    // 	ni.WorkingICMPv4.Set(report.ICMPv4)
    // 	ni.PreferredDERP = report.PreferredDERP

    // 	if ni.PreferredDERP == 0 {
    // 		// Perhaps UDP is blocked. Pick a deterministic but arbitrary
    // 		// one.
    // 		ni.PreferredDERP = c.pickDERPFallback()
    // 	}
    // 	if !c.setNearestDERP(ni.PreferredDERP) {
    // 		ni.PreferredDERP = 0
    // 	}

    // 	// TODO: set link type

    // 	c.callNetInfoCallback(ni)
    // 	return report, nil
    // }

    // var processStartUnixNano = time.Now().UnixNano()

    // // pickDERPFallback returns a non-zero but deterministic DERP node to
    // // connect to.  This is only used if netcheck couldn't find the
    // // nearest one (for instance, if UDP is blocked and thus STUN latency
    // // checks aren't working).
    // //
    // // c.mu must NOT be held.
    // func (c *Conn) pickDERPFallback() int {
    // 	c.mu.Lock()
    // 	defer c.mu.Unlock()

    // 	if !c.wantDerpLocked() {
    // 		return 0
    // 	}
    // 	ids := c.derpMap.RegionIDs()
    // 	if len(ids) == 0 {
    // 		// No DERP regions in non-nil map.
    // 		return 0
    // 	}

    // 	// TODO: figure out which DERP region most of our peers are using,
    // 	// and use that region as our fallback.
    // 	//
    // 	// If we already had selected something in the past and it has any
    // 	// peers, we want to stay on it. If there are no peers at all,
    // 	// stay on whatever DERP we previously picked. If we need to pick
    // 	// one and have no peer info, pick a region randomly.
    // 	//
    // 	// We used to do the above for legacy clients, but never updated
    // 	// it for disco.

    // 	if c.myDerp != 0 {
    // 		return c.myDerp
    // 	}

    // 	h := fnv.New64()
    // 	fmt.Fprintf(h, "%p/%d", c, processStartUnixNano) // arbitrary
    // 	return ids[rand.New(rand.NewSource(int64(h.Sum64()))).Intn(len(ids))]
    // }

    // // callNetInfoCallback calls the NetInfo callback (if previously
    // // registered with SetNetInfoCallback) if ni has substantially changed
    // // since the last state.
    // //
    // // callNetInfoCallback takes ownership of ni.
    // //
    // // c.mu must NOT be held.
    // func (c *Conn) callNetInfoCallback(ni *tailcfg.NetInfo) {
    // 	c.mu.Lock()
    // 	defer c.mu.Unlock()
    // 	if ni.BasicallyEqual(c.netInfoLast) {
    // 		return
    // 	}
    // 	c.callNetInfoCallbackLocked(ni)
    // }

    // func (c *Conn) callNetInfoCallbackLocked(ni *tailcfg.NetInfo) {
    // 	c.netInfoLast = ni
    // 	if c.netInfoFunc != nil {
    // 		c.dlogf("[v1] magicsock: netInfo update: %+v", ni)
    // 		go c.netInfoFunc(ni)
    // 	}
    // }

    // // addValidDiscoPathForTest makes addr a validated disco address for
    // // discoKey. It's used in tests to enable receiving of packets from
    // // addr without having to spin up the entire active discovery
    // // machinery.
    // func (c *Conn) addValidDiscoPathForTest(nodeKey key.NodePublic, addr netip.AddrPort) {
    // 	c.mu.Lock()
    // 	defer c.mu.Unlock()
    // 	c.peerMap.setNodeKeyForIPPort(addr, nodeKey)
    // }

    // func (c *Conn) SetNetInfoCallback(fn func(*tailcfg.NetInfo)) {
    // 	if fn == nil {
    // 		panic("nil NetInfoCallback")
    // 	}
    // 	c.mu.Lock()
    // 	last := c.netInfoLast
    // 	c.netInfoFunc = fn
    // 	c.mu.Unlock()

    // 	if last != nil {
    // 		fn(last)
    // 	}
    // }

    // // LastRecvActivityOfNodeKey describes the time we last got traffic from
    // // this endpoint (updated every ~10 seconds).
    // func (c *Conn) LastRecvActivityOfNodeKey(nk key.NodePublic) string {
    // 	c.mu.Lock()
    // 	defer c.mu.Unlock()
    // 	de, ok := c.peerMap.endpointForNodeKey(nk)
    // 	if !ok {
    // 		return "never"
    // 	}
    // 	saw := de.lastRecv.LoadAtomic()
    // 	if saw == 0 {
    // 		return "never"
    // 	}
    // 	return mono.Since(saw).Round(time.Second).String()
    // }

    // // Ping handles a "tailscale ping" CLI query.
    // func (c *Conn) Ping(peer *tailcfg.Node, res *ipnstate.PingResult, cb func(*ipnstate.PingResult)) {
    // 	c.mu.Lock()
    // 	defer c.mu.Unlock()
    // 	if c.privateKey.IsZero() {
    // 		res.Err = "local tailscaled stopped"
    // 		cb(res)
    // 		return
    // 	}
    // 	if len(peer.Addresses) > 0 {
    // 		res.NodeIP = peer.Addresses[0].Addr().String()
    // 	}
    // 	res.NodeName = peer.Name // prefer DNS name
    // 	if res.NodeName == "" {
    // 		res.NodeName = peer.Hostinfo.Hostname() // else hostname
    // 	} else {
    // 		res.NodeName, _, _ = strings.Cut(res.NodeName, ".")
    // 	}

    // 	ep, ok := c.peerMap.endpointForNodeKey(peer.Key)
    // 	if !ok {
    // 		res.Err = "unknown peer"
    // 		cb(res)
    // 		return
    // 	}
    // 	ep.cliPing(res, cb)
    // }

    // // c.mu must be held
    // func (c *Conn) populateCLIPingResponseLocked(res *ipnstate.PingResult, latency time.Duration, ep netip.AddrPort) {
    // 	res.LatencySeconds = latency.Seconds()
    // 	if ep.Addr() != derpMagicIPAddr {
    // 		res.Endpoint = ep.String()
    // 		return
    // 	}
    // 	regionID := int(ep.Port())
    // 	res.DERPRegionID = regionID
    // 	res.DERPRegionCode = c.derpRegionCodeLocked(regionID)
    // }

    // func (c *Conn) derpRegionCodeLocked(regionID int) string {
    // 	if c.derpMap == nil {
    // 		return ""
    // 	}
    // 	if dr, ok := c.derpMap.Regions[regionID]; ok {
    // 		return dr.RegionCode
    // 	}
    // 	return ""
    // }

    // // DiscoPublicKey returns the discovery public key.
    // func (c *Conn) DiscoPublicKey() key.DiscoPublic {
    // 	c.mu.Lock()
    // 	defer c.mu.Unlock()
    // 	if c.discoPrivate.IsZero() {
    // 		priv := key.NewDisco()
    // 		c.discoPrivate = priv
    // 		c.discoPublic = priv.Public()
    // 		c.discoShort = c.discoPublic.ShortString()
    // 		c.logf("magicsock: disco key = %v", c.discoShort)
    // 	}
    // 	return c.discoPublic
    // }

    // // c.mu must NOT be held.
    // func (c *Conn) setNearestDERP(derpNum int) (wantDERP bool) {
    // 	c.mu.Lock()
    // 	defer c.mu.Unlock()
    // 	if !c.wantDerpLocked() {
    // 		c.myDerp = 0
    // 		health.SetMagicSockDERPHome(0)
    // 		return false
    // 	}
    // 	if derpNum == c.myDerp {
    // 		// No change.
    // 		return true
    // 	}
    // 	if c.myDerp != 0 && derpNum != 0 {
    // 		metricDERPHomeChange.Add(1)
    // 	}
    // 	c.myDerp = derpNum
    // 	health.SetMagicSockDERPHome(derpNum)

    // 	if c.privateKey.IsZero() {
    // 		// No private key yet, so DERP connections won't come up anyway.
    // 		// Return early rather than ultimately log a couple lines of noise.
    // 		return true
    // 	}

    // 	// On change, notify all currently connected DERP servers and
    // 	// start connecting to our home DERP if we are not already.
    // 	dr := c.derpMap.Regions[derpNum]
    // 	if dr == nil {
    // 		c.logf("[unexpected] magicsock: derpMap.Regions[%v] is nil", derpNum)
    // 	} else {
    // 		c.logf("magicsock: home is now derp-%v (%v)", derpNum, c.derpMap.Regions[derpNum].RegionCode)
    // 	}
    // 	for i, ad := range c.activeDerp {
    // 		go ad.c.NotePreferred(i == c.myDerp)
    // 	}
    // 	c.goDerpConnect(derpNum)
    // 	return true
    // }

    // // startDerpHomeConnectLocked starts connecting to our DERP home, if any.
    // //
    // // c.mu must be held.
    // func (c *Conn) startDerpHomeConnectLocked() {
    // 	c.goDerpConnect(c.myDerp)
    // }

    // // goDerpConnect starts a goroutine to start connecting to the given
    // // DERP node.
    // //
    // // c.mu may be held, but does not need to be.
    // func (c *Conn) goDerpConnect(node int) {
    // 	if node == 0 {
    // 		return
    // 	}
    // 	go c.derpWriteChanOfAddr(netip.AddrPortFrom(derpMagicIPAddr, uint16(node)), key.NodePublic{})
    // }

    // // determineEndpoints returns the machine's endpoint addresses. It
    // // does a STUN lookup (via netcheck) to determine its public address.
    // //
    // // c.mu must NOT be held.
    // func (c *Conn) determineEndpoints(ctx context.Context) ([]tailcfg.Endpoint, error) {
    // 	var havePortmap bool
    // 	var portmapExt netip.AddrPort
    // 	if runtime.GOOS != "js" {
    // 		portmapExt, havePortmap = c.portMapper.GetCachedMappingOrStartCreatingOne()
    // 	}

    // 	nr, err := c.updateNetInfo(ctx)
    // 	if err != nil {
    // 		c.logf("magicsock.Conn.determineEndpoints: updateNetInfo: %v", err)
    // 		return nil, err
    // 	}

    // 	if runtime.GOOS == "js" {
    // 		// TODO(bradfitz): why does control require an
    // 		// endpoint? Otherwise it doesn't stream map responses
    // 		// back.
    // 		return []tailcfg.Endpoint{
    // 			{
    // 				Addr: netip.MustParseAddrPort("[fe80:123:456:789::1]:12345"),
    // 				Type: tailcfg.EndpointLocal,
    // 			},
    // 		}, nil
    // 	}

    // 	var already map[netip.AddrPort]tailcfg.EndpointType // endpoint -> how it was found
    // 	var eps []tailcfg.Endpoint                          // unique endpoints

    // 	ipp := func(s string) (ipp netip.AddrPort) {
    // 		ipp, _ = netip.ParseAddrPort(s)
    // 		return
    // 	}
    // 	addAddr := func(ipp netip.AddrPort, et tailcfg.EndpointType) {
    // 		if !ipp.IsValid() || (debugOmitLocalAddresses() && et == tailcfg.EndpointLocal) {
    // 			return
    // 		}
    // 		if _, ok := already[ipp]; !ok {
    // 			mak.Set(&already, ipp, et)
    // 			eps = append(eps, tailcfg.Endpoint{Addr: ipp, Type: et})
    // 		}
    // 	}

    // 	// If we didn't have a portmap earlier, maybe it's done by now.
    // 	if !havePortmap {
    // 		portmapExt, havePortmap = c.portMapper.GetCachedMappingOrStartCreatingOne()
    // 	}
    // 	if havePortmap {
    // 		addAddr(portmapExt, tailcfg.EndpointPortmapped)
    // 		c.setNetInfoHavePortMap()
    // 	}

    // 	if nr.GlobalV4 != "" {
    // 		addAddr(ipp(nr.GlobalV4), tailcfg.EndpointSTUN)

    // 		// If they're behind a hard NAT and are using a fixed
    // 		// port locally, assume they might've added a static
    // 		// port mapping on their router to the same explicit
    // 		// port that tailscaled is running with. Worst case
    // 		// it's an invalid candidate mapping.
    // 		if port := c.port.Load(); nr.MappingVariesByDestIP.EqualBool(true) && port != 0 {
    // 			if ip, _, err := net.SplitHostPort(nr.GlobalV4); err == nil {
    // 				addAddr(ipp(net.JoinHostPort(ip, strconv.Itoa(int(port)))), tailcfg.EndpointSTUN4LocalPort)
    // 			}
    // 		}
    // 	}
    // 	if nr.GlobalV6 != "" {
    // 		addAddr(ipp(nr.GlobalV6), tailcfg.EndpointSTUN)
    // 	}

    // 	c.ignoreSTUNPackets()

    // 	if localAddr := c.pconn4.LocalAddr(); localAddr.IP.IsUnspecified() {
    // 		ips, loopback, err := interfaces.LocalAddresses()
    // 		if err != nil {
    // 			return nil, err
    // 		}
    // 		if len(ips) == 0 && len(eps) == 0 {
    // 			// Only include loopback addresses if we have no
    // 			// interfaces at all to use as endpoints and don't
    // 			// have a public IPv4 or IPv6 address. This allows
    // 			// for localhost testing when you're on a plane and
    // 			// offline, for example.
    // 			ips = loopback
    // 		}
    // 		for _, ip := range ips {
    // 			addAddr(netip.AddrPortFrom(ip, uint16(localAddr.Port)), tailcfg.EndpointLocal)
    // 		}
    // 	} else {
    // 		// Our local endpoint is bound to a particular address.
    // 		// Do not offer addresses on other local interfaces.
    // 		addAddr(ipp(localAddr.String()), tailcfg.EndpointLocal)
    // 	}

    // 	// Note: the endpoints are intentionally returned in priority order,
    // 	// from "farthest but most reliable" to "closest but least
    // 	// reliable." Addresses returned from STUN should be globally
    // 	// addressable, but might go farther on the network than necessary.
    // 	// Local interface addresses might have lower latency, but not be
    // 	// globally addressable.
    // 	//
    // 	// The STUN address(es) are always first so that legacy wireguard
    // 	// can use eps[0] as its only known endpoint address (although that's
    // 	// obviously non-ideal).
    // 	//
    // 	// Despite this sorting, though, clients since 0.100 haven't relied
    // 	// on the sorting order for any decisions.
    // 	return eps, nil
    // }

    // // endpointSetsEqual reports whether x and y represent the same set of
    // // endpoints. The order doesn't matter.
    // //
    // // It does not mutate the slices.
    // func endpointSetsEqual(x, y []tailcfg.Endpoint) bool {
    // 	if len(x) == len(y) {
    // 		orderMatches := true
    // 		for i := range x {
    // 			if x[i] != y[i] {
    // 				orderMatches = false
    // 				break
    // 			}
    // 		}
    // 		if orderMatches {
    // 			return true
    // 		}
    // 	}
    // 	m := map[tailcfg.Endpoint]int{}
    // 	for _, v := range x {
    // 		m[v] |= 1
    // 	}
    // 	for _, v := range y {
    // 		m[v] |= 2
    // 	}
    // 	for _, n := range m {
    // 		if n != 3 {
    // 			return false
    // 		}
    // 	}
    // 	return true
    // }

    // // LocalPort returns the current IPv4 listener's port number.
    // func (c *Conn) LocalPort() uint16 {
    // 	if runtime.GOOS == "js" {
    // 		return 12345
    // 	}
    // 	laddr := c.pconn4.LocalAddr()
    // 	return uint16(laddr.Port)
    // }

    // var errNetworkDown = errors.New("magicsock: network down")

    // func (c *Conn) networkDown() bool { return !c.networkUp.Load() }

    // func (c *Conn) Send(buffs [][]byte, ep conn.Endpoint) error {
    // 	n := int64(len(buffs))
    // 	metricSendData.Add(n)
    // 	if c.networkDown() {
    // 		metricSendDataNetworkDown.Add(n)
    // 		return errNetworkDown
    // 	}
    // 	return ep.(*endpoint).send(buffs)
    // }

    // var errConnClosed = errors.New("Conn closed")

    // var errDropDerpPacket = errors.New("too many DERP packets queued; dropping")

    // var errNoUDP = errors.New("no UDP available on platform")

    // var (
    // 	// This acts as a compile-time check for our usage of ipv6.Message in
    // 	// udpConnWithBatchOps for both IPv6 and IPv4 operations.
    // 	_ ipv6.Message = ipv4.Message{}
    // )

    // type sendBatch struct {
    // 	ua   *net.UDPAddr
    // 	msgs []ipv6.Message // ipv4.Message and ipv6.Message are the same underlying type
    // }

    // func (c *Conn) sendUDPBatch(addr netip.AddrPort, buffs [][]byte) (sent bool, err error) {
    // 	batch := c.sendBatchPool.Get().(*sendBatch)
    // 	defer c.sendBatchPool.Put(batch)

    // 	isIPv6 := false
    // 	switch {
    // 	case addr.Addr().Is4():
    // 	case addr.Addr().Is6():
    // 		isIPv6 = true
    // 	default:
    // 		panic("bogus sendUDPBatch addr type")
    // 	}

    // 	as16 := addr.Addr().As16()
    // 	copy(batch.ua.IP, as16[:])
    // 	batch.ua.Port = int(addr.Port())
    // 	for i, buff := range buffs {
    // 		batch.msgs[i].Buffers[0] = buff
    // 		batch.msgs[i].Addr = batch.ua
    // 	}

    // 	if isIPv6 {
    // 		_, err = c.pconn6.WriteBatch(batch.msgs[:len(buffs)], 0)
    // 	} else {
    // 		_, err = c.pconn4.WriteBatch(batch.msgs[:len(buffs)], 0)
    // 	}
    // 	return err == nil, err
    // }

    // // sendUDP sends UDP packet b to ipp.
    // // See sendAddr's docs on the return value meanings.
    // func (c *Conn) sendUDP(ipp netip.AddrPort, b []byte) (sent bool, err error) {
    // 	if runtime.GOOS == "js" {
    // 		return false, errNoUDP
    // 	}
    // 	sent, err = c.sendUDPStd(ipp, b)
    // 	if err != nil {
    // 		metricSendUDPError.Add(1)
    // 	} else {
    // 		if sent {
    // 			metricSendUDP.Add(1)
    // 		}
    // 	}
    // 	return
    // }

    // // sendUDP sends UDP packet b to addr.
    // // See sendAddr's docs on the return value meanings.
    // func (c *Conn) sendUDPStd(addr netip.AddrPort, b []byte) (sent bool, err error) {
    // 	switch {
    // 	case addr.Addr().Is4():
    // 		_, err = c.pconn4.WriteToUDPAddrPort(b, addr)
    // 		if err != nil && (c.noV4.Load() || neterror.TreatAsLostUDP(err)) {
    // 			return false, nil
    // 		}
    // 	case addr.Addr().Is6():
    // 		_, err = c.pconn6.WriteToUDPAddrPort(b, addr)
    // 		if err != nil && (c.noV6.Load() || neterror.TreatAsLostUDP(err)) {
    // 			return false, nil
    // 		}
    // 	default:
    // 		panic("bogus sendUDPStd addr type")
    // 	}
    // 	return err == nil, err
    // }

    // // sendAddr sends packet b to addr, which is either a real UDP address
    // // or a fake UDP address representing a DERP server (see derpmap.go).
    // // The provided public key identifies the recipient.
    // //
    // // The returned err is whether there was an error writing when it
    // // should've worked.
    // // The returned sent is whether a packet went out at all.
    // // An example of when they might be different: sending to an
    // // IPv6 address when the local machine doesn't have IPv6 support
    // // returns (false, nil); it's not an error, but nothing was sent.
    // func (c *Conn) sendAddr(addr netip.AddrPort, pubKey key.NodePublic, b []byte) (sent bool, err error) {
    // 	if addr.Addr() != derpMagicIPAddr {
    // 		return c.sendUDP(addr, b)
    // 	}

    // 	ch := c.derpWriteChanOfAddr(addr, pubKey)
    // 	if ch == nil {
    // 		metricSendDERPErrorChan.Add(1)
    // 		return false, nil
    // 	}

    // 	// TODO(bradfitz): this makes garbage for now; we could use a
    // 	// buffer pool later.  Previously we passed ownership of this
    // 	// to derpWriteRequest and waited for derphttp.Client.Send to
    // 	// complete, but that's too slow while holding wireguard-go
    // 	// internal locks.
    // 	pkt := make([]byte, len(b))
    // 	copy(pkt, b)

    // 	select {
    // 	case <-c.donec:
    // 		metricSendDERPErrorClosed.Add(1)
    // 		return false, errConnClosed
    // 	case ch <- derpWriteRequest{addr, pubKey, pkt}:
    // 		metricSendDERPQueued.Add(1)
    // 		return true, nil
    // 	default:
    // 		metricSendDERPErrorQueue.Add(1)
    // 		// Too many writes queued. Drop packet.
    // 		return false, errDropDerpPacket
    // 	}
    // }

    // // bufferedDerpWritesBeforeDrop is how many packets writes can be
    // // queued up the DERP client to write on the wire before we start
    // // dropping.
    // //
    // // TODO: this is currently arbitrary. Figure out something better?
    // const bufferedDerpWritesBeforeDrop = 32

    // // derpWriteChanOfAddr returns a DERP client for fake UDP addresses that
    // // represent DERP servers, creating them as necessary. For real UDP
    // // addresses, it returns nil.
    // //
    // // If peer is non-zero, it can be used to find an active reverse
    // // path, without using addr.
    // func (c *Conn) derpWriteChanOfAddr(addr netip.AddrPort, peer key.NodePublic) chan<- derpWriteRequest {
    // 	if addr.Addr() != derpMagicIPAddr {
    // 		return nil
    // 	}
    // 	regionID := int(addr.Port())

    // 	if c.networkDown() {
    // 		return nil
    // 	}

    // 	c.mu.Lock()
    // 	defer c.mu.Unlock()
    // 	if !c.wantDerpLocked() || c.closed {
    // 		return nil
    // 	}
    // 	if c.derpMap == nil || c.derpMap.Regions[regionID] == nil {
    // 		return nil
    // 	}
    // 	if c.privateKey.IsZero() {
    // 		c.logf("magicsock: DERP lookup of %v with no private key; ignoring", addr)
    // 		return nil
    // 	}

    // 	// See if we have a connection open to that DERP node ID
    // 	// first. If so, might as well use it. (It's a little
    // 	// arbitrary whether we use this one vs. the reverse route
    // 	// below when we have both.)
    // 	ad, ok := c.activeDerp[regionID]
    // 	if ok {
    // 		*ad.lastWrite = time.Now()
    // 		c.setPeerLastDerpLocked(peer, regionID, regionID)
    // 		return ad.writeCh
    // 	}

    // 	// If we don't have an open connection to the peer's home DERP
    // 	// node, see if we have an open connection to a DERP node
    // 	// where we'd heard from that peer already. For instance,
    // 	// perhaps peer's home is Frankfurt, but they dialed our home DERP
    // 	// node in SF to reach us, so we can reply to them using our
    // 	// SF connection rather than dialing Frankfurt. (Issue 150)
    // 	if !peer.IsZero() && useDerpRoute() {
    // 		if r, ok := c.derpRoute[peer]; ok {
    // 			if ad, ok := c.activeDerp[r.derpID]; ok && ad.c == r.dc {
    // 				c.setPeerLastDerpLocked(peer, r.derpID, regionID)
    // 				*ad.lastWrite = time.Now()
    // 				return ad.writeCh
    // 			}
    // 		}
    // 	}

    // 	why := "home-keep-alive"
    // 	if !peer.IsZero() {
    // 		why = peer.ShortString()
    // 	}
    // 	c.logf("magicsock: adding connection to derp-%v for %v", regionID, why)

    // 	firstDerp := false
    // 	if c.activeDerp == nil {
    // 		firstDerp = true
    // 		c.activeDerp = make(map[int]activeDerp)
    // 		c.prevDerp = make(map[int]*syncs.WaitGroupChan)
    // 	}

    // 	// Note that derphttp.NewRegionClient does not dial the server
    // 	// (it doesn't block) so it is safe to do under the c.mu lock.
    // 	dc := derphttp.NewRegionClient(c.privateKey, c.logf, func() *tailcfg.DERPRegion {
    // 		// Warning: it is not legal to acquire
    // 		// magicsock.Conn.mu from this callback.
    // 		// It's run from derphttp.Client.connect (via Send, etc)
    // 		// and the lock ordering rules are that magicsock.Conn.mu
    // 		// must be acquired before derphttp.Client.mu.
    // 		// See https://github.com/tailscale/tailscale/issues/3726
    // 		if c.connCtx.Err() != nil {
    // 			// We're closing anyway; return nil to stop dialing.
    // 			return nil
    // 		}
    // 		derpMap := c.derpMapAtomic.Load()
    // 		if derpMap == nil {
    // 			return nil
    // 		}
    // 		return derpMap.Regions[regionID]
    // 	})

    // 	dc.SetCanAckPings(true)
    // 	dc.NotePreferred(c.myDerp == regionID)
    // 	dc.SetAddressFamilySelector(derpAddrFamSelector{c})
    // 	dc.DNSCache = dnscache.Get()

    // 	ctx, cancel := context.WithCancel(c.connCtx)
    // 	ch := make(chan derpWriteRequest, bufferedDerpWritesBeforeDrop)

    // 	ad.c = dc
    // 	ad.writeCh = ch
    // 	ad.cancel = cancel
    // 	ad.lastWrite = new(time.Time)
    // 	*ad.lastWrite = time.Now()
    // 	ad.createTime = time.Now()
    // 	c.activeDerp[regionID] = ad
    // 	metricNumDERPConns.Set(int64(len(c.activeDerp)))
    // 	c.logActiveDerpLocked()
    // 	c.setPeerLastDerpLocked(peer, regionID, regionID)
    // 	c.scheduleCleanStaleDerpLocked()

    // 	// Build a startGate for the derp reader+writer
    // 	// goroutines, so they don't start running until any
    // 	// previous generation is closed.
    // 	startGate := syncs.ClosedChan()
    // 	if prev := c.prevDerp[regionID]; prev != nil {
    // 		startGate = prev.DoneChan()
    // 	}
    // 	// And register a WaitGroup(Chan) for this generation.
    // 	wg := syncs.NewWaitGroupChan()
    // 	wg.Add(2)
    // 	c.prevDerp[regionID] = wg

    // 	if firstDerp {
    // 		startGate = c.derpStarted
    // 		go func() {
    // 			dc.Connect(ctx)
    // 			close(c.derpStarted)
    // 			c.muCond.Broadcast()
    // 		}()
    // 	}

    // 	go c.runDerpReader(ctx, addr, dc, wg, startGate)
    // 	go c.runDerpWriter(ctx, dc, ch, wg, startGate)
    // 	go c.derpActiveFunc()

    // 	return ad.writeCh
    // }

    // // setPeerLastDerpLocked notes that peer is now being written to via
    // // the provided DERP regionID, and that the peer advertises a DERP
    // // home region ID of homeID.
    // //
    // // If there's any change, it logs.
    // //
    // // c.mu must be held.
    // func (c *Conn) setPeerLastDerpLocked(peer key.NodePublic, regionID, homeID int) {
    // 	if peer.IsZero() {
    // 		return
    // 	}
    // 	old := c.peerLastDerp[peer]
    // 	if old == regionID {
    // 		return
    // 	}
    // 	c.peerLastDerp[peer] = regionID

    // 	var newDesc string
    // 	switch {
    // 	case regionID == homeID && regionID == c.myDerp:
    // 		newDesc = "shared home"
    // 	case regionID == homeID:
    // 		newDesc = "their home"
    // 	case regionID == c.myDerp:
    // 		newDesc = "our home"
    // 	case regionID != homeID:
    // 		newDesc = "alt"
    // 	}
    // 	if old == 0 {
    // 		c.logf("[v1] magicsock: derp route for %s set to derp-%d (%s)", peer.ShortString(), regionID, newDesc)
    // 	} else {
    // 		c.logf("[v1] magicsock: derp route for %s changed from derp-%d => derp-%d (%s)", peer.ShortString(), old, regionID, newDesc)
    // 	}
    // }

    // // derpReadResult is the type sent by runDerpClient to ReceiveIPv4
    // // when a DERP packet is available.
    // //
    // // Notably, it doesn't include the derp.ReceivedPacket because we
    // // don't want to give the receiver access to the aliased []byte.  To
    // // get at the packet contents they need to call copyBuf to copy it
    // // out, which also releases the buffer.
    // type derpReadResult struct {
    // 	regionID int
    // 	n        int // length of data received
    // 	src      key.NodePublic
    // 	// copyBuf is called to copy the data to dst.  It returns how
    // 	// much data was copied, which will be n if dst is large
    // 	// enough. copyBuf can only be called once.
    // 	// If copyBuf is nil, that's a signal from the sender to ignore
    // 	// this message.
    // 	copyBuf func(dst []byte) int
    // }

    // // runDerpReader runs in a goroutine for the life of a DERP
    // // connection, handling received packets.
    // func (c *Conn) runDerpReader(ctx context.Context, derpFakeAddr netip.AddrPort, dc *derphttp.Client, wg *syncs.WaitGroupChan, startGate <-chan struct{}) {
    // 	defer wg.Decr()
    // 	defer dc.Close()

    // 	select {
    // 	case <-startGate:
    // 	case <-ctx.Done():
    // 		return
    // 	}

    // 	didCopy := make(chan struct{}, 1)
    // 	regionID := int(derpFakeAddr.Port())
    // 	res := derpReadResult{regionID: regionID}
    // 	var pkt derp.ReceivedPacket
    // 	res.copyBuf = func(dst []byte) int {
    // 		n := copy(dst, pkt.Data)
    // 		didCopy <- struct{}{}
    // 		return n
    // 	}

    // 	defer health.SetDERPRegionConnectedState(regionID, false)
    // 	defer health.SetDERPRegionHealth(regionID, "")

    // 	// peerPresent is the set of senders we know are present on this
    // 	// connection, based on messages we've received from the server.
    // 	peerPresent := map[key.NodePublic]bool{}
    // 	bo := backoff.NewBackoff(fmt.Sprintf("derp-%d", regionID), c.logf, 5*time.Second)
    // 	var lastPacketTime time.Time
    // 	var lastPacketSrc key.NodePublic

    // 	for {
    // 		msg, connGen, err := dc.RecvDetail()
    // 		if err != nil {
    // 			health.SetDERPRegionConnectedState(regionID, false)
    // 			// Forget that all these peers have routes.
    // 			for peer := range peerPresent {
    // 				delete(peerPresent, peer)
    // 				c.removeDerpPeerRoute(peer, regionID, dc)
    // 			}
    // 			if err == derphttp.ErrClientClosed {
    // 				return
    // 			}
    // 			if c.networkDown() {
    // 				c.logf("[v1] magicsock: derp.Recv(derp-%d): network down, closing", regionID)
    // 				return
    // 			}
    // 			select {
    // 			case <-ctx.Done():
    // 				return
    // 			default:
    // 			}

    // 			c.logf("magicsock: [%p] derp.Recv(derp-%d): %v", dc, regionID, err)

    // 			// If our DERP connection broke, it might be because our network
    // 			// conditions changed. Start that check.
    // 			c.ReSTUN("derp-recv-error")

    // 			// Back off a bit before reconnecting.
    // 			bo.BackOff(ctx, err)
    // 			select {
    // 			case <-ctx.Done():
    // 				return
    // 			default:
    // 			}
    // 			continue
    // 		}
    // 		bo.BackOff(ctx, nil) // reset

    // 		now := time.Now()
    // 		if lastPacketTime.IsZero() || now.Sub(lastPacketTime) > 5*time.Second {
    // 			health.NoteDERPRegionReceivedFrame(regionID)
    // 			lastPacketTime = now
    // 		}

    // 		switch m := msg.(type) {
    // 		case derp.ServerInfoMessage:
    // 			health.SetDERPRegionConnectedState(regionID, true)
    // 			health.SetDERPRegionHealth(regionID, "") // until declared otherwise
    // 			c.logf("magicsock: derp-%d connected; connGen=%v", regionID, connGen)
    // 			continue
    // 		case derp.ReceivedPacket:
    // 			pkt = m
    // 			res.n = len(m.Data)
    // 			res.src = m.Source
    // 			if logDerpVerbose() {
    // 				c.logf("magicsock: got derp-%v packet: %q", regionID, m.Data)
    // 			}
    // 			// If this is a new sender we hadn't seen before, remember it and
    // 			// register a route for this peer.
    // 			if res.src != lastPacketSrc { // avoid map lookup w/ high throughput single peer
    // 				lastPacketSrc = res.src
    // 				if _, ok := peerPresent[res.src]; !ok {
    // 					peerPresent[res.src] = true
    // 					c.addDerpPeerRoute(res.src, regionID, dc)
    // 				}
    // 			}
    // 		case derp.PingMessage:
    // 			// Best effort reply to the ping.
    // 			pingData := [8]byte(m)
    // 			go func() {
    // 				if err := dc.SendPong(pingData); err != nil {
    // 					c.logf("magicsock: derp-%d SendPong error: %v", regionID, err)
    // 				}
    // 			}()
    // 			continue
    // 		case derp.HealthMessage:
    // 			health.SetDERPRegionHealth(regionID, m.Problem)
    // 		case derp.PeerGoneMessage:
    // 			c.removeDerpPeerRoute(key.NodePublic(m), regionID, dc)
    // 		default:
    // 			// Ignore.
    // 			continue
    // 		}

    // 		select {
    // 		case <-ctx.Done():
    // 			return
    // 		case c.derpRecvCh <- res:
    // 		}

    // 		select {
    // 		case <-ctx.Done():
    // 			return
    // 		case <-didCopy:
    // 			continue
    // 		}
    // 	}
    // }

    // type derpWriteRequest struct {
    // 	addr   netip.AddrPort
    // 	pubKey key.NodePublic
    // 	b      []byte // copied; ownership passed to receiver
    // }

    // // runDerpWriter runs in a goroutine for the life of a DERP
    // // connection, handling received packets.
    // func (c *Conn) runDerpWriter(ctx context.Context, dc *derphttp.Client, ch <-chan derpWriteRequest, wg *syncs.WaitGroupChan, startGate <-chan struct{}) {
    // 	defer wg.Decr()
    // 	select {
    // 	case <-startGate:
    // 	case <-ctx.Done():
    // 		return
    // 	}

    // 	for {
    // 		select {
    // 		case <-ctx.Done():
    // 			return
    // 		case wr := <-ch:
    // 			err := dc.Send(wr.pubKey, wr.b)
    // 			if err != nil {
    // 				c.logf("magicsock: derp.Send(%v): %v", wr.addr, err)
    // 				metricSendDERPError.Add(1)
    // 			} else {
    // 				metricSendDERP.Add(1)
    // 			}
    // 		}
    // 	}
    // }

    // type receiveBatch struct {
    // 	msgs []ipv6.Message
    // }

    // func (c *Conn) getReceiveBatch() *receiveBatch {
    // 	batch := c.receiveBatchPool.Get().(*receiveBatch)
    // 	return batch
    // }

    // func (c *Conn) putReceiveBatch(batch *receiveBatch) {
    // 	for i := range batch.msgs {
    // 		batch.msgs[i] = ipv6.Message{Buffers: batch.msgs[i].Buffers}
    // 	}
    // 	c.receiveBatchPool.Put(batch)
    // }

    // func (c *Conn) receiveIPv6(buffs [][]byte, sizes []int, eps []conn.Endpoint) (int, error) {
    // 	health.ReceiveIPv6.Enter()
    // 	defer health.ReceiveIPv6.Exit()

    // 	batch := c.getReceiveBatch()
    // 	defer c.putReceiveBatch(batch)
    // 	for {
    // 		for i := range buffs {
    // 			batch.msgs[i].Buffers[0] = buffs[i]
    // 		}
    // 		numMsgs, err := c.pconn6.ReadBatch(batch.msgs, 0)
    // 		if err != nil {
    // 			if neterror.PacketWasTruncated(err) {
    // 				// TODO(raggi): discuss whether to log?
    // 				continue
    // 			}
    // 			return 0, err
    // 		}

    // 		reportToCaller := false
    // 		for i, msg := range batch.msgs[:numMsgs] {
    // 			ipp := msg.Addr.(*net.UDPAddr).AddrPort()
    // 			if ep, ok := c.receiveIP(msg.Buffers[0][:msg.N], ipp, &c.ippEndpoint6); ok {
    // 				metricRecvDataIPv6.Add(1)
    // 				eps[i] = ep
    // 				sizes[i] = msg.N
    // 				reportToCaller = true
    // 			} else {
    // 				sizes[i] = 0
    // 			}
    // 		}

    // 		if reportToCaller {
    // 			return numMsgs, nil
    // 		}
    // 	}
    // }

    // func (c *Conn) receiveIPv4(buffs [][]byte, sizes []int, eps []conn.Endpoint) (int, error) {
    // 	health.ReceiveIPv4.Enter()
    // 	defer health.ReceiveIPv4.Exit()

    // 	batch := c.getReceiveBatch()
    // 	defer c.putReceiveBatch(batch)
    // 	for {
    // 		for i := range buffs {
    // 			batch.msgs[i].Buffers[0] = buffs[i]
    // 		}
    // 		numMsgs, err := c.pconn4.ReadBatch(batch.msgs, 0)
    // 		if err != nil {
    // 			if neterror.PacketWasTruncated(err) {
    // 				// TODO(raggi): discuss whether to log?
    // 				continue
    // 			}
    // 			return 0, err
    // 		}

    // 		reportToCaller := false
    // 		for i, msg := range batch.msgs[:numMsgs] {
    // 			ipp := msg.Addr.(*net.UDPAddr).AddrPort()
    // 			if ep, ok := c.receiveIP(msg.Buffers[0][:msg.N], ipp, &c.ippEndpoint4); ok {
    // 				metricRecvDataIPv4.Add(1)
    // 				eps[i] = ep
    // 				sizes[i] = msg.N
    // 				reportToCaller = true
    // 			} else {
    // 				sizes[i] = 0
    // 			}
    // 		}
    // 		if reportToCaller {
    // 			return numMsgs, nil
    // 		}
    // 	}
    // }

    // // receiveIP is the shared bits of ReceiveIPv4 and ReceiveIPv6.
    // //
    // // ok is whether this read should be reported up to wireguard-go (our
    // // caller).
    // func (c *Conn) receiveIP(b []byte, ipp netip.AddrPort, cache *ippEndpointCache) (ep *endpoint, ok bool) {
    // 	if stun.Is(b) {
    // 		c.stunReceiveFunc.Load()(b, ipp)
    // 		return nil, false
    // 	}
    // 	if c.handleDiscoMessage(b, ipp, key.NodePublic{}) {
    // 		return nil, false
    // 	}
    // 	if !c.havePrivateKey.Load() {
    // 		// If we have no private key, we're logged out or
    // 		// stopped. Don't try to pass these wireguard packets
    // 		// up to wireguard-go; it'll just complain (issue 1167).
    // 		return nil, false
    // 	}
    // 	if cache.ipp == ipp && cache.de != nil && cache.gen == cache.de.numStopAndReset() {
    // 		ep = cache.de
    // 	} else {
    // 		c.mu.Lock()
    // 		de, ok := c.peerMap.endpointForIPPort(ipp)
    // 		c.mu.Unlock()
    // 		if !ok {
    // 			return nil, false
    // 		}
    // 		cache.ipp = ipp
    // 		cache.de = de
    // 		cache.gen = de.numStopAndReset()
    // 		ep = de
    // 	}
    // 	ep.noteRecvActivity()
    // 	if stats := c.stats.Load(); stats != nil {
    // 		stats.UpdateRxPhysical(ep.nodeAddr, ipp, len(b))
    // 	}
    // 	return ep, true
    // }

    // func (c *connBind) receiveDERP(buffs [][]byte, sizes []int, eps []conn.Endpoint) (int, error) {
    // 	health.ReceiveDERP.Enter()
    // 	defer health.ReceiveDERP.Exit()

    // 	for dm := range c.derpRecvCh {
    // 		if c.Closed() {
    // 			break
    // 		}
    // 		n, ep := c.processDERPReadResult(dm, buffs[0])
    // 		if n == 0 {
    // 			// No data read occurred. Wait for another packet.
    // 			continue
    // 		}
    // 		metricRecvDataDERP.Add(1)
    // 		sizes[0] = n
    // 		eps[0] = ep
    // 		return 1, nil
    // 	}
    // 	return 0, net.ErrClosed
    // }

    // func (c *Conn) processDERPReadResult(dm derpReadResult, b []byte) (n int, ep *endpoint) {
    // 	if dm.copyBuf == nil {
    // 		return 0, nil
    // 	}
    // 	var regionID int
    // 	n, regionID = dm.n, dm.regionID
    // 	ncopy := dm.copyBuf(b)
    // 	if ncopy != n {
    // 		err := fmt.Errorf("received DERP packet of length %d that's too big for WireGuard buf size %d", n, ncopy)
    // 		c.logf("magicsock: %v", err)
    // 		return 0, nil
    // 	}

    // 	ipp := netip.AddrPortFrom(derpMagicIPAddr, uint16(regionID))
    // 	if c.handleDiscoMessage(b[:n], ipp, dm.src) {
    // 		return 0, nil
    // 	}

    // 	var ok bool
    // 	c.mu.Lock()
    // 	ep, ok = c.peerMap.endpointForNodeKey(dm.src)
    // 	c.mu.Unlock()
    // 	if !ok {
    // 		// We don't know anything about this node key, nothing to
    // 		// record or process.
    // 		return 0, nil
    // 	}

    // 	ep.noteRecvActivity()
    // 	if stats := c.stats.Load(); stats != nil {
    // 		stats.UpdateRxPhysical(ep.nodeAddr, ipp, dm.n)
    // 	}
    // 	return n, ep
    // }

    // // discoLogLevel controls the verbosity of discovery log messages.
    // type discoLogLevel int

    // const (
    // 	// discoLog means that a message should be logged.
    // 	discoLog discoLogLevel = iota

    // 	// discoVerboseLog means that a message should only be logged
    // 	// in TS_DEBUG_DISCO mode.
    // 	discoVerboseLog
    // )

    // // TS_DISCO_PONG_IPV4_DELAY, if set, is a time.Duration string that is how much
    // // fake latency to add before replying to disco pings. This can be used to bias
    // // peers towards using IPv6 when both IPv4 and IPv6 are available at similar
    // // speeds.
    // var debugIPv4DiscoPingPenalty = envknob.RegisterDuration("TS_DISCO_PONG_IPV4_DELAY")

    // // sendDiscoMessage sends discovery message m to dstDisco at dst.
    // //
    // // If dst is a DERP IP:port, then dstKey must be non-zero.
    // //
    // // The dstKey should only be non-zero if the dstDisco key
    // // unambiguously maps to exactly one peer.
    // func (c *Conn) sendDiscoMessage(dst netip.AddrPort, dstKey key.NodePublic, dstDisco key.DiscoPublic, m disco.Message, logLevel discoLogLevel) (sent bool, err error) {
    // 	isDERP := dst.Addr() == derpMagicIPAddr
    // 	if _, isPong := m.(*disco.Pong); isPong && !isDERP && dst.Addr().Is4() {
    // 		time.Sleep(debugIPv4DiscoPingPenalty())
    // 	}

    // 	c.mu.Lock()
    // 	if c.closed {
    // 		c.mu.Unlock()
    // 		return false, errConnClosed
    // 	}
    // 	var nonce [disco.NonceLen]byte
    // 	if _, err := crand.Read(nonce[:]); err != nil {
    // 		panic(err) // worth dying for
    // 	}
    // 	pkt := make([]byte, 0, 512) // TODO: size it correctly? pool? if it matters.
    // 	pkt = append(pkt, disco.Magic...)
    // 	pkt = c.discoPublic.AppendTo(pkt)
    // 	di := c.discoInfoLocked(dstDisco)
    // 	c.mu.Unlock()

    // 	if isDERP {
    // 		metricSendDiscoDERP.Add(1)
    // 	} else {
    // 		metricSendDiscoUDP.Add(1)
    // 	}

    // 	box := di.sharedKey.Seal(m.AppendMarshal(nil))
    // 	pkt = append(pkt, box...)
    // 	sent, err = c.sendAddr(dst, dstKey, pkt)
    // 	if sent {
    // 		if logLevel == discoLog || (logLevel == discoVerboseLog && debugDisco()) {
    // 			node := "?"
    // 			if !dstKey.IsZero() {
    // 				node = dstKey.ShortString()
    // 			}
    // 			c.dlogf("[v1] magicsock: disco: %v->%v (%v, %v) sent %v", c.discoShort, dstDisco.ShortString(), node, derpStr(dst.String()), disco.MessageSummary(m))
    // 		}
    // 		if isDERP {
    // 			metricSentDiscoDERP.Add(1)
    // 		} else {
    // 			metricSentDiscoUDP.Add(1)
    // 		}
    // 		switch m.(type) {
    // 		case *disco.Ping:
    // 			metricSentDiscoPing.Add(1)
    // 		case *disco.Pong:
    // 			metricSentDiscoPong.Add(1)
    // 		case *disco.CallMeMaybe:
    // 			metricSentDiscoCallMeMaybe.Add(1)
    // 		}
    // 	} else if err == nil {
    // 		// Can't send. (e.g. no IPv6 locally)
    // 	} else {
    // 		if !c.networkDown() {
    // 			c.logf("magicsock: disco: failed to send %T to %v: %v", m, dst, err)
    // 		}
    // 	}
    // 	return sent, err
    // }

    // // handleDiscoMessage handles a discovery message and reports whether
    // // msg was a Tailscale inter-node discovery message.
    // //
    // // A discovery message has the form:
    // //
    // //   - magic             [6]byte
    // //   - senderDiscoPubKey [32]byte
    // //   - nonce             [24]byte
    // //   - naclbox of payload (see tailscale.com/disco package for inner payload format)
    // //
    // // For messages received over DERP, the src.Addr() will be derpMagicIP (with
    // // src.Port() being the region ID) and the derpNodeSrc will be the node key
    // // it was received from at the DERP layer. derpNodeSrc is zero when received
    // // over UDP.
    // func (c *Conn) handleDiscoMessage(msg []byte, src netip.AddrPort, derpNodeSrc key.NodePublic) (isDiscoMsg bool) {
    // 	const headerLen = len(disco.Magic) + key.DiscoPublicRawLen
    // 	if len(msg) < headerLen || string(msg[:len(disco.Magic)]) != disco.Magic {
    // 		return false
    // 	}

    // 	// If the first four parts are the prefix of disco.Magic
    // 	// (0x5453f09f) then it's definitely not a valid WireGuard
    // 	// packet (which starts with little-endian uint32 1, 2, 3, 4).
    // 	// Use naked returns for all following paths.
    // 	isDiscoMsg = true

    // 	sender := key.DiscoPublicFromRaw32(mem.B(msg[len(disco.Magic):headerLen]))

    // 	c.mu.Lock()
    // 	defer c.mu.Unlock()

    // 	if c.closed {
    // 		return
    // 	}
    // 	if debugDisco() {
    // 		c.logf("magicsock: disco: got disco-looking frame from %v", sender.ShortString())
    // 	}
    // 	if c.privateKey.IsZero() {
    // 		// Ignore disco messages when we're stopped.
    // 		// Still return true, to not pass it down to wireguard.
    // 		return
    // 	}
    // 	if c.discoPrivate.IsZero() {
    // 		if debugDisco() {
    // 			c.logf("magicsock: disco: ignoring disco-looking frame, no local key")
    // 		}
    // 		return
    // 	}

    // 	if !c.peerMap.anyEndpointForDiscoKey(sender) {
    // 		metricRecvDiscoBadPeer.Add(1)
    // 		if debugDisco() {
    // 			c.logf("magicsock: disco: ignoring disco-looking frame, don't know endpoint for %v", sender.ShortString())
    // 		}
    // 		return
    // 	}

    // 	// We're now reasonably sure we're expecting communication from
    // 	// this peer, do the heavy crypto lifting to see what they want.
    // 	//
    // 	// From here on, peerNode and de are non-nil.

    // 	di := c.discoInfoLocked(sender)

    // 	sealedBox := msg[headerLen:]
    // 	payload, ok := di.sharedKey.Open(sealedBox)
    // 	if !ok {
    // 		// This might be have been intended for a previous
    // 		// disco key.  When we restart we get a new disco key
    // 		// and old packets might've still been in flight (or
    // 		// scheduled). This is particularly the case for LANs
    // 		// or non-NATed endpoints.
    // 		// Don't log in normal case. Pass on to wireguard, in case
    // 		// it's actually a wireguard packet (super unlikely,
    // 		// but).
    // 		if debugDisco() {
    // 			c.logf("magicsock: disco: failed to open naclbox from %v (wrong rcpt?)", sender)
    // 		}
    // 		metricRecvDiscoBadKey.Add(1)
    // 		return
    // 	}

    // 	dm, err := disco.Parse(payload)
    // 	if debugDisco() {
    // 		c.logf("magicsock: disco: disco.Parse = %T, %v", dm, err)
    // 	}
    // 	if err != nil {
    // 		// Couldn't parse it, but it was inside a correctly
    // 		// signed box, so just ignore it, assuming it's from a
    // 		// newer version of Tailscale that we don't
    // 		// understand. Not even worth logging about, lest it
    // 		// be too spammy for old clients.
    // 		metricRecvDiscoBadParse.Add(1)
    // 		return
    // 	}

    // 	isDERP := src.Addr() == derpMagicIPAddr
    // 	if isDERP {
    // 		metricRecvDiscoDERP.Add(1)
    // 	} else {
    // 		metricRecvDiscoUDP.Add(1)
    // 	}

    // 	switch dm := dm.(type) {
    // 	case *disco.Ping:
    // 		metricRecvDiscoPing.Add(1)
    // 		c.handlePingLocked(dm, src, di, derpNodeSrc)
    // 	case *disco.Pong:
    // 		metricRecvDiscoPong.Add(1)
    // 		// There might be multiple nodes for the sender's DiscoKey.
    // 		// Ask each to handle it, stopping once one reports that
    // 		// the Pong's TxID was theirs.
    // 		c.peerMap.forEachEndpointWithDiscoKey(sender, func(ep *endpoint) (keepGoing bool) {
    // 			if ep.handlePongConnLocked(dm, di, src) {
    // 				return false
    // 			}
    // 			return true
    // 		})
    // 	case *disco.CallMeMaybe:
    // 		metricRecvDiscoCallMeMaybe.Add(1)
    // 		if !isDERP || derpNodeSrc.IsZero() {
    // 			// CallMeMaybe messages should only come via DERP.
    // 			c.logf("[unexpected] CallMeMaybe packets should only come via DERP")
    // 			return
    // 		}
    // 		nodeKey := derpNodeSrc
    // 		ep, ok := c.peerMap.endpointForNodeKey(nodeKey)
    // 		if !ok {
    // 			metricRecvDiscoCallMeMaybeBadNode.Add(1)
    // 			c.logf("magicsock: disco: ignoring CallMeMaybe from %v; %v is unknown", sender.ShortString(), derpNodeSrc.ShortString())
    // 			return
    // 		}
    // 		if ep.discoKey != di.discoKey {
    // 			metricRecvDiscoCallMeMaybeBadDisco.Add(1)
    // 			c.logf("[unexpected] CallMeMaybe from peer via DERP whose netmap discokey != disco source")
    // 			return
    // 		}
    // 		di.setNodeKey(nodeKey)
    // 		c.dlogf("[v1] magicsock: disco: %v<-%v (%v, %v)  got call-me-maybe, %d endpoints",
    // 			c.discoShort, ep.discoShort,
    // 			ep.publicKey.ShortString(), derpStr(src.String()),
    // 			len(dm.MyNumber))
    // 		go ep.handleCallMeMaybe(dm)
    // 	}
    // 	return
    // }

    // // unambiguousNodeKeyOfPingLocked attempts to look up an unambiguous mapping
    // // from a DiscoKey dk (which sent ping dm) to a NodeKey. ok is true
    // // if there's the NodeKey is known unambiguously.
    // //
    // // derpNodeSrc is non-zero if the disco ping arrived via DERP.
    // //
    // // c.mu must be held.
    // func (c *Conn) unambiguousNodeKeyOfPingLocked(dm *disco.Ping, dk key.DiscoPublic, derpNodeSrc key.NodePublic) (nk key.NodePublic, ok bool) {
    // 	if !derpNodeSrc.IsZero() {
    // 		if ep, ok := c.peerMap.endpointForNodeKey(derpNodeSrc); ok && ep.discoKey == dk {
    // 			return derpNodeSrc, true
    // 		}
    // 	}

    // 	// Pings after 1.16.0 contains its node source. See if it maps back.
    // 	if !dm.NodeKey.IsZero() {
    // 		if ep, ok := c.peerMap.endpointForNodeKey(dm.NodeKey); ok && ep.discoKey == dk {
    // 			return dm.NodeKey, true
    // 		}
    // 	}

    // 	// If there's exactly 1 node in our netmap with DiscoKey dk,
    // 	// then it's not ambiguous which node key dm was from.
    // 	if set := c.peerMap.nodesOfDisco[dk]; len(set) == 1 {
    // 		for nk = range set {
    // 			return nk, true
    // 		}
    // 	}

    // 	return nk, false
    // }

    // // di is the discoInfo of the source of the ping.
    // // derpNodeSrc is non-zero if the ping arrived via DERP.
    // func (c *Conn) handlePingLocked(dm *disco.Ping, src netip.AddrPort, di *discoInfo, derpNodeSrc key.NodePublic) {
    // 	likelyHeartBeat := src == di.lastPingFrom && time.Since(di.lastPingTime) < 5*time.Second
    // 	di.lastPingFrom = src
    // 	di.lastPingTime = time.Now()
    // 	isDerp := src.Addr() == derpMagicIPAddr

    // 	// If we can figure out with certainty which node key this disco
    // 	// message is for, eagerly update our IP<>node and disco<>node
    // 	// mappings to make p2p path discovery faster in simple
    // 	// cases. Without this, disco would still work, but would be
    // 	// reliant on DERP call-me-maybe to establish the disco<>node
    // 	// mapping, and on subsequent disco handlePongLocked to establish
    // 	// the IP<>disco mapping.
    // 	if nk, ok := c.unambiguousNodeKeyOfPingLocked(dm, di.discoKey, derpNodeSrc); ok {
    // 		di.setNodeKey(nk)
    // 		if !isDerp {
    // 			c.peerMap.setNodeKeyForIPPort(src, nk)
    // 		}
    // 	}

    // 	// If we got a ping over DERP, then derpNodeSrc is non-zero and we reply
    // 	// over DERP (in which case ipDst is also a DERP address).
    // 	// But if the ping was over UDP (ipDst is not a DERP address), then dstKey
    // 	// will be zero here, but that's fine: sendDiscoMessage only requires
    // 	// a dstKey if the dst ip:port is DERP.
    // 	dstKey := derpNodeSrc

    // 	// Remember this route if not present.
    // 	var numNodes int
    // 	var dup bool
    // 	if isDerp {
    // 		if ep, ok := c.peerMap.endpointForNodeKey(derpNodeSrc); ok {
    // 			if ep.addCandidateEndpoint(src, dm.TxID) {
    // 				return
    // 			}
    // 			numNodes = 1
    // 		}
    // 	} else {
    // 		c.peerMap.forEachEndpointWithDiscoKey(di.discoKey, func(ep *endpoint) (keepGoing bool) {
    // 			if ep.addCandidateEndpoint(src, dm.TxID) {
    // 				dup = true
    // 				return false
    // 			}
    // 			numNodes++
    // 			if numNodes == 1 && dstKey.IsZero() {
    // 				dstKey = ep.publicKey
    // 			}
    // 			return true
    // 		})
    // 		if dup {
    // 			return
    // 		}
    // 		if numNodes > 1 {
    // 			// Zero it out if it's ambiguous, so sendDiscoMessage logging
    // 			// isn't confusing.
    // 			dstKey = key.NodePublic{}
    // 		}
    // 	}

    // 	if numNodes == 0 {
    // 		c.logf("[unexpected] got disco ping from %v/%v for node not in peers", src, derpNodeSrc)
    // 		return
    // 	}

    // 	if !likelyHeartBeat || debugDisco() {
    // 		pingNodeSrcStr := dstKey.ShortString()
    // 		if numNodes > 1 {
    // 			pingNodeSrcStr = "[one-of-multi]"
    // 		}
    // 		c.dlogf("[v1] magicsock: disco: %v<-%v (%v, %v)  got ping tx=%x", c.discoShort, di.discoShort, pingNodeSrcStr, src, dm.TxID[:6])
    // 	}

    // 	ipDst := src
    // 	discoDest := di.discoKey
    // 	go c.sendDiscoMessage(ipDst, dstKey, discoDest, &disco.Pong{
    // 		TxID: dm.TxID,
    // 		Src:  src,
    // 	}, discoVerboseLog)
    // }

    // // enqueueCallMeMaybe schedules a send of disco.CallMeMaybe to de via derpAddr
    // // once we know that our STUN endpoint is fresh.
    // //
    // // derpAddr is de.derpAddr at the time of send. It's assumed the peer won't be
    // // flipping primary DERPs in the 0-30ms it takes to confirm our STUN endpoint.
    // // If they do, traffic will just go over DERP for a bit longer until the next
    // // discovery round.
    // func (c *Conn) enqueueCallMeMaybe(derpAddr netip.AddrPort, de *endpoint) {
    // 	c.mu.Lock()
    // 	defer c.mu.Unlock()

    // 	if !c.lastEndpointsTime.After(time.Now().Add(-endpointsFreshEnoughDuration)) {
    // 		c.dlogf("[v1] magicsock: want call-me-maybe but endpoints stale; restunning")

    // 		mak.Set(&c.onEndpointRefreshed, de, func() {
    // 			c.dlogf("[v1] magicsock: STUN done; sending call-me-maybe to %v %v", de.discoShort, de.publicKey.ShortString())
    // 			c.enqueueCallMeMaybe(derpAddr, de)
    // 		})
    // 		// TODO(bradfitz): make a new 'reSTUNQuickly' method
    // 		// that passes down a do-a-lite-netcheck flag down to
    // 		// netcheck that does 1 (or 2 max) STUN queries
    // 		// (UDP-only, not HTTPs) to find our port mapping to
    // 		// our home DERP and maybe one other. For now we do a
    // 		// "full" ReSTUN which may or may not be a full one
    // 		// (depending on age) and may do HTTPS timing queries
    // 		// (if UDP is blocked). Good enough for now.
    // 		go c.ReSTUN("refresh-for-peering")
    // 		return
    // 	}

    // 	eps := make([]netip.AddrPort, 0, len(c.lastEndpoints))
    // 	for _, ep := range c.lastEndpoints {
    // 		eps = append(eps, ep.Addr)
    // 	}
    // 	go de.c.sendDiscoMessage(derpAddr, de.publicKey, de.discoKey, &disco.CallMeMaybe{MyNumber: eps}, discoLog)
    // }

    // // discoInfoLocked returns the previous or new discoInfo for k.
    // //
    // // c.mu must be held.
    // func (c *Conn) discoInfoLocked(k key.DiscoPublic) *discoInfo {
    // 	di, ok := c.discoInfo[k]
    // 	if !ok {
    // 		di = &discoInfo{
    // 			discoKey:   k,
    // 			discoShort: k.ShortString(),
    // 			sharedKey:  c.discoPrivate.Shared(k),
    // 		}
    // 		c.discoInfo[k] = di
    // 	}
    // 	return di
    // }

    // func (c *Conn) SetNetworkUp(up bool) {
    // 	c.mu.Lock()
    // 	defer c.mu.Unlock()
    // 	if c.networkUp.Load() == up {
    // 		return
    // 	}

    // 	c.logf("magicsock: SetNetworkUp(%v)", up)
    // 	c.networkUp.Store(up)

    // 	if up {
    // 		c.startDerpHomeConnectLocked()
    // 	} else {
    // 		c.portMapper.NoteNetworkDown()
    // 		c.closeAllDerpLocked("network-down")
    // 	}
    // }

    // // SetPreferredPort sets the connection's preferred local port.
    // func (c *Conn) SetPreferredPort(port uint16) {
    // 	if uint16(c.port.Load()) == port {
    // 		return
    // 	}
    // 	c.port.Store(uint32(port))

    // 	if err := c.rebind(dropCurrentPort); err != nil {
    // 		c.logf("%w", err)
    // 		return
    // 	}
    // 	c.resetEndpointStates()
    // }

    // // SetPrivateKey sets the connection's private key.
    // //
    // // This is only used to be able prove our identity when connecting to
    // // DERP servers.
    // //
    // // If the private key changes, any DERP connections are torn down &
    // // recreated when needed.
    // func (c *Conn) SetPrivateKey(privateKey key.NodePrivate) error {
    // 	c.mu.Lock()
    // 	defer c.mu.Unlock()

    // 	oldKey, newKey := c.privateKey, privateKey
    // 	if newKey.Equal(oldKey) {
    // 		return nil
    // 	}
    // 	c.privateKey = newKey
    // 	c.havePrivateKey.Store(!newKey.IsZero())

    // 	if newKey.IsZero() {
    // 		c.publicKeyAtomic.Store(key.NodePublic{})
    // 	} else {
    // 		c.publicKeyAtomic.Store(newKey.Public())
    // 	}

    // 	if oldKey.IsZero() {
    // 		c.everHadKey = true
    // 		c.logf("magicsock: SetPrivateKey called (init)")
    // 		go c.ReSTUN("set-private-key")
    // 	} else if newKey.IsZero() {
    // 		c.logf("magicsock: SetPrivateKey called (zeroed)")
    // 		c.closeAllDerpLocked("zero-private-key")
    // 		c.stopPeriodicReSTUNTimerLocked()
    // 		c.onEndpointRefreshed = nil
    // 	} else {
    // 		c.logf("magicsock: SetPrivateKey called (changed)")
    // 		c.closeAllDerpLocked("new-private-key")
    // 	}

    // 	// Key changed. Close existing DERP connections and reconnect to home.
    // 	if c.myDerp != 0 && !newKey.IsZero() {
    // 		c.logf("magicsock: private key changed, reconnecting to home derp-%d", c.myDerp)
    // 		c.startDerpHomeConnectLocked()
    // 	}

    // 	if newKey.IsZero() {
    // 		c.peerMap.forEachEndpoint(func(ep *endpoint) {
    // 			ep.stopAndReset()
    // 		})
    // 	}

    // 	return nil
    // }

    // // UpdatePeers is called when the set of WireGuard peers changes. It
    // // then removes any state for old peers.
    // //
    // // The caller passes ownership of newPeers map to UpdatePeers.
    // func (c *Conn) UpdatePeers(newPeers map[key.NodePublic]struct{}) {
    // 	c.mu.Lock()
    // 	defer c.mu.Unlock()

    // 	oldPeers := c.peerSet
    // 	c.peerSet = newPeers

    // 	// Clean up any key.NodePublic-keyed maps for peers that no longer
    // 	// exist.
    // 	for peer := range oldPeers {
    // 		if _, ok := newPeers[peer]; !ok {
    // 			delete(c.derpRoute, peer)
    // 			delete(c.peerLastDerp, peer)
    // 		}
    // 	}

    // 	if len(oldPeers) == 0 && len(newPeers) > 0 {
    // 		go c.ReSTUN("non-zero-peers")
    // 	}
    // }

    // // SetDERPMap controls which (if any) DERP servers are used.
    // // A nil value means to disable DERP; it's disabled by default.
    // func (c *Conn) SetDERPMap(dm *tailcfg.DERPMap) {
    // 	c.mu.Lock()
    // 	defer c.mu.Unlock()

    // 	if reflect.DeepEqual(dm, c.derpMap) {
    // 		return
    // 	}

    // 	c.derpMapAtomic.Store(dm)
    // 	old := c.derpMap
    // 	c.derpMap = dm
    // 	if dm == nil {
    // 		c.closeAllDerpLocked("derp-disabled")
    // 		return
    // 	}

    // 	// Reconnect any DERP region that changed definitions.
    // 	if old != nil {
    // 		changes := false
    // 		for rid, oldDef := range old.Regions {
    // 			if reflect.DeepEqual(oldDef, dm.Regions[rid]) {
    // 				continue
    // 			}
    // 			changes = true
    // 			if rid == c.myDerp {
    // 				c.myDerp = 0
    // 			}
    // 			c.closeDerpLocked(rid, "derp-region-redefined")
    // 		}
    // 		if changes {
    // 			c.logActiveDerpLocked()
    // 		}
    // 	}

    // 	go c.ReSTUN("derp-map-update")
    // }

    // func nodesEqual(x, y []*tailcfg.Node) bool {
    // 	if len(x) != len(y) {
    // 		return false
    // 	}
    // 	for i := range x {
    // 		if !x[i].Equal(y[i]) {
    // 			return false
    // 		}
    // 	}
    // 	return true
    // }

    // // SetNetworkMap is called when the control client gets a new network
    // // map from the control server. It must always be non-nil.
    // //
    // // It should not use the DERPMap field of NetworkMap; that's
    // // conditionally sent to SetDERPMap instead.
    // func (c *Conn) SetNetworkMap(nm *netmap.NetworkMap) {
    // 	c.mu.Lock()
    // 	defer c.mu.Unlock()

    // 	if c.closed {
    // 		return
    // 	}

    // 	priorNetmap := c.netMap
    // 	var priorDebug *tailcfg.Debug
    // 	if priorNetmap != nil {
    // 		priorDebug = priorNetmap.Debug
    // 	}
    // 	debugChanged := !reflect.DeepEqual(priorDebug, nm.Debug)
    // 	metricNumPeers.Set(int64(len(nm.Peers)))

    // 	// Update c.netMap regardless, before the following early return.
    // 	c.netMap = nm

    // 	if priorNetmap != nil && nodesEqual(priorNetmap.Peers, nm.Peers) && !debugChanged {
    // 		// The rest of this function is all adjusting state for peers that have
    // 		// changed. But if the set of peers is equal and the debug flags (for
    // 		// silent disco) haven't changed, no need to do anything else.
    // 		return
    // 	}

    // 	c.logf("[v1] magicsock: got updated network map; %d peers", len(nm.Peers))
    // 	heartbeatDisabled := debugEnableSilentDisco() || (c.netMap != nil && c.netMap.Debug != nil && c.netMap.Debug.EnableSilentDisco)

    // 	// Try a pass of just upserting nodes and creating missing
    // 	// endpoints. If the set of nodes is the same, this is an
    // 	// efficient alloc-free update. If the set of nodes is different,
    // 	// we'll fall through to the next pass, which allocates but can
    // 	// handle full set updates.
    // 	for _, n := range nm.Peers {
    // 		if ep, ok := c.peerMap.endpointForNodeKey(n.Key); ok {
    // 			if n.DiscoKey.IsZero() {
    // 				// Discokey transitioned from non-zero to zero? Ignore. Server's confused.
    // 				c.peerMap.deleteEndpoint(ep)
    // 				continue
    // 			}
    // 			oldDiscoKey := ep.discoKey
    // 			ep.updateFromNode(n, heartbeatDisabled)
    // 			c.peerMap.upsertEndpoint(ep, oldDiscoKey) // maybe update discokey mappings in peerMap
    // 			continue
    // 		}
    // 		if n.DiscoKey.IsZero() {
    // 			// Ancient pre-0.100 node. Ignore, so we can assume elsewhere in magicsock
    // 			// that all nodes have a DiscoKey.
    // 			continue
    // 		}

    // 		ep := &endpoint{
    // 			c:                 c,
    // 			publicKey:         n.Key,
    // 			publicKeyHex:      n.Key.UntypedHexString(),
    // 			sentPing:          map[stun.TxID]sentPing{},
    // 			endpointState:     map[netip.AddrPort]*endpointState{},
    // 			heartbeatDisabled: heartbeatDisabled,
    // 		}
    // 		if len(n.Addresses) > 0 {
    // 			ep.nodeAddr = n.Addresses[0].Addr()
    // 		}
    // 		ep.discoKey = n.DiscoKey
    // 		ep.discoShort = n.DiscoKey.ShortString()
    // 		ep.initFakeUDPAddr()
    // 		if debugDisco() { // rather than making a new knob
    // 			c.logf("magicsock: created endpoint key=%s: disco=%s; %v", n.Key.ShortString(), n.DiscoKey.ShortString(), logger.ArgWriter(func(w *bufio.Writer) {
    // 				const derpPrefix = "127.3.3.40:"
    // 				if strings.HasPrefix(n.DERP, derpPrefix) {
    // 					ipp, _ := netip.ParseAddrPort(n.DERP)
    // 					regionID := int(ipp.Port())
    // 					code := c.derpRegionCodeLocked(regionID)
    // 					if code != "" {
    // 						code = "(" + code + ")"
    // 					}
    // 					fmt.Fprintf(w, "derp=%v%s ", regionID, code)
    // 				}

    // 				for _, a := range n.AllowedIPs {
    // 					if a.IsSingleIP() {
    // 						fmt.Fprintf(w, "aip=%v ", a.Addr())
    // 					} else {
    // 						fmt.Fprintf(w, "aip=%v ", a)
    // 					}
    // 				}
    // 				for _, ep := range n.Endpoints {
    // 					fmt.Fprintf(w, "ep=%v ", ep)
    // 				}
    // 			}))
    // 		}
    // 		ep.updateFromNode(n, heartbeatDisabled)
    // 		c.peerMap.upsertEndpoint(ep, key.DiscoPublic{})
    // 	}

    // 	// If the set of nodes changed since the last SetNetworkMap, the
    // 	// upsert loop just above made c.peerMap contain the union of the
    // 	// old and new peers - which will be larger than the set from the
    // 	// current netmap. If that happens, go through the allocful
    // 	// deletion path to clean up moribund nodes.
    // 	if c.peerMap.nodeCount() != len(nm.Peers) {
    // 		keep := make(map[key.NodePublic]bool, len(nm.Peers))
    // 		for _, n := range nm.Peers {
    // 			keep[n.Key] = true
    // 		}
    // 		c.peerMap.forEachEndpoint(func(ep *endpoint) {
    // 			if !keep[ep.publicKey] {
    // 				c.peerMap.deleteEndpoint(ep)
    // 			}
    // 		})
    // 	}

    // 	// discokeys might have changed in the above. Discard unused info.
    // 	for dk := range c.discoInfo {
    // 		if !c.peerMap.anyEndpointForDiscoKey(dk) {
    // 			delete(c.discoInfo, dk)
    // 		}
    // 	}
    // }

    // func (c *Conn) wantDerpLocked() bool { return c.derpMap != nil }

    // // c.mu must be held.
    // func (c *Conn) closeAllDerpLocked(why string) {
    // 	if len(c.activeDerp) == 0 {
    // 		return // without the useless log statement
    // 	}
    // 	for i := range c.activeDerp {
    // 		c.closeDerpLocked(i, why)
    // 	}
    // 	c.logActiveDerpLocked()
    // }

    // // maybeCloseDERPsOnRebind, in response to a rebind, closes all
    // // DERP connections that don't have a local address in okayLocalIPs
    // // and pings all those that do.
    // func (c *Conn) maybeCloseDERPsOnRebind(okayLocalIPs []netip.Prefix) {
    // 	c.mu.Lock()
    // 	defer c.mu.Unlock()
    // 	for regionID, ad := range c.activeDerp {
    // 		la, err := ad.c.LocalAddr()
    // 		if err != nil {
    // 			c.closeOrReconnectDERPLocked(regionID, "rebind-no-localaddr")
    // 			continue
    // 		}
    // 		if !tsaddr.PrefixesContainsIP(okayLocalIPs, la.Addr()) {
    // 			c.closeOrReconnectDERPLocked(regionID, "rebind-default-route-change")
    // 			continue
    // 		}
    // 		regionID := regionID
    // 		dc := ad.c
    // 		go func() {
    // 			ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
    // 			defer cancel()
    // 			if err := dc.Ping(ctx); err != nil {
    // 				c.mu.Lock()
    // 				defer c.mu.Unlock()
    // 				c.closeOrReconnectDERPLocked(regionID, "rebind-ping-fail")
    // 				return
    // 			}
    // 			c.logf("post-rebind ping of DERP region %d okay", regionID)
    // 		}()
    // 	}
    // 	c.logActiveDerpLocked()
    // }

    // // closeOrReconnectDERPLocked closes the DERP connection to the
    // // provided regionID and starts reconnecting it if it's our current
    // // home DERP.
    // //
    // // why is a reason for logging.
    // //
    // // c.mu must be held.
    // func (c *Conn) closeOrReconnectDERPLocked(regionID int, why string) {
    // 	c.closeDerpLocked(regionID, why)
    // 	if !c.privateKey.IsZero() && c.myDerp == regionID {
    // 		c.startDerpHomeConnectLocked()
    // 	}
    // }

    // // c.mu must be held.
    // // It is the responsibility of the caller to call logActiveDerpLocked after any set of closes.
    // func (c *Conn) closeDerpLocked(regionID int, why string) {
    // 	if ad, ok := c.activeDerp[regionID]; ok {
    // 		c.logf("magicsock: closing connection to derp-%v (%v), age %v", regionID, why, time.Since(ad.createTime).Round(time.Second))
    // 		go ad.c.Close()
    // 		ad.cancel()
    // 		delete(c.activeDerp, regionID)
    // 		metricNumDERPConns.Set(int64(len(c.activeDerp)))
    // 	}
    // }

    // // c.mu must be held.
    // func (c *Conn) logActiveDerpLocked() {
    // 	now := time.Now()
    // 	c.logf("magicsock: %v active derp conns%s", len(c.activeDerp), logger.ArgWriter(func(buf *bufio.Writer) {
    // 		if len(c.activeDerp) == 0 {
    // 			return
    // 		}
    // 		buf.WriteString(":")
    // 		c.foreachActiveDerpSortedLocked(func(node int, ad activeDerp) {
    // 			fmt.Fprintf(buf, " derp-%d=cr%v,wr%v", node, simpleDur(now.Sub(ad.createTime)), simpleDur(now.Sub(*ad.lastWrite)))
    // 		})
    // 	}))
    // }

    // func (c *Conn) logEndpointChange(endpoints []tailcfg.Endpoint) {
    // 	c.logf("magicsock: endpoints changed: %s", logger.ArgWriter(func(buf *bufio.Writer) {
    // 		for i, ep := range endpoints {
    // 			if i > 0 {
    // 				buf.WriteString(", ")
    // 			}
    // 			fmt.Fprintf(buf, "%s (%s)", ep.Addr, ep.Type)
    // 		}
    // 	}))
    // }

    // // c.mu must be held.
    // func (c *Conn) foreachActiveDerpSortedLocked(fn func(regionID int, ad activeDerp)) {
    // 	if len(c.activeDerp) < 2 {
    // 		for id, ad := range c.activeDerp {
    // 			fn(id, ad)
    // 		}
    // 		return
    // 	}
    // 	ids := make([]int, 0, len(c.activeDerp))
    // 	for id := range c.activeDerp {
    // 		ids = append(ids, id)
    // 	}
    // 	sort.Ints(ids)
    // 	for _, id := range ids {
    // 		fn(id, c.activeDerp[id])
    // 	}
    // }

    // func (c *Conn) cleanStaleDerp() {
    // 	c.mu.Lock()
    // 	defer c.mu.Unlock()
    // 	if c.closed {
    // 		return
    // 	}
    // 	c.derpCleanupTimerArmed = false

    // 	tooOld := time.Now().Add(-derpInactiveCleanupTime)
    // 	dirty := false
    // 	someNonHomeOpen := false
    // 	for i, ad := range c.activeDerp {
    // 		if i == c.myDerp {
    // 			continue
    // 		}
    // 		if ad.lastWrite.Before(tooOld) {
    // 			c.closeDerpLocked(i, "idle")
    // 			dirty = true
    // 		} else {
    // 			someNonHomeOpen = true
    // 		}
    // 	}
    // 	if dirty {
    // 		c.logActiveDerpLocked()
    // 	}
    // 	if someNonHomeOpen {
    // 		c.scheduleCleanStaleDerpLocked()
    // 	}
    // }

    // func (c *Conn) scheduleCleanStaleDerpLocked() {
    // 	if c.derpCleanupTimerArmed {
    // 		// Already going to fire soon. Let the existing one
    // 		// fire lest it get infinitely delayed by repeated
    // 		// calls to scheduleCleanStaleDerpLocked.
    // 		return
    // 	}
    // 	c.derpCleanupTimerArmed = true
    // 	if c.derpCleanupTimer != nil {
    // 		c.derpCleanupTimer.Reset(derpCleanStaleInterval)
    // 	} else {
    // 		c.derpCleanupTimer = time.AfterFunc(derpCleanStaleInterval, c.cleanStaleDerp)
    // 	}
    // }

    // // DERPs reports the number of active DERP connections.
    // func (c *Conn) DERPs() int {
    // 	c.mu.Lock()
    // 	defer c.mu.Unlock()

    // 	return len(c.activeDerp)
    // }

    // // Bind returns the wireguard-go conn.Bind for c.
    // func (c *Conn) Bind() conn.Bind {
    // 	return c.bind
    // }
}

/// A route entry for a public key, saying that a certain peer should be available at DERP
/// node derpID, as long as the current connection for that derpID is dc. (but dc should not be
/// used to write directly; it's owned by the read/write loops)
#[derive(Debug)]
struct DerpRoute {
    derp_id: usize,
    dc: derp::http::Client, // don't use directly; see comment above
}

/// The info and state for the DiscoKey in the Conn.discoInfo map key.
///
/// Note that a DiscoKey does not necessarily map to exactly one
/// node. In the case of shared nodes and users switching accounts, two
/// nodes in the NetMap may legitimately have the same DiscoKey.  As
/// such, no fields in here should be considered node-specific.
#[derive(Debug)]
struct DiscoInfo {
    /// The same as the Conn.discoInfo map key, just so you can pass around a `DiscoInfo` alone.
    /// Not modified once initialized.
    disco_key: key::DiscoPublic,

    /// The precomputed key for communication with the peer that has the `DiscoKey` used to
    /// look up this `DiscoInfo` in Conn.discoInfo.
    /// Not modified once initialized.
    shared_key: key::DiscoShared,

    // Mutable fields follow, owned by Conn.mu:
    /// Tthe src of a ping for `DiscoKey`.
    last_ping_from: SocketAddr,

    /// The last time of a ping for discoKey.
    last_ping_time: Instant,

    /// The last NodeKey seen using `DiscoKey`.
    /// It's only updated if the NodeKey is unambiguous.
    last_node_key: key::NodePublic,

    /// The time a NodeKey was last seen using this `DiscoKey`. It's only updated if the
    /// NodeKey is unambiguous.
    last_node_key_time: Instant,
}

// // setNodeKey sets the most recent mapping from di.discoKey to the
// // NodeKey nk.
// func (di *discoInfo) setNodeKey(nk key.NodePublic) {
// 	di.lastNodeKey = nk
// 	di.lastNodeKeyTime = time.Now()
// }
