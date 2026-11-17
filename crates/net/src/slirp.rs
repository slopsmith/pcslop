//! User-mode networking through libslirp, the TCP/IP stack behind QEMU's
//! `-netdev user`. It needs no host privileges: guest connections become
//! ordinary sockets of this process.
//!
//! libslirp is not thread-safe, so one thread owns it and sleeps in poll(2)
//! on its sockets and a wake-up pipe. The emulator thread only touches two
//! queues and the pipe, so a send never blocks the vCPU on the network.

use std::collections::VecDeque;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::net::Ipv4Addr;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Instant;

use crate::Backend;
use crate::user::{self, UserNet};

mod ffi {
    use std::ffi::{c_char, c_int, c_void};

    #[repr(C)]
    pub struct Slirp {
        _opaque: [u8; 0],
    }

    pub type TimerCb = unsafe extern "C" fn(opaque: *mut c_void);
    pub type AddPollCb = unsafe extern "C" fn(fd: c_int, events: c_int, opaque: *mut c_void) -> c_int;
    pub type GetREventsCb = unsafe extern "C" fn(idx: c_int, opaque: *mut c_void) -> c_int;

    pub const POLL_IN: c_int = 1 << 0;
    pub const POLL_OUT: c_int = 1 << 1;
    pub const POLL_PRI: c_int = 1 << 2;
    pub const POLL_ERR: c_int = 1 << 3;
    pub const POLL_HUP: c_int = 1 << 4;

    /// `SlirpConfig` from libslirp.h, up to the version 5 fields.
    #[repr(C)]
    pub struct Config {
        pub version: u32,
        pub restricted: c_int,
        pub in_enabled: bool,
        pub vnetwork: libc::in_addr,
        pub vnetmask: libc::in_addr,
        pub vhost: libc::in_addr,
        pub in6_enabled: bool,
        pub vprefix_addr6: libc::in6_addr,
        pub vprefix_len: u8,
        pub vhost6: libc::in6_addr,
        pub vhostname: *const c_char,
        pub tftp_server_name: *const c_char,
        pub tftp_path: *const c_char,
        pub bootfile: *const c_char,
        pub vdhcp_start: libc::in_addr,
        pub vnameserver: libc::in_addr,
        pub vnameserver6: libc::in6_addr,
        pub vdnssearch: *const *const c_char,
        pub vdomainname: *const c_char,
        pub if_mtu: usize,
        pub if_mru: usize,
        pub disable_host_loopback: bool,
        pub enable_emu: bool,
        pub outbound_addr: *const libc::sockaddr_in,
        pub outbound_addr6: *const libc::sockaddr_in6,
        pub disable_dns: bool,
        pub disable_dhcp: bool,
        pub mfr_id: u32,
        pub oob_eth_addr: [u8; 6],
    }

    /// `SlirpCb` from libslirp.h.
    #[repr(C)]
    pub struct Callbacks {
        pub send_packet: unsafe extern "C" fn(buf: *const c_void, len: usize, opaque: *mut c_void) -> isize,
        pub guest_error: unsafe extern "C" fn(msg: *const c_char, opaque: *mut c_void),
        pub clock_get_ns: unsafe extern "C" fn(opaque: *mut c_void) -> i64,
        pub timer_new: Option<unsafe extern "C" fn(cb: TimerCb, cb_opaque: *mut c_void, opaque: *mut c_void) -> *mut c_void>,
        pub timer_free: unsafe extern "C" fn(timer: *mut c_void, opaque: *mut c_void),
        pub timer_mod: unsafe extern "C" fn(timer: *mut c_void, expire_ms: i64, opaque: *mut c_void),
        pub register_poll_fd: unsafe extern "C" fn(fd: c_int, opaque: *mut c_void),
        pub unregister_poll_fd: unsafe extern "C" fn(fd: c_int, opaque: *mut c_void),
        pub notify: unsafe extern "C" fn(opaque: *mut c_void),
        pub init_completed: unsafe extern "C" fn(slirp: *mut Slirp, opaque: *mut c_void),
        pub timer_new_opaque: unsafe extern "C" fn(id: c_int, cb_opaque: *mut c_void, opaque: *mut c_void) -> *mut c_void,
        pub register_poll_socket: unsafe extern "C" fn(fd: c_int, opaque: *mut c_void),
        pub unregister_poll_socket: unsafe extern "C" fn(fd: c_int, opaque: *mut c_void),
    }

    unsafe extern "C" {
        pub fn slirp_new(cfg: *const Config, callbacks: *const Callbacks, opaque: *mut c_void) -> *mut Slirp;
        pub fn slirp_cleanup(slirp: *mut Slirp);
        pub fn slirp_pollfds_fill_socket(slirp: *mut Slirp, timeout: *mut u32, add_poll: AddPollCb, opaque: *mut c_void);
        pub fn slirp_pollfds_poll(slirp: *mut Slirp, select_error: c_int, get_revents: GetREventsCb, opaque: *mut c_void);
        pub fn slirp_input(slirp: *mut Slirp, pkt: *const u8, pkt_len: c_int);
        pub fn slirp_handle_timer(slirp: *mut Slirp, id: c_int, cb_opaque: *mut c_void);
        pub fn slirp_add_hostfwd(
            slirp: *mut Slirp,
            is_udp: c_int,
            host_addr: libc::in_addr,
            host_port: c_int,
            guest_addr: libc::in_addr,
            guest_port: c_int,
        ) -> c_int;
    }
}

/// Frames waiting for the guest beyond this are dropped; TCP resends them.
const MAX_QUEUED: usize = 2048;
/// libslirp's `SlirpConfig` version whose fields we fill in.
const CONFIG_VERSION: u32 = 4;
/// Longest sleep in poll(2), so a stop request is never missed for long.
const MAX_POLL_MS: u32 = 1000;

struct Shared {
    to_guest: Mutex<VecDeque<Vec<u8>>>,
    to_host: Mutex<VecDeque<Vec<u8>>>,
    stop: AtomicBool,
    wake_write: RawFd,
    wake_guest: Box<dyn Fn() + Send + Sync>,
}

impl Shared {
    fn wake_slirp(&self) {
        let _ = unsafe { libc::write(self.wake_write, [0u8].as_ptr().cast(), 1) };
    }
}

/// A user-mode network with QEMU's layout: the guest is 10.0.2.15, the
/// gateway 10.0.2.2 and the DNS forwarder 10.0.2.3.
pub struct Slirp {
    shared: Arc<Shared>,
    wake_read: RawFd,
    thread: Option<JoinHandle<()>>,
}

impl Slirp {
    /// Starts the network thread and sets up the port forwards. `wake_guest`
    /// runs on that thread when frames arrive for the guest and none were waiting.
    pub fn start(cfg: &UserNet, wake_guest: Box<dyn Fn() + Send + Sync>) -> Result<Slirp, String> {
        let mut fds = [0; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(format!("cannot make a pipe: {}", std::io::Error::last_os_error()));
        }
        for fd in fds {
            unsafe {
                libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        let shared = Arc::new(Shared {
            to_guest: Mutex::new(VecDeque::new()),
            to_host: Mutex::new(VecDeque::new()),
            stop: AtomicBool::new(false),
            wake_write: fds[1],
            wake_guest,
        });
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_shared = shared.clone();
        let cfg = cfg.clone();
        let wake_read = fds[0];
        let thread = std::thread::Builder::new()
            .name("slirp".into())
            .spawn(move || run(thread_shared, wake_read, &cfg, ready_tx))
            .map_err(|e| e.to_string())?;
        let net = Slirp { shared, wake_read, thread: Some(thread) };
        ready_rx.recv().map_err(|_| "the network thread died".to_string())??;
        Ok(net)
    }
}

impl Backend for Slirp {
    fn send(&mut self, frame: &[u8]) {
        let was_empty = {
            let mut q = self.shared.to_host.lock().unwrap();
            q.push_back(frame.to_vec());
            q.len() == 1
        };
        if was_empty {
            self.shared.wake_slirp();
        }
    }

    fn recv(&mut self) -> Option<Vec<u8>> {
        self.shared.to_guest.lock().unwrap().pop_front()
    }
}

impl Drop for Slirp {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.wake_slirp();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        unsafe {
            libc::close(self.wake_read);
            libc::close(self.shared.wake_write);
        }
    }
}

struct Timer {
    id: c_int,
    cb_opaque: *mut c_void,
    expire_ms: Option<i64>,
}

/// State the callbacks reach through libslirp's opaque pointer. It lives on
/// the network thread's stack for as long as the `Slirp` instance does.
struct Ctx {
    shared: Arc<Shared>,
    start: Instant,
    timers: Vec<*mut Timer>,
    fds: Vec<libc::pollfd>,
}

impl Ctx {
    fn now_ms(&self) -> i64 {
        self.start.elapsed().as_millis() as i64
    }
}

fn in_addr(a: Ipv4Addr) -> libc::in_addr {
    libc::in_addr { s_addr: u32::from(a).to_be() }
}

fn run(shared: Arc<Shared>, wake_read: RawFd, cfg: &UserNet, ready: mpsc::Sender<Result<(), String>>) {
    let mut ctx = Ctx { shared, start: Instant::now(), timers: Vec::new(), fds: Vec::new() };
    let opaque = (&raw mut ctx).cast::<c_void>();
    let config = ffi::Config {
        version: CONFIG_VERSION,
        restricted: 0,
        in_enabled: true,
        vnetwork: in_addr(user::NETWORK),
        vnetmask: in_addr(user::NETMASK),
        vhost: in_addr(user::GATEWAY),
        in6_enabled: false,
        vprefix_addr6: unsafe { std::mem::zeroed() },
        vprefix_len: 0,
        vhost6: unsafe { std::mem::zeroed() },
        vhostname: std::ptr::null(),
        tftp_server_name: std::ptr::null(),
        tftp_path: std::ptr::null(),
        bootfile: std::ptr::null(),
        vdhcp_start: in_addr(user::GUEST),
        vnameserver: in_addr(user::DNS),
        vnameserver6: unsafe { std::mem::zeroed() },
        vdnssearch: std::ptr::null(),
        vdomainname: std::ptr::null(),
        if_mtu: 0,
        if_mru: 0,
        disable_host_loopback: false,
        enable_emu: false,
        outbound_addr: std::ptr::null(),
        outbound_addr6: std::ptr::null(),
        disable_dns: false,
        disable_dhcp: false,
        mfr_id: 0,
        oob_eth_addr: [0; 6],
    };
    let callbacks = ffi::Callbacks {
        send_packet,
        guest_error,
        clock_get_ns,
        timer_new: None,
        timer_free,
        timer_mod,
        register_poll_fd: ignore_fd,
        unregister_poll_fd: ignore_fd,
        notify,
        init_completed,
        timer_new_opaque,
        register_poll_socket: ignore_fd,
        unregister_poll_socket: ignore_fd,
    };
    let slirp = unsafe { ffi::slirp_new(&config, &callbacks, opaque) };
    if slirp.is_null() {
        let _ = ready.send(Err("libslirp refused its configuration".into()));
        return;
    }
    for f in &cfg.hostfwd {
        let r = unsafe {
            ffi::slirp_add_hostfwd(slirp, f.udp as c_int, in_addr(f.host_addr), f.host_port.into(), in_addr(f.guest_addr), f.guest_port.into())
        };
        if r < 0 {
            let proto = if f.udp { "udp" } else { "tcp" };
            let _ = ready.send(Err(format!("cannot forward {proto} {}:{}; is the port taken?", f.host_addr, f.host_port)));
            unsafe { ffi::slirp_cleanup(slirp) };
            return;
        }
    }
    let _ = ready.send(Ok(()));
    while !ctx.shared.stop.load(Ordering::Relaxed) {
        let frames: Vec<Vec<u8>> = ctx.shared.to_host.lock().unwrap().drain(..).collect();
        for f in frames {
            unsafe { ffi::slirp_input(slirp, f.as_ptr(), f.len() as c_int) };
        }
        ctx.fds.clear();
        ctx.fds.push(libc::pollfd { fd: wake_read, events: libc::POLLIN, revents: 0 });
        let mut timeout = MAX_POLL_MS;
        let now = ctx.now_ms();
        for &t in &ctx.timers {
            if let Some(at) = unsafe { (*t).expire_ms } {
                timeout = timeout.min(at.saturating_sub(now).clamp(0, MAX_POLL_MS as i64) as u32);
            }
        }
        unsafe { ffi::slirp_pollfds_fill_socket(slirp, &mut timeout, add_poll, opaque) };
        let n = unsafe { libc::poll(ctx.fds.as_mut_ptr(), ctx.fds.len() as libc::nfds_t, timeout as c_int) };
        if ctx.fds[0].revents & libc::POLLIN != 0 {
            let mut sink = [0u8; 64];
            while unsafe { libc::read(wake_read, sink.as_mut_ptr().cast(), sink.len()) } > 0 {}
        }
        unsafe { ffi::slirp_pollfds_poll(slirp, (n < 0) as c_int, get_revents, opaque) };
        let now = ctx.now_ms();
        let due: Vec<(c_int, *mut c_void)> = ctx
            .timers
            .iter()
            .filter_map(|&t| unsafe {
                let t = &mut *t;
                (t.expire_ms.is_some_and(|at| at <= now)).then(|| {
                    t.expire_ms = None;
                    (t.id, t.cb_opaque)
                })
            })
            .collect();
        for (id, cb_opaque) in due {
            unsafe { ffi::slirp_handle_timer(slirp, id, cb_opaque) };
        }
    }
    unsafe { ffi::slirp_cleanup(slirp) };
    for t in ctx.timers.drain(..) {
        drop(unsafe { Box::from_raw(t) });
    }
}

unsafe fn ctx<'a>(opaque: *mut c_void) -> &'a mut Ctx {
    unsafe { &mut *opaque.cast::<Ctx>() }
}

unsafe extern "C" fn send_packet(buf: *const c_void, len: usize, opaque: *mut c_void) -> isize {
    let ctx = unsafe { ctx(opaque) };
    let frame = unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), len) }.to_vec();
    let was_empty = {
        let mut q = ctx.shared.to_guest.lock().unwrap();
        if q.len() >= MAX_QUEUED {
            return len as isize;
        }
        q.push_back(frame);
        q.len() == 1
    };
    if was_empty {
        (ctx.shared.wake_guest)();
    }
    len as isize
}

unsafe extern "C" fn guest_error(msg: *const c_char, _opaque: *mut c_void) {
    eprintln!("slirp: {}", unsafe { CStr::from_ptr(msg) }.to_string_lossy());
}

unsafe extern "C" fn clock_get_ns(opaque: *mut c_void) -> i64 {
    unsafe { ctx(opaque) }.start.elapsed().as_nanos() as i64
}

unsafe extern "C" fn timer_new_opaque(id: c_int, cb_opaque: *mut c_void, opaque: *mut c_void) -> *mut c_void {
    let t = Box::into_raw(Box::new(Timer { id, cb_opaque, expire_ms: None }));
    unsafe { ctx(opaque) }.timers.push(t);
    t.cast()
}

unsafe extern "C" fn timer_free(timer: *mut c_void, opaque: *mut c_void) {
    let ctx = unsafe { ctx(opaque) };
    let t = timer.cast::<Timer>();
    ctx.timers.retain(|&x| x != t);
    drop(unsafe { Box::from_raw(t) });
}

unsafe extern "C" fn timer_mod(timer: *mut c_void, expire_ms: i64, _opaque: *mut c_void) {
    unsafe { (*timer.cast::<Timer>()).expire_ms = Some(expire_ms) };
}

unsafe extern "C" fn ignore_fd(_fd: c_int, _opaque: *mut c_void) {}

unsafe extern "C" fn notify(_opaque: *mut c_void) {}

unsafe extern "C" fn init_completed(_slirp: *mut ffi::Slirp, _opaque: *mut c_void) {}

unsafe extern "C" fn add_poll(fd: c_int, events: c_int, opaque: *mut c_void) -> c_int {
    let ctx = unsafe { ctx(opaque) };
    let mut ev = 0;
    for (s, p) in [(ffi::POLL_IN, libc::POLLIN), (ffi::POLL_OUT, libc::POLLOUT), (ffi::POLL_PRI, libc::POLLPRI)] {
        if events & s != 0 {
            ev |= p;
        }
    }
    ctx.fds.push(libc::pollfd { fd, events: ev, revents: 0 });
    (ctx.fds.len() - 1) as c_int
}

unsafe extern "C" fn get_revents(idx: c_int, opaque: *mut c_void) -> c_int {
    let ctx = unsafe { ctx(opaque) };
    let Some(fd) = ctx.fds.get(idx as usize) else { return 0 };
    let mut r = 0;
    for (s, p) in [
        (ffi::POLL_IN, libc::POLLIN),
        (ffi::POLL_OUT, libc::POLLOUT),
        (ffi::POLL_PRI, libc::POLLPRI),
        (ffi::POLL_ERR, libc::POLLERR),
        (ffi::POLL_HUP, libc::POLLHUP),
    ] {
        if fd.revents & p != 0 {
            r |= s;
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use std::mem::{offset_of, size_of};
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    use super::*;

    #[test]
    fn config_matches_the_c_layout() {
        assert_eq!(size_of::<ffi::Config>(), 192);
        assert_eq!(offset_of!(ffi::Config, vprefix_addr6), 28);
        assert_eq!(offset_of!(ffi::Config, vhostname), 64);
        assert_eq!(offset_of!(ffi::Config, vnameserver6), 104);
        assert_eq!(offset_of!(ffi::Config, if_mtu), 136);
        assert_eq!(offset_of!(ffi::Config, disable_dhcp), 177);
        assert_eq!(offset_of!(ffi::Config, oob_eth_addr), 184);
        assert_eq!(size_of::<ffi::Callbacks>(), 104);
        assert_eq!(offset_of!(ffi::Callbacks, timer_new_opaque), 80);
    }

    fn dhcp_discover() -> Vec<u8> {
        let mut f = vec![0xff; 6];
        f.extend_from_slice(&crate::DEFAULT_MAC);
        f.extend_from_slice(&[0x08, 0x00]);
        let mut dhcp = vec![1, 1, 6, 0, 0x12, 0x34, 0x56, 0x78, 0, 0, 0x80, 0];
        dhcp.extend_from_slice(&[0; 16]);
        dhcp.extend_from_slice(&crate::DEFAULT_MAC);
        dhcp.extend_from_slice(&[0; 10 + 64 + 128]);
        dhcp.extend_from_slice(&[99, 130, 83, 99, 53, 1, 1, 255]);
        let udp_len = 8 + dhcp.len();
        let ip_len = 20 + udp_len;
        let mut ip = vec![0x45, 0, (ip_len >> 8) as u8, ip_len as u8, 0, 0, 0, 0, 64, 17, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255];
        let mut sum: u32 = ip.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]]) as u32).sum();
        sum = (sum & 0xffff) + (sum >> 16);
        ip[10..12].copy_from_slice(&(!(sum as u16)).to_be_bytes());
        f.extend_from_slice(&ip);
        f.extend_from_slice(&[0, 68, 0, 67, (udp_len >> 8) as u8, udp_len as u8, 0, 0]);
        f.extend_from_slice(&dhcp);
        f
    }

    #[test]
    fn dhcp_offers_the_first_guest_address() {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let mut net = Slirp::start(&UserNet::default(), Box::new(move || tx.lock().unwrap().send(()).unwrap())).unwrap();
        net.send(&dhcp_discover());
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => panic!("no reply to DHCPDISCOVER"),
            Err(e) => panic!("{e}"),
        }
        let reply = net.recv().unwrap();
        assert_eq!(reply[..6], [0xff; 6], "the DISCOVER asked for a broadcast reply");
        let yiaddr = &reply[14 + 20 + 8 + 16..][..4];
        assert_eq!(yiaddr, user::GUEST.octets());
    }

    #[test]
    fn a_taken_forward_port_is_an_error() {
        let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = busy.local_addr().unwrap().port();
        let cfg = UserNet::parse(&format!("user,hostfwd=tcp::{port}-:22")).unwrap();
        assert!(Slirp::start(&cfg, Box::new(|| {})).is_err());
    }
}
