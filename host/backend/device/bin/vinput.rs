// SPDX-License-Identifier: Apache-2.0
//
// The virtio-input devices over vhost-user (QEMU `vhost-user-input-pci`):
// one daemon per device, each on its own socket. What a device reports and
// which input it takes is `device::vinput`; this is the transport.
//
// Events are written into the event queue by whoever sends them -- the
// display link's thread -- and the guest is interrupted at once through the
// call eventfd (KVM's irqfd). The daemon's own thread only answers QEMU's
// messages (features, config, rings) and the guest's kicks: on the event
// queue a kick means new buffers, and events that found none are put in
// them; on the status queue (the keyboard's LEDs) the buffers are handed
// straight back.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, RwLock};

use device::chain::{writable, write_scattered};
use device::vinput::{
    CONFIG_SIZE, Config, EVENT_QUEUE, Kind, MAX_QUEUE_SIZE, NUM_QUEUES, Port, STATUS_QUEUE,
};
use protocol::messages::InputEventEntry;
use vhost::vhost_user::Listener;
use vhost::vhost_user::message::{VhostUserProtocolFeatures, VhostUserVirtioFeatures};
use vhost_user_backend::{VhostUserBackendMut, VhostUserDaemon, VringRwLock, VringT};
use virtio_bindings::bindings::virtio_config::VIRTIO_F_VERSION_1;
use virtio_queue::{QueueOwnedT, QueueT};
use vm_memory::{GuestAddressSpace, GuestMemoryAtomic, GuestMemoryMmap};

/// Events kept for a guest that has every buffer out. A driver serves its
/// queue in its interrupt handler, so a backlog this deep means it has
/// stopped; the device then counts as not ready and input goes to the boot
/// console until the driver takes buffers again.
const BACKLOG_MAX: usize = 1024;

#[derive(Default)]
struct PortState {
    target: Option<(VringRwLock, GuestMemoryAtomic<GuestMemoryMmap>)>,
    mem: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
    backlog: VecDeque<[u8; 8]>,
    /// The guest has posted buffers since the queue last started.
    posted: bool,
    sent: u64,
    dropped: u64,
}

impl PortState {
    fn live(&mut self) -> bool {
        let Some((vring, mem)) = self.target.as_ref() else {
            return false;
        };
        let vr = vring.get_ref();
        let q = vr.get_queue();
        if !q.ready() || q.avail_ring() == 0 {
            self.posted = false;
            return false;
        }
        if !self.posted {
            self.posted = q.next_avail() != 0
                || q.avail_idx(&*mem.memory(), std::sync::atomic::Ordering::Acquire)
                    .is_ok_and(|i| i.0 != 0);
        }
        self.posted
    }

    /// Put backlogged events into posted buffers; interrupt the guest once
    /// if any went.
    fn drain(&mut self) {
        let Some((vring, mem)) = self.target.clone() else {
            return;
        };
        let guard = mem.memory();
        let mut placed = false;
        while let Some(ev) = self.backlog.front().copied() {
            let mut vr = vring.get_mut();
            let Ok(mut avail) = vr.get_queue_mut().iter(guard.clone()) else {
                break;
            };
            let Some(chain) = avail.next() else {
                break; // every buffer is out; the guest's next kick brings more
            };
            let head = chain.head_index();
            drop(vr);
            let segs = writable(chain);
            let written = write_scattered(&*guard, &segs, &ev).unwrap_or(0);
            if vring.add_used(head, written as u32).is_err() {
                break;
            }
            self.backlog.pop_front();
            self.sent += 1;
            placed = true;
        }
        if placed {
            let _ = vring.signal_used_queue();
        }
    }
}

/// One device's event queue, shared by the daemon and the display link.
pub struct VhostPort {
    kind: Kind,
    st: Mutex<PortState>,
}

impl VhostPort {
    pub fn new(kind: Kind) -> Arc<Self> {
        Arc::new(Self {
            kind,
            st: Mutex::new(PortState::default()),
        })
    }
}

impl Port for VhostPort {
    fn ready(&self) -> bool {
        let mut st = self.st.lock().unwrap();
        st.live() && st.backlog.len() < BACKLOG_MAX
    }

    fn send(&self, events: &[InputEventEntry]) {
        let mut st = self.st.lock().unwrap();
        if !st.live() {
            st.dropped += events.len() as u64;
            return;
        }
        if st.backlog.len() + events.len() > BACKLOG_MAX {
            if st.dropped == 0 {
                log::warn!(
                    "virtio-input: {}: the guest has taken no buffer for {} events; dropping",
                    self.kind.name(),
                    st.backlog.len()
                );
            }
            st.dropped += events.len() as u64;
            return;
        }
        st.backlog
            .extend(events.iter().map(InputEventEntry::to_bytes));
        st.drain();
    }
}

/// The vhost-user side of one device.
struct InputBackend {
    port: Arc<VhostPort>,
    config: Config,
}

impl VhostUserBackendMut for InputBackend {
    type Bitmap = ();
    type Vring = VringRwLock;

    fn num_queues(&self) -> usize {
        NUM_QUEUES
    }

    fn max_queue_size(&self) -> usize {
        MAX_QUEUE_SIZE
    }

    fn features(&self) -> u64 {
        (1 << VIRTIO_F_VERSION_1) | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::MQ | VhostUserProtocolFeatures::CONFIG
    }

    fn get_config(&self, offset: u32, size: u32) -> Vec<u8> {
        self.config.read(offset, size)
    }

    fn set_config(&mut self, offset: u32, buf: &[u8]) -> std::io::Result<()> {
        if offset as usize + buf.len() > CONFIG_SIZE {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
        }
        self.config.write(offset, buf);
        Ok(())
    }

    fn set_event_idx(&mut self, _enabled: bool) {}

    fn update_memory(&mut self, mem: GuestMemoryAtomic<GuestMemoryMmap>) -> std::io::Result<()> {
        let mut st = self.port.st.lock().unwrap();
        st.mem = Some(mem.clone());
        if let Some((v, _)) = st.target.take() {
            st.target = Some((v, mem));
        }
        Ok(())
    }

    fn handle_event(
        &mut self,
        device_event: u16,
        _evset: vmm_sys_util::epoll::EventSet,
        vrings: &[VringRwLock],
        _thread_id: usize,
    ) -> std::io::Result<()> {
        match device_event as usize {
            EVENT_QUEUE => {
                let mut st = self.port.st.lock().unwrap();
                let Some(mem) = st.mem.clone() else {
                    return Ok(());
                };
                if st.target.is_none() {
                    log::info!(
                        "virtio-input: {}: the guest's driver posted its first buffers",
                        self.port.kind.name()
                    );
                }
                st.target = Some((vrings[EVENT_QUEUE].clone(), mem));
                st.drain();
            }
            STATUS_QUEUE => {
                // LED state from the guest: nothing on the host to light.
                let Some(mem) = self.port.st.lock().unwrap().mem.clone() else {
                    return Ok(());
                };
                let vring = &vrings[STATUS_QUEUE];
                let guard = mem.memory();
                let mut used = false;
                loop {
                    let mut vr = vring.get_mut();
                    let Ok(mut avail) = vr.get_queue_mut().iter(guard.clone()) else {
                        break;
                    };
                    let Some(chain) = avail.next() else {
                        break;
                    };
                    let head = chain.head_index();
                    drop(vr);
                    if vring.add_used(head, 0).is_err() {
                        break;
                    }
                    used = true;
                }
                if used {
                    let _ = vring.signal_used_queue();
                }
            }
            n => {
                return Err(std::io::Error::other(format!(
                    "virtio-input: event for unknown queue {n}"
                )));
            }
        }
        Ok(())
    }
}

/// Serve device `kind` on `listener` (one frontend connection: the VM's
/// QEMU) on a thread of its own. Returns the port the router writes to.
pub fn serve(kind: Kind, mut listener: Listener) -> anyhow::Result<Arc<VhostPort>> {
    let port = VhostPort::new(kind);
    let backend = Arc::new(RwLock::new(InputBackend {
        port: port.clone(),
        config: Config::new(kind),
    }));
    let mut daemon = VhostUserDaemon::new(
        format!("conduit-input-{}", kind.name()),
        backend,
        GuestMemoryAtomic::new(GuestMemoryMmap::new()),
    )
    .map_err(|e| anyhow::anyhow!("virtio-input {}: create daemon: {e:?}", kind.name()))?;
    let p = port.clone();
    std::thread::Builder::new()
        .name(format!("vinput-{}", kind.name()))
        .spawn(move || {
            let r = daemon.start(&mut listener).and_then(|()| daemon.wait());
            for h in daemon.get_epoll_handlers() {
                h.send_exit_event();
            }
            match r {
                Ok(()) => log::info!("virtio-input: {}: the VM disconnected", kind.name()),
                Err(e) => log::info!("virtio-input: {}: connection ended: {e:?}", kind.name()),
            }
            let st = p.st.lock().unwrap();
            log::info!(
                "virtio-input: {}: {} events delivered, {} dropped",
                kind.name(),
                st.sent,
                st.dropped
            );
        })?;
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_without_a_guest_is_not_ready_and_drops() {
        let p = VhostPort::new(Kind::Mouse);
        assert!(!p.ready());
        p.send(&[InputEventEntry::new(2, 0, 1)]);
        let st = p.st.lock().unwrap();
        assert_eq!((st.sent, st.dropped, st.backlog.len()), (0, 1, 0));
    }

    /// Events go into the buffers the guest posted, one each; what finds
    /// none waits, in order, for the next buffer (the guest's kick).
    #[test]
    fn events_land_in_posted_buffers_and_the_rest_waits_for_more() {
        use virtio_bindings::bindings::virtio_ring::VRING_DESC_F_WRITE;
        use virtio_queue::desc::{RawDescriptor, split::Descriptor as SplitDescriptor};
        use virtio_queue::mock::MockSplitQueue;
        use vm_memory::{Bytes, GuestAddress};

        let gm = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x20_0000)]).unwrap();
        let vq = MockSplitQueue::new(&gm, 16);
        let buf = |i: u64| {
            RawDescriptor::from(SplitDescriptor::new(
                0x10_0000 + i * 0x100,
                8,
                VRING_DESC_F_WRITE as u16,
                0,
            ))
        };
        vq.add_desc_chains(&[buf(0), buf(1)], 0).unwrap();
        let mem = GuestMemoryAtomic::new(gm.clone());
        let vring = VringRwLock::new(mem.clone(), 16).unwrap();
        vring.set_queue_size(16);
        vring
            .set_queue_info(vq.desc_table_addr().0, vq.avail_addr().0, vq.used_addr().0)
            .unwrap();
        vring.set_queue_ready(true);

        let p = VhostPort::new(Kind::Mouse);
        {
            let mut st = p.st.lock().unwrap();
            st.mem = Some(mem.clone());
            st.target = Some((vring, mem));
        }
        assert!(p.ready());
        let evs = [
            InputEventEntry::new(2, 0, 5),
            InputEventEntry::new(2, 1, -7),
            InputEventEntry::new(0, 0, 0),
        ];
        p.send(&evs);
        assert_eq!(vq.used().idx().load(), 2);
        assert_eq!(
            gm.read_obj::<[u8; 8]>(GuestAddress(0x10_0000)).unwrap(),
            evs[0].to_bytes()
        );
        assert_eq!(
            gm.read_obj::<[u8; 8]>(GuestAddress(0x10_0100)).unwrap(),
            evs[1].to_bytes()
        );
        assert_eq!(p.st.lock().unwrap().backlog.len(), 1);

        // The guest hands one buffer back (and kicks).
        vq.add_desc_chains(&[buf(2)], 2).unwrap();
        p.st.lock().unwrap().drain();
        assert_eq!(vq.used().idx().load(), 3);
        assert_eq!(
            gm.read_obj::<[u8; 8]>(GuestAddress(0x10_0200)).unwrap(),
            evs[2].to_bytes()
        );
        let st = p.st.lock().unwrap();
        assert_eq!((st.sent, st.dropped, st.backlog.len()), (3, 0, 0));
    }

    #[test]
    fn config_reads_and_writes_go_through_the_backend() {
        let mut b = InputBackend {
            port: VhostPort::new(Kind::Keyboard),
            config: Config::new(Kind::Keyboard),
        };
        b.set_config(0, &[0x01, 0]).unwrap();
        let c = b.get_config(0, CONFIG_SIZE as u32);
        assert_eq!(&c[8..8 + c[2] as usize], b"Conduit Keyboard");
        assert!(b.set_config(130, &[0; 10]).is_err());
        assert_eq!(b.num_queues(), 2);
    }
}
