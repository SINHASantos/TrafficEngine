use e2d2::operators::*;
use e2d2::scheduler::*;
use e2d2::allocators::CacheAligned;
use e2d2::native::zcsi::rte_kni_handle_request;
use e2d2::headers::{NullHeader, IpHeader, MacHeader, TcpHeader};
use e2d2::interface::*;
use e2d2::utils::{finalize_checksum, ipv4_extract_flow};
use e2d2::queues::{new_mpsc_queue_pair, MpscProducer};
use e2d2::headers::EndOffset;
use e2d2::common::EmptyMetadata;
use e2d2::utils;

use std::sync::Arc;
use std::cmp::min;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::process::Command;
use std::sync::mpsc::{channel, Sender, TryRecvError};
use std::ops::BitAnd;

use eui48::MacAddress;
use ipnet::Ipv4Net;
use uuid::Uuid;

use rand;
use cmanager::*;
use timer_wheel::TimerWheel;
use Configuration;
use {PipelineId, MessageFrom, MessageTo, TaskType};
use std::sync::mpsc::SyncSender;

const MIN_FRAME_SIZE: usize = 60; // without fcs

pub struct KniHandleRequest {
    pub kni_port: Arc<PmdPort>,
}

impl Executable for KniHandleRequest {
    fn execute(&mut self) -> u32 {
        unsafe {
            rte_kni_handle_request(self.kni_port.get_kni());
        }
        1
    }
}

pub fn is_kni_core(pci: &CacheAligned<PortQueue>) -> bool {
    pci.rxq() == 0
}

pub fn setup_kni(kni_name: &str, ip_address: &str, mac_address: &str, kni_netns: &str) {
    debug!("setup_kni");
    //# ip link set dev vEth1 address XX:XX:XX:XX:XX:XX
    let output = Command::new("ip")
        .args(&["link", "set", "dev", kni_name, "address", mac_address])
        .output()
        .expect("failed to assign MAC address to kni i/f");
    let reply = output.stderr;

    debug!(
        "assigning MAC addr {} to {}: {}, {}",
        mac_address,
        kni_name,
        output.status,
        String::from_utf8_lossy(&reply)
    );

    //# ip netns add nskni
    let output = Command::new("ip")
        .args(&["netns", "add", kni_netns])
        .output()
        .expect("failed to create namespace for kni i/f");
    let reply = output.stderr;

    debug!(
        "creating network namespace {}: {}, {}",
        kni_netns,
        output.status,
        String::from_utf8_lossy(&reply)
    );

    // ip link set dev vEth1 netns nskni
    let output = Command::new("ip")
        .args(&["link", "set", "dev", kni_name, "netns", kni_netns])
        .output()
        .expect("failed to move kni i/f to namespace");
    let reply = output.stderr;

    debug!(
        "moving kni i/f {} to namesapce {}: {}, {}",
        kni_name,
        kni_netns,
        output.status,
        String::from_utf8_lossy(&reply)
    );

    // e.g. ip netns exec nskni ip addr add w.x.y.z/24 dev vEth1
    let output = Command::new("ip")
        .args(&["netns", "exec", kni_netns, "ip", "addr", "add", ip_address, "dev", kni_name])
        .output()
        .expect("failed to assign IP address to kni i/f");
    let reply = output.stderr;
    debug!(
        "assigning IP addr {} to {}: {}, {}",
        ip_address,
        kni_name,
        output.status,
        String::from_utf8_lossy(&reply)
    );
    // e.g. ip netns exec nskni ip link set dev vEth1 up
    let output1 = Command::new("ip")
        .args(&["netns", "exec", kni_netns, "ip", "link", "set", "dev", kni_name, "up"])
        .output()
        .expect("failed to set kni i/f up");
    let reply1 = output1.stderr;
    debug!(
        "ip netns exec {} ip link set dev {} up: {}, {}",
        kni_netns,
        kni_name,
        output1.status,
        String::from_utf8_lossy(&reply1)
    );
    // e.g. ip netns exec nskni ip addr show dev vEth1
    let output2 = Command::new("ip")
        .args(&["netns", "exec", kni_netns, "ip", "addr", "show", "dev", kni_name])
        .output()
        .expect("failed to show IP address of kni i/f");
    let reply2 = output2.stdout;
    info!("show IP addr: {}\n {}", output.status, String::from_utf8_lossy(&reply2));
}

#[derive(Clone)]
pub struct PacketInjector {
    mac: MacHeader,
    ip: IpHeader,
    tcp: TcpHeader,
    producer: MpscProducer,
    no_batches: u32,
    sent_batches: u32,
    tx: Sender<MessageFrom>,
}

pub const PRIVATE_ETYPE_TAG: u16 = 0x08FF;

impl PacketInjector {
    // by setting no_batches=0 batch creation is unlimited
    pub fn new(producer: MpscProducer, hd_src_data: &L234Data, no_batches: u32, tx: Sender<MessageFrom>) -> PacketInjector {
        let mut mac = MacHeader::new();
        // TODO revisit which fields we really need to initialize here!
        mac.src = hd_src_data.mac.clone();
        mac.set_etype(PRIVATE_ETYPE_TAG); // mark this through an unused ethertype as an internal frame, will be re-written later in the pipeline
        let mut ip = IpHeader::new();
        ip.set_src(u32::from(hd_src_data.ip));
        ip.set_ttl(128);
        ip.set_version(4);
        ip.set_protocol(6); //tcp
        ip.set_ihl(5);
        ip.set_length(40);
        ip.set_flags(0x2); // DF=1, MF=0 flag: don't fragment
        let mut tcp = TcpHeader::new();
        tcp.set_syn_flag();
        tcp.set_src_port(hd_src_data.port);
        tcp.set_data_offset(5);
        PacketInjector {
            mac,
            ip,
            tcp,
            producer,
            no_batches,
            tx,
            sent_batches: 0,
        }
    }

    #[inline]
    fn initialize_packet(&self, pkt: Packet<NullHeader, EmptyMetadata>) -> Packet<TcpHeader, EmptyMetadata> {
        pkt.push_header(&self.mac)
            .unwrap()
            .push_header(&self.ip)
            .unwrap()
            .push_header(&self.tcp)
            .unwrap()
    }

    #[inline]
    pub fn create_packet(&mut self) -> Packet<TcpHeader, EmptyMetadata> {
        let p = self.initialize_packet(new_packet().unwrap());
        self.tcp.incr_src_port();
        p
    }
}

impl Executable for PacketInjector {
    fn execute(&mut self) -> u32 {
        let mut count = 0;
        if self.no_batches == 0 || self.sent_batches < self.no_batches {
            for _ in 0..16 {
                let p = self.create_packet();
                self.producer.enqueue_one(p);
                count += 1;
            }
            self.sent_batches += 1;
        }
        count
    }
}

pub fn setup_generator<F1, F2>(
    core: i32,
    pci: &CacheAligned<PortQueue>,
    kni: &CacheAligned<PortQueue>,
    sched: &mut StandaloneScheduler,
    configuration: &Configuration,
    f_select_server: Arc<F1>,
    f_process_payload_c_s: Arc<F2>,
    tx: Sender<MessageFrom>,
) where
    F1: Fn(&mut Connection) + Sized + Send + Sync + 'static,
    F2: Fn(&mut Connection, &mut [u8], usize) + Sized + Send + Sync + 'static,
{
    let me = L234Data {
        mac: MacAddress::parse_str(&configuration.engine.mac).unwrap(),
        ip: u32::from(configuration.engine.ipnet.parse::<Ipv4Net>().unwrap().addr()),
        port: configuration.engine.port,
        server_id: "TrafficEngine".to_string(),
    };

    let pipeline_id = PipelineId {
        core: core as u16,
        port_id: pci.port.port_id() as u16,
        rxq: pci.rxq(),
    };
    debug!("enter setup_generator {}", pipeline_id);

    let mut sm: ConnectionManager = ConnectionManager::new(pipeline_id.clone(), pci.clone(), me.clone(), configuration.clone());
    let mut wheel = TimerWheel::new(128, 16, 128);

    // setting up a a reverse message channel between this pipeline and the main program thread
    debug!("setting up reverse channel from pipeline {}", pipeline_id);
    let (remote_tx, rx) = channel::<MessageTo>();
    // we send the transmitter to the remote receiver of our messages
    tx.send(MessageFrom::Channel(pipeline_id.clone(), remote_tx)).unwrap();

    // forwarding frames coming from KNI to PCI, if we are the kni core
    if is_kni_core(pci) {
        let forward2pci = ReceiveBatch::new(kni.clone())
            .parse::<MacHeader>()
            //.transform(box move |p| {
            //    let ethhead = p.get_mut_header();
            //    //debug!("sending KNI frame to PCI: Eth header = { }", &ethhead);
            //})
            .send(pci.clone());
        let uuid = Uuid::new_v4();
        let name = String::from("Kni2Pci");
        sched.add_runnable(Runnable::from_task(uuid, name, forward2pci).ready());
    }
    let thread_id_0 = format!("<c{}, rx{}>: ", core, pci.rxq());
    let thread_id_1 = format!("<c{}, rx{}>: ", core, pci.rxq());
    let thread_id_2 = format!("<c{}, rx{}>: ", core, pci.rxq());

    let me_clone = me.clone();
    // only accept traffic from PCI with matching L2 address
    let l2filter_from_pci = ReceiveBatch::new(pci.clone()).parse::<MacHeader>().filter(box move |p| {
        let header = p.get_header();
        if header.dst == me_clone.mac {
            //debug!("{} from pci: found mac: {} ", thread_id_0, &header);
            true
        } else if header.dst.is_multicast() || header.dst.is_broadcast() {
            //debug!("{} from pci: multicast mac: {} ", thread_id_0, &header);
            true
        } else {
            debug!("{} from pci: discarding because mac unknown: {} ", thread_id_0, &header);
            false
        }
    });

    let tcp_min_port = sm.tcp_port_base();
    let pd_clone = me.clone();
    let uuid_l2groupby = Uuid::new_v4();
    let uuid_l2groupby_clone = uuid_l2groupby.clone();
    // group the traffic into TCP traffic addressed to Proxy (group 1),
    // and send all other traffic to KNI (group 0)
    let mut l2groups = l2filter_from_pci.group_by(
        2,
        box move |p| {
            let payload = p.get_payload();
            let ipflow = ipv4_extract_flow(payload);
            if ipflow.is_none() {
                debug!("{} not ip_flow", thread_id_1);
                0
            } else {
                let ipflow = ipflow.unwrap();
                if ipflow.dst_ip == pd_clone.ip && ipflow.proto == 6 {
                    if ipflow.dst_port == pd_clone.port || ipflow.dst_port >= tcp_min_port {
                        //debug!("{} proxy tcp flow: {}", thread_id_1, ipflow);
                        1
                    } else {
                        //debug!("{} no proxy tcp flow: {}", thread_id_1, ipflow);
                        0
                    }
                } else {
                    //debug!("{} ignored by proxy: not a tcp flow or not addressed to proxy", thread_id_1);
                    0
                }
            }
        },
        sched,
        uuid_l2groupby_clone,
    );
    // we create SYN packets and merge them with the upstream from the pci i/f
    let (producer, consumer) = new_mpsc_queue_pair();
    let creator = PacketInjector::new(producer, &me, 512, tx.clone());
    let mut syn_counter = 0u64;
    let uuid = Uuid::new_v4();
    let name = String::from("PacketInjector");
    sched.add_runnable(Runnable::from_task(uuid, name, creator).unready());
    tx.send(MessageFrom::Task(pipeline_id.clone(), uuid, TaskType::TcpGenerator))
        .unwrap();

    let tx_clone = tx.clone();
    let pipeline_id_clone = pipeline_id.clone();

    let l2_input_stream = merge(vec![consumer.compose(), l2groups.get_group(1).unwrap().compose()]);
    // group 0 -> dump packets
    // group 1 -> send to PCI
    // group 2 -> send to KNI
    let uuid_l4groupby = Uuid::new_v4();
    let uuid_l4groupby_clone = uuid_l4groupby.clone();
    // process TCP traffic addressed to Proxy
    let mut l4groups = l2_input_stream
        .parse::<MacHeader>()
        .parse::<IpHeader>()
        .parse::<TcpHeader>()
        .group_by(
            3,
            box move |p| {
                // this is the major closure for TCP processing
                struct HeaderState<'a> {
                    mac: &'a mut MacHeader,
                    ip: &'a mut IpHeader,
                    tcp: &'a mut TcpHeader,
                    //flow: Flow,
                }

                impl<'a> HeaderState<'a> {
                    fn set_server_socket(&mut self, ip: u32, port: u16) {
                        self.ip.set_dst(ip);
                        self.tcp.set_dst_port(port);
                    }
                }

                fn do_ttl(h: &mut HeaderState) {
                    let ttl = h.ip.ttl();
                    if ttl >= 1 {
                        h.ip.set_ttl(ttl - 1);
                    }
                    h.ip.update_checksum();
                }

                fn make_reply_packet(h: &mut HeaderState) {
                    let smac = h.mac.src;
                    let dmac = h.mac.dst;
                    let sip = h.ip.src();
                    let dip = h.ip.dst();
                    let sport = h.tcp.src_port();
                    let dport = h.tcp.dst_port();
                    h.mac.set_smac(&dmac);
                    h.mac.set_dmac(&smac);
                    h.ip.set_dst(sip);
                    h.ip.set_src(dip);
                    h.tcp.set_src_port(dport);
                    h.tcp.set_dst_port(sport);
                    h.tcp.set_ack_flag();
                    let ack_num = h.tcp.seq_num().wrapping_add(1);
                    h.tcp.set_ack_num(ack_num);
                }

                fn set_header(c: &mut Connection, h: &mut HeaderState, me: &L234Data) {
                    if c.server.is_none() {
                        error!("no server set: {}", c);
                    }
                    h.mac.set_dmac(&c.server.as_ref().unwrap().mac);
                    h.mac.set_smac(&me.mac);
                    let l2l3 = &c.server.as_ref().unwrap();
                    h.set_server_socket(l2l3.ip, l2l3.port);
                    h.ip.set_src(me.ip);
                    h.tcp.set_src_port(c.p_port());
                    h.ip.update_checksum();
                }

                fn server_to_client<M: Sized + Send>(
                    // we will need p once s->c payload inspection is required
                    _p: &mut Packet<TcpHeader, M>,
                    c: &mut Connection,
                    h: &mut HeaderState,
                    pd: &L234Data,
                ) {
                    // this is the s->c part of the stable two-way connection state
                    // translate packets and forward to client
                    h.mac.set_dmac(&c.client_mac.src);
                    h.mac.set_smac(&pd.mac);
                    let ip_server = h.ip.src();
                    h.ip.set_dst(u32::from(*c.get_client_sock().ip()));
                    h.ip.set_src(pd.ip);
                    let server_src_port = h.tcp.src_port();
                    h.tcp.set_src_port(pd.port);
                    h.tcp.set_dst_port(c.get_client_sock().port());
                    h.tcp.update_checksum_incremental(server_src_port, pd.port);
                    h.tcp.update_checksum_incremental(c.p_port(), c.get_client_sock().port());
                    h.tcp.update_checksum_incremental(
                        !finalize_checksum(ip_server),
                        !finalize_checksum(u32::from(*c.get_client_sock().ip())),
                    );
                    // adapt seqn and ackn from server packet
                    let oldseqn = h.tcp.seq_num();
                    let newseqn = oldseqn.wrapping_add(c.c_seqn);
                    let oldackn = h.tcp.ack_num();
                    let newackn = oldackn.wrapping_sub(c.c2s_inserted_bytes as u32);
                    if c.c2s_inserted_bytes != 0 {
                        h.tcp.set_ack_num(newackn);
                        h.tcp
                            .update_checksum_incremental(!finalize_checksum(oldackn), !finalize_checksum(newackn));
                    }
                    h.tcp.set_seq_num(newseqn);
                    h.tcp
                        .update_checksum_incremental(!finalize_checksum(oldseqn), !finalize_checksum(newseqn));
                    //debug!("translated s->c: {}", p);
                }

                #[inline]
                pub fn tcpip_payload_size<M: Sized + Send>(p: &Packet<TcpHeader, M>) -> u16 {
                    let iph = p.get_pre_header().unwrap();
                    // payload size = ip total length - ip header length -tcp header length
                    iph.length() - (iph.ihl() as u16) * 4u16 - (p.get_header().data_offset() as u16) * 4u16
                }

                fn server_synack_received<M: Sized + Send>(
                    p: &mut Packet<TcpHeader, M>,
                    c: &mut Connection,
                    h: &mut HeaderState,
                    seqn_inc: u32,
                ) {
                    make_reply_packet(h);
                    h.tcp.unset_syn_flag();
                    c.c_seqn = c.c_seqn.wrapping_add(seqn_inc);
                    h.tcp.set_seq_num(c.c_seqn);
                    update_tcp_checksum(p, h.ip.payload_size(0), h.ip.src(), h.ip.dst());
                }

                fn generate_syn<M: Sized + Send, F>(
                    p: &mut Packet<TcpHeader, M>,
                    c: &mut Connection,
                    h: &mut HeaderState,
                    me: &L234Data,
                    f_select_server: &Arc<F>,
                    tx: &Sender<MessageFrom>,
                    pipeline_id: &PipelineId,
                    syn_counter: &mut u64,
                ) where
                    F: Fn(&mut Connection),
                {
                    h.mac.set_etype(0x0800); // overwrite private ethertype tag
                    f_select_server(c);
                    // save server_id to connection record
                    c.con_rec.server_id = if c.server.is_some() {
                        c.server.as_ref().unwrap().server_id.clone()
                    } else {
                        String::from("<unselected>")
                    };
                    set_header(c, h, me);
                    //generate seq number:
                    c.c_seqn = rand::random::<u32>();
                    h.tcp.set_seq_num(c.c_seqn);
                    h.tcp.set_syn_flag();
                    h.tcp.set_window_size(5840); // 4* MSS(1460)
                    h.tcp.set_ack_num(0u32);
                    h.tcp.unset_ack_flag();
                    h.tcp.unset_psh_flag();
                    update_tcp_checksum(p, h.ip.payload_size(0), h.ip.src(), h.ip.dst());
                    unsafe {
                        *syn_counter += 1;
                        if syn_counter.bitand(1023u64) == 0 {
                            tx.send(MessageFrom::GenTimeStamp(pipeline_id.clone(), *syn_counter, utils::rdtsc_unsafe()))
                                .unwrap();
                        }
                        if syn_counter.bitand(8191u64) == 0 {
                            tx.send(MessageFrom::PrintPerformance(vec![pipeline_id.core as i32]));
                        }
                    }

                    debug!("SYN packet to server - L3: {}, L4: {}", h.ip, p.get_header());
                }

                /*
                let pipe_id = pipeline_id.clone();
                loop {
                    match rx.try_recv() {
                        Ok(MessageTo::Exit) => {
                            sm.send_all_c_records(&tx);
                            debug!("{}: exiting recv task", pipe_id);
                        }
                        Err(TryRecvError::Disconnected) => {
                            error!("{}: tried to receive from disconnected message channel", pipe_id);
                            break;
                        }
                        Err(TryRecvError::Empty) => break, // nothing in queue
                    };
                }
*/
                let mut group_index = 0usize; // the index of the group to be returned

                assert!(p.get_pre_header().is_some()); // we must have parsed the headers
                assert!(p.get_pre_pre_header().is_some()); // we must have parsed the headers

                let hs_ip;
                let hs_mac;
                let hs_tcp;

                // converting to raw pointer avoids to borrow mutably from p
                let ptr = p.get_mut_pre_header().unwrap() as *mut IpHeader;
                unsafe {
                    hs_ip = &mut *ptr;
                }
                let ptr = p.get_mut_pre_pre_header().unwrap() as *mut MacHeader;
                unsafe {
                    hs_mac = &mut *ptr;
                }
                let ptr = p.get_mut_header() as *mut TcpHeader;
                unsafe {
                    hs_tcp = &mut *ptr;
                }

                let mut hs = HeaderState {
                    mac: hs_mac,
                    ip: hs_ip,
                    tcp: hs_tcp,
                };

                // if set by the following tcp state machine,
                // the port/connection becomes released afterwards
                // this is cumbersome, but we must make the  borrow checker happy
                let mut release_connection = None;

                // check if we got a packet from generator
                if hs.mac.etype() == PRIVATE_ETYPE_TAG {
                    let opt_c = sm.create(&mut wheel);
                    if opt_c.is_some() {
                        let c = opt_c.unwrap();
                        generate_syn(
                            p,
                            c,
                            &mut hs,
                            &me,
                            &f_select_server,
                            &tx_clone,
                            &pipeline_id_clone,
                            &mut syn_counter,
                        );
                        c.con_rec.c_state = TcpState::SynSent;
                        c.con_rec.s_state = TcpState::SynReceived;
                    };
                    group_index = 1;
                } else {
                    // check that flow steering worked:
                    assert!(sm.owns_tcp_port(hs.tcp.dst_port()));

                    let mut c = sm.get_mut(hs.tcp.dst_port());
                    if c.is_some() {
                        let mut c = c.as_mut().unwrap();
                        let mut b_unexpected = false;
                        let old_s_state = c.con_rec.s_state;
                        let old_c_state = c.con_rec.c_state;

                        if hs.tcp.ack_flag() && hs.tcp.syn_flag() {
                            group_index = 1;
                            if (c.con_rec.s_state == TcpState::SynReceived) {
                                c.server_con_established();
                                tx_clone.send(MessageFrom::Established(c.con_rec.clone())).unwrap();
                                debug!(
                                    "established two-way client server connection, SYN-ACK received: L3: {}, L4: {}",
                                    hs.ip, hs.tcp
                                );
                                server_synack_received(p, &mut c, &mut hs, 1u32);
                            } else if (c.con_rec.s_state == TcpState::Established) {
                                server_synack_received(p, &mut c, &mut hs, 0u32);
                            } else {
                                group_index = 0;
                            } // ignore the SynAck
                        } else if hs.tcp.fin_flag() {
                            if c.con_rec.c_state >= TcpState::FinWait {
                                // got FIN receipt to a client initiated FIN
                                debug!("received FIN-reply from server on port {}", hs.tcp.dst_port());
                                c.con_rec.s_state = TcpState::LastAck;
                                c.con_rec.c_state = TcpState::Closed;
                            } else {
                                // server initiated TCP close
                                debug!(
                                    "server closes connection on port {}/{} in state {:?}",
                                    hs.tcp.dst_port(),
                                    c.get_client_sock().port(),
                                    c.con_rec.s_state,
                                );
                                c.con_rec.s_state = TcpState::FinWait;
                            }
                        } else if hs.tcp.rst_flag() {
                            c.con_rec.s_state = TcpState::Closed;
                            c.con_rec.c_state = TcpState::Listen;
                            c.con_rec.c_released(ReleaseCause::RstServer);
                            // release connection in the next block
                            release_connection = Some(c.p_port());
                        } else if c.con_rec.c_state == TcpState::LastAck && hs.tcp.ack_flag() {
                            // received final ack from server for server initiated close
                            debug!("received final ACK for server initiated close on port { }", hs.tcp.dst_port());
                            c.con_rec.s_state = TcpState::Closed;
                            c.con_rec.c_state = TcpState::Listen;
                            c.con_rec.c_released(ReleaseCause::FinServer);
                            // release connection in the next block
                            release_connection = Some(c.p_port());
                        } else {
                            // debug!("received from server { } in c/s state {:?}/{:?} ", hs.tcp, c.con_rec.c_state, c.con_rec.s_state);
                            b_unexpected = true; //  except we revise it, see below
                        }

                        // once we established a two-way e-2-e connection, we always forward server side packets
                        if old_s_state >= TcpState::Established && old_c_state >= TcpState::Established {
                            // translate packets and forward to client
                            server_to_client(p, &mut c, &mut hs, &me);
                            group_index = 1;
                            b_unexpected = false;
                        }

                        if b_unexpected {
                            warn!(
                                "{} unexpected server side TCP packet on port {}/{} in client/server state {:?}/{:?}, sending to KNI i/f",
                                thread_id_2,
                                hs.tcp.dst_port(),
                                c.get_client_sock().port(),
                                c.con_rec.c_state,
                                c.con_rec.s_state,
                            );
                            group_index = 2;
                        }
                    } else {
                        warn!("proxy has no state on port {}, sending to KNI i/f", hs.tcp.dst_port());
                        // we send this to KNI which handles out-of-order TCP, e.g. by sending RST
                        group_index = 2;
                    }
                }

                // here we check if we shall release the connection state,
                // required because of borrow checker for the state manager sm
                if let Some(sport) = release_connection {
                    debug!("releasing port {}", sport);
                    let con_rec = sm.release_port(sport);
                    if con_rec.is_some() {
                        tx_clone.send(MessageFrom::CRecord(con_rec.unwrap())).unwrap()
                    };
                }
                do_ttl(&mut hs);
                group_index
            },
            sched,
            uuid_l4groupby_clone,
        );

    let l2kniflow = l2groups.get_group(0).unwrap().compose();
    let l4kniflow = l4groups.get_group(2).unwrap().compose();
    let pipe2kni = merge(vec![l2kniflow, l4kniflow]).send(kni.clone());
    let l4pciflow = l4groups.get_group(1).unwrap().compose();
    let l4dumpflow = l4groups.get_group(0).unwrap().filter(box move |_| false).compose();
    let pipe2pci = merge(vec![l4pciflow, l4dumpflow]).send(pci.clone());
    let uuid_pipe2kni = Uuid::new_v4();
    let name = String::from("Pipe2Kni");
    sched.add_runnable(Runnable::from_task(uuid_pipe2kni, name, pipe2kni).unready());
    tx.send(MessageFrom::Task(pipeline_id.clone(), uuid_pipe2kni, TaskType::Pipe2Kni))
        .unwrap();
    let uuid_pipe2pci = Uuid::new_v4();
    let name = String::from("Pipe2Pci");
    sched.add_runnable(Runnable::from_task(uuid_pipe2pci, name, pipe2pci).unready());
    tx.send(MessageFrom::Task(pipeline_id.clone(), uuid_pipe2pci, TaskType::Pipe2Pci))
        .unwrap();
}
