use std::{
    fs::File,
    io,
    io::{ErrorKind, Read as StdRead, Write},
    mem,
    os::fd::{AsRawFd, OwnedFd},
    path::PathBuf,
    ptr,
};

use io_uring::{
    cqueue::{buffer_select, more, Entry},
    opcode::{AcceptMulti, Close, PollAdd, RecvMsgMulti, SendMsg},
    squeue::{Flags, PushError},
    types::Fixed,
    IoUring,
};
use log::{debug, info, trace, warn};
use ringboard_core::{dirs::socket_file, init_unix_server, IoErr};
use rustix::{
    io::Errno,
    net::{RecvFlags, SocketType},
};

use crate::{
    allocator::Allocator,
    io_uring::{buf_ring::BufRing, register_buf_ring, types::RecvMsgOutMut},
    requests,
    send_msg_bufs::{SendMsgBufs, Token},
    CliError,
};

pub const MAX_NUM_CLIENTS: u8 = 1 << MAX_NUM_CLIENTS_SHIFT;
pub const MAX_NUM_BUFS_PER_CLIENT: u8 = 8;

const MAX_NUM_CLIENTS_SHIFT: u32 = 5;
const URING_ENTRIES: u8 = MAX_NUM_CLIENTS * 3;

#[derive(Default, Debug)]
struct Clients {
    connections: u32,
    pending_closes: u32,
    pending_recv: u32,
}

impl Clients {
    fn is_connected(&self, id: u8) -> bool {
        debug_assert!(id < MAX_NUM_CLIENTS);
        (self.connections & (1 << id)) != 0
    }

    fn is_closing(&self, id: u8) -> bool {
        debug_assert!(id < MAX_NUM_CLIENTS);
        (self.pending_closes & (1 << id)) != 0
    }

    fn set_connected(&mut self, id: u8) {
        debug_assert!(id < MAX_NUM_CLIENTS);
        self.connections |= 1 << id;
        self.pending_closes &= !(1 << id);
        self.pending_recv &= !(1 << id);
    }

    fn set_disconnected(&mut self, id: u8) {
        debug_assert!(id < MAX_NUM_CLIENTS);
        self.connections &= !(1 << id);
        self.pending_closes |= 1 << id;
    }

    fn set_closed(&mut self, id: u8) {
        debug_assert!(id < MAX_NUM_CLIENTS);
        self.connections &= !(1 << id);
        self.pending_closes &= !(1 << id);
        self.pending_recv &= !(1 << id);
    }

    fn set_pending_recv(&mut self, id: u8) {
        debug_assert!(id < MAX_NUM_CLIENTS);
        self.pending_recv |= 1 << id;
    }

    fn take_pending_recv(&mut self, id: u8) -> bool {
        debug_assert!(id < MAX_NUM_CLIENTS);
        let r = (self.pending_recv & (1 << id)) != 0;
        self.pending_recv &= !(1 << id);
        r
    }
}

fn setup_uring() -> Result<IoUring, CliError> {
    let uring = IoUring::<io_uring::squeue::Entry>::builder()
        .setup_coop_taskrun()
        .setup_single_issuer()
        .setup_defer_taskrun()
        .build(URING_ENTRIES.into())
        .map_io_err(|| "Failed to create io_uring.")?;

    let signal_handler = unsafe {
        use std::os::fd::FromRawFd;

        let mut set = mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&mut set);

        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGQUIT);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigprocmask(libc::SIG_BLOCK, &set, ptr::null_mut());

        let fd = libc::signalfd(-1, &set, 0);
        if fd < 0 {
            return Err(CliError::Internal {
                context: "Could not create signal fd.".into(),
            });
        }
        OwnedFd::from_raw_fd(fd)
    };

    let low_mem_listener = {
        let mut cgroup = String::with_capacity(160);
        cgroup.push_str("/sys/fs/cgroup");
        let start = cgroup.len();
        File::open("/proc/self/cgroup")
            .map_io_err(|| "Failed to open cgroup file: \"/proc/self/cgroup\"")?
            .read_to_string(&mut cgroup)
            .map_io_err(|| "Failed to read cgroup file: \"/proc/self/cgroup\"")?;
        if let Some((idx, _)) = cgroup.match_indices(':').nth(1) {
            cgroup.replace_range(start..=idx, "");
        }
        cgroup.truncate(cgroup.trim_end().len());

        let mut mem_pressure_path = PathBuf::from(cgroup);
        mem_pressure_path.push("memory.pressure");
        let mut mem_pressure = File::options()
            .read(true)
            .write(true)
            .open(&mem_pressure_path)
            .map_io_err(|| format!("Failed to open pressure file: {mem_pressure_path:?}"))?;

        mem_pressure
            .write_all(b"some 50000 2000000")
            .map_io_err(|| format!("Failed to write to pressure file: {mem_pressure_path:?}"))?;

        OwnedFd::from(mem_pressure)
    };

    let socket = init_unix_server(socket_file(), SocketType::SEQPACKET)?;

    let built_ins = [
        socket.as_raw_fd(),
        signal_handler.as_raw_fd(),
        low_mem_listener.as_raw_fd(),
    ];
    uring
        .submitter()
        .register_files_sparse(u32::from(MAX_NUM_CLIENTS) + u32::try_from(built_ins.len()).unwrap())
        .map_io_err(|| "Failed to set up io_uring fixed file table.")?;
    uring
        .submitter()
        .register_files_update(MAX_NUM_CLIENTS.into(), &built_ins)
        .map_io_err(|| "Failed to register socket FD with io_uring.")?;

    Ok(uring)
}

impl From<PushError> for CliError {
    fn from(_: PushError) -> Self {
        Self::Internal {
            context: "Mismanaged io_uring SQEs.".into(),
        }
    }
}

pub fn run(allocator: &mut Allocator) -> Result<(), CliError> {
    const REQ_TYPE_ACCEPT: u64 = 0;
    const REQ_TYPE_RECV: u64 = 1;
    const REQ_TYPE_CLOSE: u64 = 2;
    const REQ_TYPE_READ_SIGNALS: u64 = 3;
    const REQ_TYPE_SENDMSG: u64 = 4;
    const REQ_TYPE_LOW_MEM: u64 = 5;
    const REQ_TYPE_MASK: u64 = 0b111;
    const REQ_TYPE_SHIFT: u32 = REQ_TYPE_MASK.count_ones();

    let accept = AcceptMulti::new(Fixed(MAX_NUM_CLIENTS.into()))
        .allocate_file_index(true)
        .build()
        .user_data(REQ_TYPE_ACCEPT);
    let poll_low_mem = PollAdd::new(
        Fixed(u32::from(MAX_NUM_CLIENTS) + 2),
        u32::try_from(libc::POLLPRI).unwrap(),
    )
    .multi(true)
    .build()
    .user_data(REQ_TYPE_LOW_MEM);
    let receive_hdr = {
        let mut hdr = unsafe { mem::zeroed::<libc::msghdr>() };
        hdr.msg_controllen = 24;
        hdr
    };
    let recvmsg = |fd| {
        RecvMsgMulti::new(Fixed(u32::from(fd)), &receive_hdr, u16::from(fd))
            .flags(RecvFlags::TRUNC.bits())
            .build()
    };

    let store_fd = |fd| u64::from(fd) << (u64::BITS - MAX_NUM_CLIENTS_SHIFT);
    let restore_fd = |entry: &Entry| {
        u8::try_from(entry.user_data() >> (u64::BITS - MAX_NUM_CLIENTS_SHIFT)).unwrap()
    };

    let close = |fd| {
        Close::new(Fixed(u32::from(fd)))
            .build()
            .user_data(REQ_TYPE_CLOSE | store_fd(fd))
    };

    let mut uring = setup_uring()?;

    #[cfg(feature = "systemd")]
    sd_notify::notify(false, &[sd_notify::NotifyState::Ready])
        .map_io_err(|| "Failed to notify systemd of startup completion.")?;

    {
        let read_signals = PollAdd::new(
            Fixed(u32::from(MAX_NUM_CLIENTS) + 1),
            u32::try_from(libc::POLLIN).unwrap(),
        )
        .build()
        .user_data(REQ_TYPE_READ_SIGNALS);

        let mut submission = uring.submission();
        unsafe {
            submission
                .push_multiple(&[accept.clone(), read_signals, poll_low_mem.clone()])
                .unwrap();
        }
    }

    info!("Server event loop started.");

    let mut sequence_number = 0;
    let mut client_buffers = [const { None::<BufRing> }; MAX_NUM_CLIENTS as usize];
    let mut send_bufs = SendMsgBufs::new();
    let mut clients = Clients::default();
    let mut pending_accept = false;
    'outer: loop {
        {
            let want = uring.submission().is_empty().into();
            trace!("Waiting for at least {want} events.");
            match uring.submit_and_wait(want) {
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                r => r,
            }
            .map_io_err(|| "Failed to wait for io_uring.")?;
        }

        let mut completions = unsafe { uring.completion_shared() };
        let mut submissions = unsafe { uring.submission_shared() };
        loop {
            if submissions.capacity() - submissions.len() < 2 {
                break;
            }
            let Some(entry) = completions.next() else {
                break;
            };

            let result = u32::try_from(entry.result())
                .map_err(|_| io::Error::from_raw_os_error(-entry.result()));
            match entry.user_data() & REQ_TYPE_MASK {
                REQ_TYPE_ACCEPT => 'accept: {
                    debug!("Handling accept completion.");
                    let client = match result {
                        Err(e) if e.raw_os_error() == Some(Errno::NFILE.raw_os_error()) => {
                            warn!("Too many clients clients connected, dropping connection.");
                            pending_accept = true;
                            break 'accept;
                        }
                        r => r.map_io_err(|| "Failed to accept socket connection.")?,
                    };
                    debug_assert!(client < u32::from(MAX_NUM_CLIENTS));
                    #[allow(clippy::cast_possible_truncation)]
                    let client = client as u8;
                    debug!("Accepting client {client}.");

                    debug_assert!(client_buffers[usize::from(client)].is_none());
                    client_buffers[usize::from(client)] = Some(
                        register_buf_ring(
                            &uring.submitter(),
                            MAX_NUM_BUFS_PER_CLIENT.into(),
                            client.into(),
                            256,
                        )
                        .map_io_err(|| "Failed to register buffer ring with io_uring.")?,
                    );

                    if !more(entry.flags()) {
                        unsafe { submissions.push(&accept) }?;
                    }
                    let recv = recvmsg(client).user_data(REQ_TYPE_RECV | store_fd(client));
                    unsafe { submissions.push(&recv) }?;
                }
                REQ_TYPE_RECV => 'recv: {
                    let fd = restore_fd(&entry);
                    debug!("Handling recv completion for client {fd}.");
                    match result {
                        Err(e)
                            if [Errno::MSGSIZE, Errno::NOBUFS]
                                .iter()
                                .any(|kind| e.raw_os_error() == Some(kind.raw_os_error())) =>
                        {
                            warn!("No buffers available to receive client {fd}'s message.");
                            clients.set_pending_recv(fd);
                            break 'recv;
                        }
                        Err(e) if e.kind() == ErrorKind::ConnectionReset => {
                            warn!("Client {fd} reset the connection.");
                            unsafe { submissions.push(&close(fd)) }?;
                            clients.set_disconnected(fd);
                            break 'recv;
                        }
                        r => r.map_io_err(|| format!("Failed to recv from client {fd}."))?,
                    };

                    debug_assert!(buffer_select(entry.flags()).is_some());
                    let mut buf_submissions = client_buffers[usize::from(fd)]
                        .as_mut()
                        .unwrap()
                        .submissions();
                    let mut buf = unsafe {
                        buf_submissions.get(entry.flags(), usize::try_from(entry.result()).unwrap())
                    };
                    let msg = RecvMsgOutMut::parse(&mut buf, &receive_hdr).map_err(|()| {
                        CliError::Internal {
                            context: "Didn't allocate enough large enough buffers.".into(),
                        }
                    })?;
                    if msg.is_name_data_truncated()
                        || msg.is_control_data_truncated()
                        || msg.is_payload_truncated()
                    {
                        return Err(CliError::Internal {
                            context: "Received data was truncated.".into(),
                        });
                    }

                    if msg.payload_data.is_empty() {
                        debug!("Client {fd} closed the connection.");
                        if !clients.is_closing(fd) {
                            unsafe { submissions.push(&close(fd)) }?;
                            clients.set_disconnected(fd);
                        }
                    } else {
                        if clients.is_closing(fd) {
                            debug!("Dropping spurious message for client {fd}.");
                            break 'recv;
                        }

                        let response = if clients.is_connected(fd) {
                            requests::handle(
                                msg.payload_data,
                                msg.control_data,
                                &mut send_bufs,
                                allocator,
                                &mut sequence_number,
                            )?
                        } else {
                            let (version_valid, resp) =
                                requests::connect(msg.payload_data, &mut send_bufs)?;
                            if version_valid {
                                info!("Client {fd} connected.");
                                clients.set_connected(fd);
                            } else {
                                clients.set_disconnected(fd);
                            }
                            Some(resp)
                        };
                        if let Some((token, msghdr)) = response {
                            let send = SendMsg::new(Fixed(fd.into()), msghdr)
                                .build()
                                .flags(if clients.is_connected(fd) {
                                    Flags::empty()
                                } else {
                                    Flags::IO_LINK
                                })
                                .user_data(
                                    REQ_TYPE_SENDMSG
                                        | (u64::from(token) << REQ_TYPE_SHIFT)
                                        | (u64::from(buf.into_index())
                                            << (REQ_TYPE_SHIFT + Token::BITS))
                                        | store_fd(fd),
                                );
                            unsafe { submissions.push(&send) }?;
                        }

                        if clients.is_connected(fd) {
                            if !more(entry.flags()) {
                                let recv = recvmsg(fd).user_data(entry.user_data());
                                unsafe { submissions.push(&recv) }?;
                            }
                        } else {
                            unsafe { submissions.push(&close(fd)) }?;
                        }
                    }
                }
                REQ_TYPE_SENDMSG => 'send: {
                    let fd = restore_fd(&entry);
                    debug!("Handling sendmsg completion for client {fd}.");

                    {
                        let token = entry.user_data() >> REQ_TYPE_SHIFT;
                        unsafe {
                            send_bufs.free(token);
                        }
                    }
                    {
                        let index = entry.user_data() >> (REQ_TYPE_SHIFT + u8::BITS);
                        let index = u16::try_from(index & u64::from(u16::MAX)).unwrap();
                        let mut submissions = client_buffers[usize::from(fd)]
                            .as_mut()
                            .unwrap()
                            .submissions();
                        unsafe {
                            submissions.recycle_by_index(index);
                        }
                    }

                    match result {
                        Err(e) if e.kind() == ErrorKind::BrokenPipe => {
                            if !clients.is_closing(fd) {
                                debug!(
                                    "Client {fd} closed the connection before consuming all \
                                     responses."
                                );
                                unsafe { submissions.push(&close(fd)) }?;
                                clients.set_disconnected(fd);
                            }
                            break 'send;
                        }
                        Err(e) if e.kind() == ErrorKind::ConnectionReset => {
                            if !clients.is_closing(fd) {
                                warn!("Client {fd} forcefully disconnected.");
                                unsafe { submissions.push(&close(fd)) }?;
                                clients.set_disconnected(fd);
                            }
                            break 'send;
                        }
                        r => {
                            r.map_io_err(|| format!("Failed to send response to client {fd}."))?;
                        }
                    };

                    if !clients.is_closing(fd)
                        && clients.is_connected(fd)
                        && clients.take_pending_recv(fd)
                    {
                        info!("Restoring client {fd}'s connection.");
                        let recv = recvmsg(fd).user_data(REQ_TYPE_RECV | store_fd(fd));
                        unsafe { submissions.push(&recv) }?;
                    }
                }
                REQ_TYPE_CLOSE => {
                    let fd = restore_fd(&entry);
                    debug!("Handling close completion for client {fd}.");
                    result.map_io_err(|| format!("Failed to close client {fd}."))?;
                    info!("Client {fd} disconnected.");

                    clients.set_closed(fd);
                    if let Some(bufs) = mem::take(&mut client_buffers[usize::from(fd)]) {
                        bufs.unregister(&uring.submitter())
                            .map_io_err(|| "Failed to unregister buffer ring with io_uring.")?;
                    }

                    if pending_accept && clients.pending_closes == 0 {
                        info!("Restoring ability to accept new clients.");
                        unsafe { submissions.push(&accept) }?;
                        pending_accept = false;
                    }
                }
                REQ_TYPE_READ_SIGNALS => {
                    debug!("Handling read_signals completion.");
                    let result = result.map_io_err(|| "Failed to poll for signals.")?;
                    if (result & u32::try_from(libc::POLLIN).unwrap()) == 0 {
                        return Err(CliError::Internal {
                            context: format!("Unknown signal poll event received: {result}").into(),
                        });
                    }

                    break 'outer;
                }
                REQ_TYPE_LOW_MEM => {
                    debug!("Handling low memory completion.");
                    let result = result.map_io_err(|| "Failed to poll for low memory events.")?;

                    if !more(entry.flags()) {
                        unsafe { submissions.push(&poll_low_mem) }?;
                    }

                    if (result & u32::try_from(libc::POLLERR).unwrap()) != 0 {
                        return Err(CliError::Internal {
                            context: "Error polling for low memory events".into(),
                        });
                    } else if (result & u32::try_from(libc::POLLPRI).unwrap()) != 0 {
                        send_bufs.trim();
                    } else {
                        return Err(CliError::Internal {
                            context: format!("Unknown low memory poll event received: {result}")
                                .into(),
                        });
                    }
                }
                _ => unreachable!(),
            }
        }
    }
    Ok(())
}
