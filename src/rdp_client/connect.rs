//! Everything between a socket and a desktop.
//!
//! One long sequence, and every step of it is a question the host answers before the
//! next may be asked: the security negotiation, the TLS session, the credentials, the
//! channels, the logon, the licence, the capabilities, and the four PDUs that say the
//! share is live. [`super::proto`] is each PDU; this is the order they go in.
//!
//! It is written as one function on purpose. The steps share almost nothing but the
//! socket and what the last one said, and split across a state machine the thing a
//! reader wants — what is sent, and what has to come back — would be the one thing not
//! written down anywhere.

use anyhow::{Context as _, Result, bail};
use log::{debug, info};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, ReadHalf, WriteHalf};
use tokio::net::TcpStream;

use super::proto::capabilities::{ConfirmActive, DemandActive};
use super::proto::credssp::{self, Credentials};
use super::proto::finalization::{self, Response};
use super::proto::frame::Frames;
use super::proto::gcc::{Channel, ConferenceCreateRequest, ConferenceCreateResponse};
use super::proto::info::ClientInfo;
use super::proto::share::{self, Pdu};
use super::proto::x224::{ConnectionConfirm, ConnectionRequest, Security, TPKT_HEADER};
use super::proto::{fastpath, license, mcs, tls, x224};
use super::session::Connect;
use crate::engine;

/// The keyboard layout the session is opened with: US English, which every Windows
/// host has. It says nothing about which keys arrive — a scancode is a position on a
/// keyboard, and the host maps it with the layout the *session* is using — so this is
/// bookkeeping the server keeps rather than anything input depends on.
const KEYBOARD_LAYOUT: u32 = 0x0409;

/// The static virtual channels a session asks for, each one a capability the caller
/// turned on: [`Channel::DYNAMIC`] is the transport Display Control and the graphics
/// pipeline both ride on, and [`Channel::CLIPBOARD`] is MS-RDPECLIP itself. A session
/// that wants none of them asks for no channel at all.
///
/// The order is what makes the server's answer readable: `SC_NET` numbers the
/// channels in the order `CS_NET` named them and says nothing else about which is
/// which, so the numbers are paired back up with the names here — see
/// [`Connected::channel`].
fn wanted_channels(config: &Connect) -> Vec<Channel> {
    let mut channels = Vec::new();
    if config.resize || config.egfx {
        channels.push(Channel::DYNAMIC);
    }
    if config.clipboard {
        channels.push(Channel::CLIPBOARD);
    }
    if config.audio.is_some() {
        // Both, because a Windows host redirects sound only to a client that named
        // device redirection too — see `proto/rdpdr.rs`.
        channels.push(Channel::AUDIO);
        channels.push(Channel::DEVICES);
    }
    channels
}

/// A static virtual channel this session joined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Joined {
    /// The number the server gave it, which every PDU on it is addressed to.
    pub number: u16,
    /// What every chunk of this channel's PDUs wears beyond first and last — see
    /// [`Channel::chunk_flags`].
    pub flags: u32,
}

/// A live share, and everything later PDUs are addressed with.
pub(super) struct Connected {
    pub frames: Frames<ReadHalf<tls::Stream>>,
    pub writer: WriteHalf<tls::Stream>,
    /// This client's own MCS user, which every PDU it sends names as its source.
    pub user: u16,
    /// Where the desktop and everything about it travels.
    pub io_channel: u16,
    /// Every static virtual channel that was asked for, with the number the server
    /// gave it — read by name through [`Connected::channel`].
    pub channels: Vec<(Channel, u16)>,
    /// What the server said when it opened the share.
    pub demand: DemandActive,
    /// PDUs that arrived on a static virtual channel while the share was being
    /// finalized, in the order they came — see [`activate`].
    pub deferred: Vec<(u16, Vec<u8>)>,
}

impl Connected {
    /// One channel as it was joined, or `None` for a channel this session never asked
    /// for.
    pub fn channel(&self, wanted: Channel) -> Option<Joined> {
        self.channels
            .iter()
            .find(|(channel, _)| *channel == wanted)
            .map(|(channel, number)| Joined { number: *number, flags: channel.chunk_flags() })
    }
}

/// TCP, X.224, TLS, CredSSP, MCS, the logon, and the capability exchange — up to the
/// first desktop.
///
/// The socket is [`engine::tcp_connect`]'s, so an RDP host that is switched off is
/// noticed on the same keepalive schedule as every other engine's.
pub(super) async fn connect(config: &Connect) -> Result<Connected> {
    let dest = engine::host_port(&config.host, config.port);
    let mut tcp = engine::tcp_connect(&dest).await?;
    // This end of the socket, which the Client Info PDU tells the server about.
    let client_address = tcp.local_addr().context("reading the local address")?.ip();
    // The name TLS and CredSSP are checked against. A bracketed IPv6 literal is how
    // `host_port` writes one, not a name either check understands.
    let server_name = config.host.trim_start_matches('[').trim_end_matches(']').to_owned();

    // 1. The security negotiation, in the clear. Only NLA is offered, so only NLA
    //    can be chosen — see the module doc on `proto`.
    let request = ConnectionRequest {
        cookie: Some(config.username.clone()),
        protocols: Security::HYBRID,
    };
    tcp.write_all(&request.encode()).await.context("sending the X.224 Connection Request")?;
    let frame = read_frame(&mut tcp).await.context("reading the X.224 Connection Confirm")?;
    let protocol = match ConnectionConfirm::decode(&frame)? {
        ConnectionConfirm::Negotiated { protocol, .. } if protocol == Security::HYBRID => protocol,
        ConnectionConfirm::Negotiated { protocol, .. } => {
            bail!("{dest} chose {protocol:?}, and this client speaks only NLA")
        }
        // A server that answers without negotiation data wants the legacy RDP
        // security this client does not implement, which is the same refusal.
        ConnectionConfirm::Unnegotiated => {
            bail!("{dest} offered no security negotiation, and this client speaks only NLA")
        }
        ConnectionConfirm::Refused(why) => bail!("{dest} refused the connection: {why:?}"),
    };

    // 2. The TLS session everything after this lives inside, and 3. the credentials,
    //    which CredSSP binds to that session's public key.
    let mut stream = tls::upgrade(tcp, &server_name).await?;
    let public_key = tls::public_key(&stream)?;
    let credentials = Credentials {
        username: &config.username,
        password: &config.password,
        domain: config.domain.as_deref(),
    };
    credssp::authenticate(&mut stream, &server_name, &credentials, public_key).await?;
    debug!("rdp: authenticated as {}", config.username);

    let (reader, mut writer) = tokio::io::split(stream);
    let mut frames = Frames::new(reader);
    let mut frame = Vec::new();

    // 4. MCS Connect-Initial, carrying the GCC conference. The answer numbers every
    //    channel the session will use.
    let wanted = wanted_channels(config);
    let conference = ConferenceCreateRequest {
        width: narrow(config.width),
        height: narrow(config.height),
        client_name: "remotex",
        keyboard_layout: KEYBOARD_LAYOUT,
        selected_protocol: protocol.bits(),
        channels: &wanted,
        graphics: config.egfx,
    }
    .encode();
    writer
        .write_all(&mcs::connect_initial(&conference)?)
        .await
        .context("sending the MCS Connect-Initial")?;
    frames.next(&mut frame).await?;
    let answer = mcs::connect_response(&frame)?;
    let answer = ConferenceCreateResponse::decode(answer)?;
    let ConferenceCreateResponse { io_channel, channels } = answer;
    if channels.len() != wanted.len() {
        let asked = wanted.len();
        bail!("the host numbered {} channels, and {asked} were asked for", channels.len());
    }
    // Paired with the names in the order both sides listed them, which is the only
    // thing that says which number is which channel.
    let numbered: Vec<(Channel, u16)> =
        wanted.iter().copied().zip(channels.iter().copied()).collect();
    for (channel, number) in &numbered {
        debug!("rdp: the host numbered {} channel {number}", channel.name);
    }

    // 5. Erect the domain — which is not answered — and attach a user to it.
    writer.write_all(&mcs::erect_domain_request()).await.context("sending the MCS Erect Domain")?;
    writer.write_all(&mcs::attach_user_request()).await.context("sending the MCS Attach User")?;
    frames.next(&mut frame).await?;
    let user = mcs::attach_user_confirm(&frame)?;

    // 6. Join every channel, the user's own first. Each is its own round trip.
    for channel in [user, io_channel].into_iter().chain(channels) {
        writer
            .write_all(&mcs::channel_join_request(user, channel))
            .await
            .context("sending an MCS Channel Join Request")?;
        frames.next(&mut frame).await?;
        let joined = mcs::channel_join_confirm(&frame)?;
        if joined != channel {
            bail!("the host joined channel {joined} where {channel} was asked for");
        }
    }

    // 7. The logon. The credentials went through CredSSP already; this says to use
    //    them rather than to show a logon screen.
    let logon = ClientInfo {
        username: &config.username,
        password: &config.password,
        domain: config.domain.as_deref(),
        address: client_address,
        audio: config.audio.is_some(),
    }
    .encode()?;
    send(&mut writer, user, io_channel, &logon).await.context("sending the Client Info PDU")?;

    // 8. Licensing, which on a host like this one is one PDU saying there is none.
    let payload = receive(&mut frames, &mut frame, io_channel).await?;
    license::accept(payload)?;

    // 9. The capability exchange, and 10. the four PDUs that make the share live.
    //     Both happen again, unchanged, every time the server rebuilds the desktop.
    let payload = receive(&mut frames, &mut frame, io_channel).await?;
    let Pdu::DemandActive(body) = share::decode(payload)? else {
        bail!("the host did not demand a share once licensing was done");
    };
    let demand = DemandActive::decode(body)?;
    info!(
        "rdp: the host opened a {}x{} desktop, share {:#x}",
        demand.width, demand.height, demand.share_id
    );
    let deferred =
        activate(&mut frames, &mut writer, &mut frame, user, io_channel, &demand).await?;

    Ok(Connected { frames, writer, user, io_channel, channels: numbered, demand, deferred })
}

/// The capability exchange and the handshake after it: everything between a Demand
/// Active and a live share.
///
/// Run once while connecting, and again every time the server tears the desktop down
/// and builds it back — the Deactivation-Reactivation Sequence repeats this whole
/// exchange, which is why it is one function rather than part of the sequence above.
///
/// Fast-path updates that arrive part-way through are read past: the server may start
/// painting a desktop before it has finished agreeing on one, and what is dropped
/// here is asked for again by whoever called this.
///
/// A static virtual channel is not the share's, and nothing on one can be asked for
/// again: a server opens the clipboard as soon as the channel is up, which is before
/// this exchange ends, and a Monitor Ready read past here would be a session whose
/// clipboard never started. So those are handed back rather than dropped, for the
/// caller to act on once it can.
pub(super) async fn activate(
    frames: &mut Frames<ReadHalf<tls::Stream>>,
    writer: &mut WriteHalf<tls::Stream>,
    frame: &mut Vec<u8>,
    user: u16,
    io_channel: u16,
    demand: &DemandActive,
) -> Result<Vec<(u16, Vec<u8>)>> {
    let confirm = ConfirmActive {
        share_id: demand.share_id,
        width: demand.width,
        height: demand.height,
        keyboard_layout: KEYBOARD_LAYOUT,
        multifragment: demand.multifragment,
    }
    .encode();
    let confirm = share::confirm_active(user, &confirm);
    send(writer, user, io_channel, &confirm).await.context("sending the Confirm Active PDU")?;

    // All four go out without waiting; the server's four come back in its own time,
    // with session PDUs among them.
    for request in finalization::requests(user, demand.share_id) {
        let framed = mcs::send_data_request(user, io_channel, &request)?;
        writer.write_all(&framed).await.context("sending a finalization PDU")?;
    }
    let mut deferred = Vec::new();
    loop {
        frames.next(frame).await?;
        if fastpath::is_output(frame[0]) {
            continue;
        }
        let payload = match mcs::send_data_indication(frame)? {
            mcs::Indication::Data(data) if data.channel == io_channel => data.payload,
            mcs::Indication::Data(data) => {
                deferred.push((data.channel, data.payload.to_vec()));
                continue;
            }
            mcs::Indication::Disconnect(reason) => {
                bail!("the host ended the connection before the desktop was ready: {reason}")
            }
        };
        let Pdu::Data(data) = share::decode(payload)? else {
            bail!("the host deactivated the share during connection finalization");
        };
        if finalization::response(&data)? == Response::FontMap {
            return Ok(deferred);
        }
    }
}

/// One PDU out, addressed to a channel.
async fn send(
    writer: &mut WriteHalf<tls::Stream>,
    user: u16,
    channel: u16,
    pdu: &[u8],
) -> Result<()> {
    let framed = mcs::send_data_request(user, channel, pdu)?;
    writer.write_all(&framed).await?;
    Ok(())
}

/// One PDU in, off the channel it was expected on.
async fn receive<'a>(
    frames: &mut Frames<ReadHalf<tls::Stream>>,
    frame: &'a mut Vec<u8>,
    channel: u16,
) -> Result<&'a [u8]> {
    frames.next(frame).await?;
    match mcs::send_data_indication(frame)? {
        mcs::Indication::Data(data) if data.channel == channel => Ok(data.payload),
        mcs::Indication::Data(data) => {
            bail!("the host sent a PDU on channel {}, where {channel} was expected", data.channel)
        }
        mcs::Indication::Disconnect(reason) => {
            bail!("the host ended the connection before the desktop was ready: {reason}")
        }
    }
}

/// One whole TPKT frame off the socket, for the two exchanges that happen before
/// [`Frames`] has anything to buffer.
async fn read_frame(tcp: &mut TcpStream) -> Result<Vec<u8>> {
    let mut header = [0_u8; TPKT_HEADER];
    tcp.read_exact(&mut header).await?;
    let length = x224::frame_length(&header)?;
    let mut frame = vec![0_u8; length];
    frame[..TPKT_HEADER].copy_from_slice(&header);
    tcp.read_exact(&mut frame[TPKT_HEADER..]).await?;
    Ok(frame)
}

/// A desktop dimension as the `u16` the protocol counts in, saturating rather than
/// wrapping: nothing real exceeds RDP's own 8192 a side.
fn narrow(v: u32) -> u16 {
    u16::try_from(v).unwrap_or(u16::MAX)
}
