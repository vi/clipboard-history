use std::{fs::File, io::Read};

use arrayvec::ArrayVec;
use clipboard_history_core::{protocol, protocol::Request};
use log::{info, warn};
use rustix::net::{AncillaryDrain, RecvAncillaryMessage};

use crate::{
    send_msg_bufs::{SendMsgBufs, Token},
    CliError,
};

pub fn connect(
    payload: &[u8],
    send_bufs: &mut SendMsgBufs,
) -> Result<(bool, (Token, *const libc::msghdr)), CliError> {
    info!("Establishing client/server protocol connection.");
    let version = payload[0];
    let valid = version == protocol::VERSION;
    if !valid {
        warn!(
            "Protocol version mismatch: expected {} but got {version}.",
            protocol::VERSION
        );
    }

    let response = send_bufs
        .alloc(
            0,
            1,
            |_| (),
            |buf| {
                buf[0].write(protocol::VERSION);
            },
        )
        .map_err(|()| CliError::Internal {
            context: "Didn't allocate enough send buffers.".into(),
        })?;

    Ok((valid, response))
}

pub fn handle(
    request: &Request,
    control_data: &mut [u8],
    send_bufs: &mut SendMsgBufs,
) -> Result<Option<(Token, *const libc::msghdr)>, CliError> {
    info!("Processing request: {request:?}");
    match request {
        Request::Add => {
            let mut fds = ArrayVec::<_, 1>::new();

            for message in unsafe { AncillaryDrain::parse(control_data) } {
                if let RecvAncillaryMessage::ScmRights(received_fds) = message {
                    fds.extend(received_fds);
                }
            }

            for fd in fds {
                let mut f = File::from(fd);
                let mut s = String::new();
                f.read_to_string(&mut s).unwrap();
                dbg!(s);
            }
            Ok(None)
        }
    }
}
