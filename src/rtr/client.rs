//! The RTR client.
//!
//! This module implements a generic RTR client through [`Client`]. In order
//! to use the client, you will need to provide a type that implements
//! [`VrpTarget`] as well as one that implements [`VrpUpdate`]. The former
//! represents the place where all the information received via the RTR client
//! is stored, while the latter receives a set of updates and applies it to
//! the target.
//!
//! For more information on how to use the client, see the [`Client`] type.
//!
//! [`Client`]: struct.Client.html
//! [`VrpTarget`]: trait VrpTarget.html
//! [`VrpUpdate`]: trait.VrpUpdate.html
use std::io;
use std::future::Future;
use std::marker::Unpin;
use tokio::time::{timeout, timeout_at, Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use super::payload::{Action, Payload, Timing};
use super::pdu;
use super::state::State;


//------------ Configuration Constants ---------------------------------------

const IO_TIMEOUT: Duration = Duration::from_secs(1);


//------------ VrpTarget -----------------------------------------------------

/// A type that keeps data received via RTR.
///
/// The data of the target consisting of a set of items called VRPs for
/// Validated RPKI Payload. It is modified by atomic updates that add
/// or remove items of the set.
///
/// This trait provides a method to start and apply updates which are
/// collected into a different type that implements the companion
/// [`VrpUpdate`] trait.
///
/// [`VrpUpdate`]: trait.VrpUpdate.html
pub trait VrpTarget {
    /// The type of a single update.
    type Update: VrpUpdate;

    /// Starts a new update.
    ///
    /// If the update is a for a reset query, `reset` will be `true`, meaning
    /// that when the update is applied, all previous data should be removed.
    /// This flag is repeated later in `apply`, leaving it to implementations
    /// whether to store updates differently for reset and serial queries.
    fn start(&mut self, reset: bool) -> Self::Update;

    /// Applies an update to the target.
    ///
    /// The data to apply is handed over via `update`. If `reset` is `true`,
    /// the data should replace the current data of the target. Otherwise it
    /// entries should be added and removed according to the action. The
    /// `timing` parameter contains the timing information provided by the
    /// server.
    fn apply(
        &mut self, update: Self::Update, reset: bool, timing: Timing
    ) -> Result<(), VrpError>;
}


//------------ VrpUpdate -----------------------------------------------------

/// A type that can receive a VRP data update.
///
/// The update happens by repeatedly calling the [`push_vrp`] method with a
/// single update as received by the client. The data is not filtered. It
/// may contain duplicates and it may conflict with the current data set.
/// It is the task of the implementor to deal with such situations.
///
/// A value of this type is created via `VrpTarget::start` when the client
/// starts processing an update. If the update succeeds, the value is applied
/// to the target by giving it to `VrpTarget::apply`. If the update fails at
/// any point, the valus is simply dropped.
///
/// [`push_vrp`]: #method.push_vrp
/// [`VrpTarget::start`]: trait.VrpTarget.html#method.start
/// [`VrpTarget::apply`]: trait.VrpTarget.html#method.apply
pub trait VrpUpdate {
    /// Updates one single VRP.
    ///
    /// The `action` argument describes whether the VRP is to be announced,
    /// i.e., added to the data set, or withdrawn, i.e., removed. The VRP
    /// itself is given via `payload`.
    fn push_vrp(
        &mut self, action: Action, payload: Payload
    ) -> Result<(), VrpError>;
}

impl VrpUpdate for Vec<(Action, Payload)> {
    fn push_vrp(
        &mut self, action: Action, payload: Payload
    ) -> Result<(), VrpError> {
        self.push((action, payload));
        Ok(())
    }
}


//------------ Client --------------------------------------------------------

/// An RTR client.
///
/// The client wraps a socket – represented by the type argument `Sock` which
/// needs to support Tokio’s asynchronous writing and reading – and runs an
/// RTR client over it. All data received will be passed on a [`VrpTarget`]
/// of type `Target`.
///
/// The client keeps the socket open until either the server closes the
/// connection, an error happens, or the client  is dropped. It will
/// periodically push a new dataset to the target.
pub struct Client<Sock, Target> {
    /// The socket to communicate over.
    sock: Sock,

    /// The target for the VRP set.
    target: Target,

    /// The current synchronisation state.
    ///
    /// The first element is the session ID, the second is the serial. If
    /// this is `None`, we do a reset query next.
    state: Option<State>,

    /// The RRDP version to use.
    ///
    /// If this is `None` we haven’t spoken with the server yet. In this
    /// case, we use 1 and accept any version from the server. Otherwise 
    /// send this version and receving a differing version from the server
    /// is an error as it is not allowed to change its mind halfway.
    version: Option<u8>,

    /// The timing parameters reported by the server.
    ///
    /// We use the `refresh` value to determine how long to wait before
    /// requesting an update. The other values we just report to the target.
    timing: Timing,

    /// The next time we should be running.
    ///
    /// If this is None, we should be running now.
    next_update: Option<Instant>,
}

impl<Sock, Target> Client<Sock, Target> {
    /// Creates a new client.
    ///
    /// The client will use `sock` for communicating with the server and
    /// `target` to send updates to.
    ///
    /// If the last state of a connection with this server is known – it can
    /// be determined by calling [`state`] on the client – it can be reused
    /// via the `state` argument. Make sure to also have the matching data in
    /// your target in this case since the there will not necessarily be a
    /// reset update. If you don’t have any state or don’t want to reuse an
    /// earlier session, simply pass `None`.
    ///
    /// [`state`]: #method.state
    pub fn new(
        sock: Sock,
        target: Target,
        state: Option<State>
    ) -> Self {
        Client {
            sock, target, state,
            version: None,
            timing: Timing::default(),
            next_update: None,
        }
    }

    /// Returns a reference to the target.
    pub fn target(&self) -> &Target {
        &self.target
    }

    /// Returns a mutable reference to the target.
    pub fn target_mut(&mut self) -> &mut Target {
        &mut self.target
    }

    /// Converts the client into its target.
    pub fn into_target(self) -> Target {
        self.target
    }

    /// Returns the current state of the session.
    ///
    /// The method will return `None` if there hasn’t been initial state and
    /// there has not been any converstation with the server yet.
    pub fn state(&self) -> Option<State> {
        self.state
    }

    /// Returns the protocol version to use.
    fn version(&self) -> u8 {
        self.version.unwrap_or(1)
    }
}

impl<Sock, Target> Client<Sock, Target>
where
    Sock: AsyncRead + AsyncWrite + Unpin,
    Target: VrpTarget
{
    /// Runs the client.
    ///
    /// The method will keep the client asynchronously running, fetching any
    /// new data that becomes available on the server and pushing it to the
    /// target until either the server closes the connection – in which case
    /// the method will return `Ok(())` –, an error happens – which will be
    /// returned or the future gets dropped.
    pub async fn run(&mut self) -> Result<(), io::Error> {
        match self._run().await {
            Ok(()) => Ok(()),
            Err(err) => {
                if err.kind() == io::ErrorKind::UnexpectedEof {
                    Ok(())
                }
                else {
                    Err(err)
                }
            }
        }
    }

    /// Internal version of run.
    ///
    /// This is only here to make error handling easier.
    async fn _run(&mut self) -> Result<(), io::Error> {
        // End of loop via an io::Error.
        loop {
            match self.state {
                Some(state) => {
                    match self.serial(state).await? {
                        Some(update) => {
                            self.apply(update, false).await?;
                        }
                        None => continue,
                    }
                }
                None => {
                    let update = self.reset().await?;
                    self.apply(update, true).await?;
                }
            }

            if let Ok(Err(err)) = timeout(
                Duration::from_secs(u64::from(self.timing.refresh)),
                pdu::SerialNotify::read(&mut self.sock)
            ).await {
                return Err(err)
            }
        }
    }

    /// Performs a single update of the client data.
    ///
    /// The method will wait until the next update is due and the request one
    /// single update from the server. It will request a new update object
    /// from the target, apply the update to that object and, if the update
    /// succeeds, return the object.
    pub async fn update(
        &mut self
    ) -> Result<Target::Update, io::Error> {
        if let Some(instant) = self.next_update.take() {
            if let Ok(Err(err)) = timeout_at(
                instant, pdu::SerialNotify::read(&mut self.sock)
            ).await {
                return Err(err)
            }
        }

        if let Some(state) = self.state {
            if let Some(update) = self.serial(state).await? {
                self.next_update = Some(
                    Instant::now() + self.timing.refresh_duration()
                );
                return Ok(update)
            }
        }
        let res = self.reset().await;
        self.next_update = Some(
            Instant::now() + self.timing.refresh_duration()
        );
        res
    }


    /// Perform a serial query.
    ///
    /// Returns some update if the query succeeded and the client should now
    /// wait for a while. Returns `None` if the server reported a restart and
    /// we need to proceed with a reset query. Returns an error
    /// in any other case.
    async fn serial(
        &mut self, state: State
    ) -> Result<Option<Target::Update>, io::Error> {
        pdu::SerialQuery::new(
            self.version(), state,
        ).write(&mut self.sock).await?;
        let start = match self.try_io(FirstReply::read).await? {
            FirstReply::Response(start) => start,
            FirstReply::Reset(_) => {
                self.state = None;
                return Ok(None)
            }
        };
        self.check_version(start.version())?;

        let mut target = self.target.start(false);
        loop {
            match pdu::Payload::read(&mut self.sock).await? {
                Ok(Some(pdu)) => {
                    self.check_version(pdu.version())?;
                    let (action, payload) = pdu.to_payload();
                    if let Err(err) = target.push_vrp(action, payload) {
                        err.send(
                            self.version(), Some(pdu), &mut self.sock
                        ).await?;
                        return Err(io::Error::new(io::ErrorKind::Other, ""));
                    }
                }
                Ok(None) => {
                    // Unsupported but legal payload: ignore.
                }
                Err(end) => {
                    self.check_version(end.version())?;
                    self.state = Some(end.state());
                    if let Some(timing) = end.timing() {
                        self.timing = timing
                    }
                    break;
                }
            }
        }
        Ok(Some(target))
    }

    /// Performs a reset query.
    pub async fn reset(&mut self) -> Result<Target::Update, io::Error> {
        pdu::ResetQuery::new(
            self.version()
        ).write(&mut self.sock).await?;
        let start = self.try_io(|sock| {
            pdu::CacheResponse::read(sock)
        }).await?;
        self.check_version(start.version())?;
        let mut target = self.target.start(true);
        loop {
            match pdu::Payload::read(&mut self.sock).await? {
                Ok(Some(pdu)) => {
                    self.check_version(pdu.version())?;
                    let (action, payload) = pdu.to_payload();
                    if let Err(err) = target.push_vrp(action, payload) {
                        err.send(
                            self.version(), Some(pdu), &mut self.sock
                        ).await?;
                        return Err(io::Error::new(io::ErrorKind::Other, ""));
                    }
                }
                Ok(None) => {
                    // Unsupported but legal payload: ignore.
                }
                Err(end) => {
                    self.check_version(end.version())?;
                    self.state = Some(end.state());
                    if let Some(timing) = end.timing() {
                        self.timing = timing
                    }
                    break;
                }
            }
        }
        Ok(target)
    }

    /// Tries to apply an update and sends errors if that fails.
    async fn apply(
        &mut self, update: Target::Update, reset: bool
    ) -> Result<(), io::Error> {
        if let Err(err) = self.target.apply(update, reset, self.timing) {
            err.send(self.version(), None, &mut self.sock).await?;
            Err(io::Error::new(io::ErrorKind::Other, ""))
        }
        else {
            Ok(())
        }
    }

    /// Performs some IO operation on the socket.
    ///
    /// The mutable reference to the socket is passed to the closure provided
    /// which does the actual IO. The closure is given `IO_TIMEOUT` to finsih
    /// whatever it is doing. Otherwise it is cancelled and a timeout error
    /// is returned.
    async fn try_io<'a, F, Fut, T>(
        &'a mut self, op: F
    ) -> Result<T, io::Error>
    where
        F: FnOnce(&'a mut Sock) -> Fut,
        Fut: Future<Output = Result<T, io::Error>> + 'a
    {
        match timeout(IO_TIMEOUT, op(&mut self.sock)).await {
            Ok(res) => res,
            Err(_) => {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "server response timed out"
                ))
            }
        }
    }

    /// Checks whether `version` matches the stored version.
    ///
    /// Returns an error if it doesn’t.
    fn check_version(&mut self, version: u8) -> Result<(), io::Error> {
        if let Some(stored_version) = self.version {
            if version != stored_version {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "version has changed"
                ))
            }
            else {
                Ok(())
            }
        }
        else {
            self.version = Some(version);
            Ok(())
        }
    }
}


//------------ FirstReply ----------------------------------------------------

/// The first reply from a server in response to a serial query.
enum FirstReply {
    /// A cache response. Actual data is to follow.
    Response(pdu::CacheResponse),

    /// A reset response. We need to retry with a reset query.
    Reset(pdu::CacheReset),
}

impl FirstReply {
    /// Reads the first reply from a socket.
    ///
    /// If any other reply than a cache response or reset response is
    /// received or anything else goes wrong, returns an error.
    async fn read<Sock: AsyncRead + Unpin>(
        sock: &mut Sock
    ) -> Result<Self, io::Error> {
        let header = pdu::Header::read(sock).await?;
        match header.pdu() {
            pdu::CacheResponse::PDU => {
                pdu::CacheResponse::read_payload(
                    header, sock
                ).await.map(FirstReply::Response)
            }
            pdu::CacheReset::PDU => {
                pdu::CacheReset::read_payload(
                    header, sock
                ).await.map(FirstReply::Reset)
            }
            pdu::Error::PDU => {
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("server reported error {}", header.session())
                ))
            }
            pdu => {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unexpected PDU {}", pdu)
                ))
            }
        }
    }
}


//------------ VrpError ------------------------------------------------------

/// A received VRP was not acceptable.
#[derive(Clone, Copy, Debug)]
pub enum VrpError {
    /// A nonexisting record was withdrawn.
    UnknownWithdraw,

    /// An existing record was announced again.
    DuplicateAnnounce,

    /// The record is corrupt.
    Corrupt,

    /// An internal error in the receiver happend.
    Internal,
}

impl VrpError {
    fn error_code(self) -> u16 {
        match self {
            VrpError::UnknownWithdraw => 6,
            VrpError::DuplicateAnnounce => 7,
            VrpError::Corrupt => 0,
            VrpError::Internal => 1
        }
    }

    async fn send(
        self, version: u8, pdu: Option<pdu::Payload>,
        sock: &mut (impl AsyncWrite + Unpin)
    ) -> Result<(), io::Error> {
        match pdu {
            Some(pdu::Payload::V4(pdu)) => {
                pdu::Error::new(
                    version, self.error_code(), pdu, ""
                ).write(sock).await
            }
            Some(pdu::Payload::V6(pdu)) => {
                pdu::Error::new(
                    version, self.error_code(), pdu, ""
                ).write(sock).await
            }
            None => {
                pdu::Error::new(
                    version, self.error_code(), "", ""
                ).write(sock).await
            }
        }
    }
}

