// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Code to spin up communication mesh for a Timely cluster.
//!
//! The heart of this code is `create_sockets`, which establishes connections with the other
//! processes ("peers") in the Timely cluster. This process needs to be fault-tolerant: If one or
//! multiple processes restart while connections are established, this must not leave the cluster
//! in a stalled state where the processes cannot finish setting up connections for whatever
//! reason.
//!
//! A Timely cluster assumes reliable networking among all processes in the cluster and forces its
//! processes to crash if this condition is not met. It is therefore impossible to set up a working
//! Timely cluster in the presence of persistent process or network failures. However, we assume
//! that any period of instability eventually resolves. `create_sockets` is written to ensure
//! that once things are stable again, processes can correctly establish connections among each
//! other.
//!
//! If a process returns from `create_sockets` with one or more sockets that are connected to
//! processes that have crashed, that process will also crash upon initializing its side of the
//! Timely cluster. We can say that processes connected to crashed processes are "doomed".
//! Additionally, all processes connected to doomed processes are doomed themselves, as their
//! doomed peer will eventually crash, forcing them to crash too. We need to avoid a circle of doom
//! where new processes perpetually connect to doomed processes, become doomed themselves, doom
//! other processes that connect to them, and then crash.
//!
//! `create_sockets` avoids the circle of doom by ensuring that a new generation of processes
//! does not connect to the previous generation. We pessimistically assume that the entire previous
//! generation has been doomed and to successfully connect we need to spin up an entire new
//! generation. This approach can cause us to force restarts of non-doomed processes and therefore
//! leaves some efficiency on the floor, but we are more concerned about our ability to reason
//! about the system than about cluster startup time.
//!
//! To differentiate between generations, we rely on an epoch, i.e., a number that increases
//! between process restarts. Unfortunately, we don't have a way to get a perfect epoch here, so we
//! use the system time instead. Note that the system time is not guaranteed to always increase,
//! but as long as it increases eventually, we will eventually succeed in establishing connections
//! between processes.
//!
//! Each process performs the following protocol:
//!
//!  * Let `my_index` be the index of the processes in the Timely cluster.
//!  * If `my_index` == 0, mint a new `my_epoch`. Otherwise leave `my_epoch` uninitialized.
//!  * For each `index` < `my_index`:
//!    * Connect to the peer at `index`.
//!    * Receive `peer_epoch`.
//!    * If `my_epoch` is unitialized, set `my_epoch` to `peer_epoch`.
//!    * Send `my_epoch`.
//!    * Compare epochs:
//!      * `my_epoch` < `peer_epoch`: fail the protocol
//!      * `my_epoch` > `peer_epoch`: retry the connection
//!      * `my_epoch` == `peer_epoch`: connection successfully established
//!  * Until a connections has been established with all peers:
//!    * Accept a connection from a peer at `index` > `my_index`.
//!    * If a connection to a peer at `index` was already established, fail the protocol.
//!    * Send `my_epoch`.
//!    * Receive `peer_epoch`.
//!    * Compare epochs and react, as above.
//!
//! Process 0 acts as the leader of a generation. When a process connects to process 0 and learns
//! its epoch, it becomes part of that generation and cannot connect to processes of other
//! generations anymore. When a process crashes that was previously part of a generation, it dooms
//! that generation. When it restarts, it can't connect to the same generation anymore because
//! process 0 won't accept the connection. What's more, in attempting to connect to the processes
//! of the doomed generation, the new process forces them to fail the protocol and rejoin as part
//! of the new generation, ensuring we don't get stuck with multiple processes on different
//! generations indefinitely.

use std::any::Any;
use std::cmp::Ordering;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context};
use futures::TryFutureExt;
use mz_ore::cast::CastFrom;
use mz_ore::netio::{Listener, Stream};
use timely::communication::allocator::zero_copy::allocator::TcpBuilder;
use timely::communication::allocator::zero_copy::bytes_slab::BytesRefill;
use mz_ore::retry::Retry;
use regex::Regex;
use timely::communication::allocator::zero_copy::initialize::initialize_networking_from_sockets;
use timely::communication::allocator::{GenericBuilder, PeerBuilder};
use tracing::{info, warn};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Creates communication mesh from cluster config
pub async fn initialize_networking<P>(
    workers: usize,
    process: usize,
    addresses: Vec<String>,
    refill: BytesRefill,
    builder_fn: impl Fn(TcpBuilder<P::Peer>) -> GenericBuilder,
) -> Result<(Vec<GenericBuilder>, Box<dyn Any + Send>), anyhow::Error>
where
    P: PeerBuilder,
{
    info!(
        process,
        ?addresses,
        "initializing network for timely instance"
    );

    let sockets = loop {
        match create_sockets(process, &addresses).await {
            Ok(sockets) => break sockets,
            Err(error) if error.is_fatal() => bail!("failed to set up Timely sockets: {error}"),
            Err(error) => info!("creating sockets failed: {error}; retrying"),
        }
    };

    if sockets
        .iter()
        .filter_map(|s| s.as_ref())
        .all(|s| s.is_tcp())
    {
        let sockets = sockets
            .into_iter()
            .map(|s| s.map(|s| s.unwrap_tcp().into_std()).transpose())
            .collect::<Result<Vec<_>, _>>()
            .map_err(anyhow::Error::from)
            .context("failed to get standard sockets from tokio sockets")?;
        initialize_networking_inner::<_, P, _>(sockets, process, workers, refill, builder_fn)
    } else if sockets
        .iter()
        .filter_map(|s| s.as_ref())
        .all(|s| s.is_unix())
    {
        let sockets = sockets
            .into_iter()
            .map(|s| s.map(|s| s.unwrap_unix().into_std()).transpose())
            .collect::<Result<Vec<_>, _>>()
            .map_err(anyhow::Error::from)
            .context("failed to get standard sockets from tokio sockets")?;
        initialize_networking_inner::<_, P, _>(sockets, process, workers, refill, builder_fn)
    } else {
        anyhow::bail!("cannot mix TCP and Unix streams");
    }
}

fn initialize_networking_inner<S, P, PF>(
    sockets: Vec<Option<S>>,
    process: usize,
    workers: usize,
    refill: BytesRefill,
    builder_fn: PF,
) -> Result<(Vec<GenericBuilder>, Box<dyn Any + Send>), anyhow::Error>
where
    S: timely::communication::allocator::zero_copy::stream::Stream + 'static,
    P: PeerBuilder,
    PF: Fn(TcpBuilder<P::Peer>) -> GenericBuilder,
{
    for s in &sockets {
        if let Some(s) = s {
            s.set_nonblocking(false)
                .context("failed to set socket to non-blocking")?;
        }
    }

    match initialize_networking_from_sockets::<_, P>(
        sockets,
        process,
        workers,
        refill,
        Arc::new(|_| None),
    ) {
        Ok((stuff, guard)) => {
            info!(process = process, "successfully initialized network");
            Ok((stuff.into_iter().map(builder_fn).collect(), Box::new(guard)))
        }
        Err(err) => {
            warn!(process, "failed to initialize network: {err}");
            Err(anyhow::Error::from(err).context("failed to initialize networking from sockets"))
        }
    }
}

/// Errors returned by `create_sockets`.
#[derive(Debug)]
enum CreateSocketsError {
    Bind {
        address: String,
        error: std::io::Error,
    },
    EpochMismatch {
        peer_index: usize,
        peer_epoch: Epoch,
        my_epoch: Epoch,
    },
    Reconnect {
        peer_index: usize,
    },
}

impl CreateSocketsError {
    /// Whether the error isn't expected to resolve on a retry.
    fn is_fatal(&self) -> bool {
        matches!(self, Self::Bind { .. })
    }
}

impl fmt::Display for CreateSocketsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bind { address, error } => write!(f, "failed to bind at {address}: {error}"),
            Self::EpochMismatch {
                peer_index,
                peer_epoch,
                my_epoch,
            } => write!(
                f,
                "peer {peer_index} has greater epoch: {peer_epoch} > {my_epoch}"
            ),
            Self::Reconnect { peer_index } => {
                write!(f, "observed second instance of peer {peer_index}")
            }
        }
    }
}

impl std::error::Error for CreateSocketsError {}

/// Epoch type used in the `create_sockets` protocol.
///
/// Epochs are derived from the system time and therefore not guaranteed to be strictly
/// increasing. For `create_sockets` it is sufficient for it to eventually increase.
///
/// Epoch values also include a random component, to ensure two values produced by different calls
/// to `Epoch::mint` never compare as equal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Epoch {
    time: u64,
    nonce: u64,
}

impl Epoch {
    fn mint() -> Self {
        let time = SystemTime::UNIX_EPOCH
            .elapsed()
            .expect("current time is after 1970")
            .as_millis()
            .try_into()
            .expect("fits");
        let nonce = rand::random();
        Self { time, nonce }
    }

    async fn read(s: &mut Stream) -> std::io::Result<Self> {
        let time = s.read_u64().await?;
        let nonce = s.read_u64().await?;
        Ok(Self { time, nonce })
    }

    async fn write(&self, s: &mut Stream) -> std::io::Result<()> {
        s.write_u64(self.time).await?;
        s.write_u64(self.nonce).await?;
        Ok(())
    }
}

impl fmt::Display for Epoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}, {})", self.time, self.nonce)
    }
}

/// Creates socket connections from a list of host addresses.
///
/// The item at index i in the resulting vec, is a connection to process i, except for item
/// `my_index` which is None (no socket to self).
async fn create_sockets(
    my_index: usize,
    addresses: &[String],
) -> Result<Vec<Option<Stream>>, CreateSocketsError> {
    let my_address = &addresses[my_index];

    // Binding to a TCP address of the form `hostname:port` unnecessarily involves a DNS query. We
    // should get the port from here, but otherwise just bind to `0.0.0.0`.
    let port_re = Regex::new(r":(\d{1,5})$").unwrap();
    let listen_address = match port_re.captures(my_address) {
        Some(cap) => format!("0.0.0.0:{}", &cap[1]),
        None => my_address.to_string(),
    };

    let listener = Retry::default()
        .initial_backoff(Duration::from_secs(1))
        .clamp_backoff(Duration::from_secs(1))
        .max_tries(10)
        .retry_async(|_| {
            Listener::bind(&listen_address)
                .inspect_err(|error| warn!(%listen_address, "failed to listen: {error}"))
        })
        .await
        .map_err(|error| CreateSocketsError::Bind {
            address: listen_address,
            error,
        })?;

    let (my_epoch, sockets_lower) = match my_index {
        0 => {
            let epoch = Epoch::mint();
            info!(my_index, "minted epoch: {epoch}");
            (epoch, Vec::new())
        }
        _ => connect_lower(my_index, addresses).await?,
    };

    let n_peers = addresses.len();
    let sockets_higher = accept_higher(my_index, my_epoch, n_peers, &listener).await?;

    let connections_lower = sockets_lower.into_iter().map(Some);
    let connections_higher = sockets_higher.into_iter().map(Some);
    let connections = connections_lower
        .chain([None])
        .chain(connections_higher)
        .collect();

    Ok(connections)
}

/// Connect to peers with indexes less than `my_index`.
///
/// Returns a list of connections ordered by peer index, as well as the epoch of the current
/// generation on success, or an error if the protocol has failed and must be restarted.
async fn connect_lower(
    my_index: usize,
    addresses: &[String],
) -> Result<(Epoch, Vec<Stream>), CreateSocketsError> {
    assert!(my_index > 0);
    assert!(my_index <= addresses.len());

    async fn handshake(
        my_index: usize,
        my_epoch: Option<Epoch>,
        address: &str,
    ) -> anyhow::Result<(Epoch, Stream)> {
        let mut s = Stream::connect(address).await?;
        if let Stream::Tcp(tcp) = &s {
            tcp.set_nodelay(true)?;
        }

        s.write_u64(u64::cast_from(my_index)).await?;
        let peer_epoch = Epoch::read(&mut s).await?;
        let my_epoch = my_epoch.unwrap_or(peer_epoch);
        my_epoch.write(&mut s).await?;

        Ok((peer_epoch, s))
    }

    let mut my_epoch = None;
    let mut sockets = Vec::new();

    while sockets.len() < my_index {
        let index = sockets.len();
        let address = &addresses[index];

        info!(my_index, "connecting to peer {index} at address: {address}");

        let (peer_epoch, sock) = Retry::default()
            .initial_backoff(Duration::from_secs(1))
            .clamp_backoff(Duration::from_secs(1))
            .retry_async(|_| {
                handshake(my_index, my_epoch, address).inspect_err(|error| {
                    info!(my_index, "error connecting to peer {index}: {error}")
                })
            })
            .await
            .expect("retries forever");

        if let Some(epoch) = my_epoch {
            match peer_epoch.cmp(&epoch) {
                Ordering::Less => {
                    info!(
                        my_index,
                        "refusing connection to peer {index} with smaller epoch: \
                         {peer_epoch} < {epoch}",
                    );
                    continue;
                }
                Ordering::Greater => {
                    return Err(CreateSocketsError::EpochMismatch {
                        peer_index: index,
                        peer_epoch,
                        my_epoch: epoch,
                    });
                }
                Ordering::Equal => info!(my_index, "connected to peer {index}"),
            }
        } else {
            info!(my_index, "received epoch from peer {index}: {peer_epoch}");
            my_epoch = Some(peer_epoch);
        }

        sockets.push(sock);
    }

    let my_epoch = my_epoch.expect("must exist");
    Ok((my_epoch, sockets))
}

/// Accept connections from peers with indexes greater than `my_index`.
///
/// Returns a list of connections ordered by peer index, starting with `my_index` + 1,
/// or an error if the protocol has failed and must be restarted.
async fn accept_higher(
    my_index: usize,
    my_epoch: Epoch,
    n_peers: usize,
    listener: &Listener,
) -> Result<Vec<Stream>, CreateSocketsError> {
    assert!(my_index < n_peers);

    async fn accept(listener: &Listener) -> anyhow::Result<(usize, Stream)> {
        let (mut s, _) = listener.accept().await?;
        if let Stream::Tcp(tcp) = &s {
            tcp.set_nodelay(true)?;
        }

        let peer_index = s.read_u64().await?;
        let peer_index = usize::cast_from(peer_index);
        Ok((peer_index, s))
    }

    async fn exchange_epochs(my_epoch: Epoch, s: &mut Stream) -> anyhow::Result<Epoch> {
        my_epoch.write(s).await?;
        let peer_epoch = Epoch::read(s).await?;
        Ok(peer_epoch)
    }

    let offset = my_index + 1;
    let mut sockets: Vec<_> = (offset..n_peers).map(|_| None).collect();

    while sockets.iter().any(|s| s.is_none()) {
        info!(my_index, "accepting connection from peer");

        let (index, mut sock) = match accept(listener).await {
            Ok(result) => result,
            Err(error) => {
                info!(my_index, "error accepting connection: {error}");
                continue;
            }
        };

        if sockets[index - offset].is_some() {
            return Err(CreateSocketsError::Reconnect { peer_index: index });
        }

        let peer_epoch = match exchange_epochs(my_epoch, &mut sock).await {
            Ok(result) => result,
            Err(error) => {
                info!(my_index, "error exchanging epochs: {error}");
                continue;
            }
        };

        match peer_epoch.cmp(&my_epoch) {
            Ordering::Less => {
                info!(
                    my_index,
                    "refusing connection from peer {index} with smaller epoch: \
                     {peer_epoch} < {my_epoch}",
                );
                continue;
            }
            Ordering::Greater => {
                return Err(CreateSocketsError::EpochMismatch {
                    peer_index: index,
                    peer_epoch,
                    my_epoch,
                });
            }
            Ordering::Equal => info!(my_index, "connected to peer {index}"),
        }

        sockets[index - offset] = Some(sock);
    }

    Ok(sockets.into_iter().map(|s| s.unwrap()).collect())
}
