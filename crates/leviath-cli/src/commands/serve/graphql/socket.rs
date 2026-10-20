//! One `/ws/graphql` connection: the protocol async-graphql speaks, run
//! inside the same keepalive `/ws` has.
//!
//! async-graphql reads and writes protocol messages and nothing else. It sends
//! no WebSocket ping, sets no deadline on a send, and a peer that stopped
//! reading without closing would hold its subscriptions open until TCP gave
//! up. So the socket is not handed to it. This loop owns the socket, and
//! async-graphql talks to it over two channels: client messages in, protocol
//! messages out. The loop answers to the ping cadence, the pong deadline, the
//! send deadline and the server's own shutdown, exactly as `/ws` does.

use async_graphql_axum::{GraphQLProtocol, GraphQLWebSocket};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{FutureExt, SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::PollSender;

use super::super::admin_gate::Presented;
use super::super::keepalive::{Beat, Heartbeat, WsTiming, send_within};
use super::super::shutdown::{Shutdown, going_away};
use super::LeviathSchema;

/// How many protocol messages may wait for the socket before async-graphql
/// itself waits. Small: the send deadline is what bounds a slow peer, and a
/// deep queue would only hide one.
const OUTBOUND: usize = 16;

/// How many client messages may wait for the protocol. The socket is not read
/// while they do, so a client sending faster than the protocol answers is
/// slowed down rather than buffered.
const INBOUND: usize = 16;

/// Serve one subscription socket until the client leaves, stops answering,
/// stops reading, or the server stops.
///
/// When the server begins to stop, the socket is closed with 1001 (going
/// away), so a client can tell a shutdown from a network fault.
pub(super) async fn serve(
    socket: WebSocket,
    schema: LeviathSchema,
    protocol: GraphQLProtocol,
    presented: Presented,
    timing: WsTiming,
    shutdown: Shutdown,
) {
    let (mut sink, mut stream) = socket.split();
    let (inbound, from_client) = mpsc::channel::<Result<Message, axum::Error>>(INBOUND);
    let (to_client, outbound) = mpsc::channel::<Message>(OUTBOUND);
    let mut outbound = ReceiverStream::new(outbound);
    let from_client = ReceiverStream::new(from_client);
    let to_client = PollSender::new(to_client);
    // Fused, so once the protocol has said its last the select below can keep
    // polling it without polling a finished future. Its end drops `to_client`,
    // which is what ends the loop once its last message has gone out.
    let mut connection = async_graphql::Data::default();
    connection.insert(presented);
    let mut graphql = GraphQLWebSocket::new_with_pair(to_client, from_client, schema, protocol)
        .with_data(connection)
        .serve()
        .boxed()
        .fuse();
    let mut heartbeat = Heartbeat::new(timing);
    loop {
        tokio::select! {
            () = &mut graphql => {}
            // Room for the message is reserved before the socket is read, so
            // a read message always has somewhere to go. Both halves are
            // safe to drop part way, which `select!` does to the losers.
            (room, msg) = async { (inbound.reserve().await, stream.next().await) } => match msg {
                Some(Ok(Message::Pong(_))) => heartbeat.answered(),
                Some(Ok(Message::Close(_))) => {
                    // The reply to a close is queued by the read and goes
                    // out on the next flush, which is this one.
                    let _ = tokio::time::timeout(timing.send_timeout, sink.flush()).await;
                    break;
                }
                Some(Err(_)) | None => break,
                Some(Ok(msg)) => {
                    // No room means the protocol has ended, which is the
                    // protocol closing the socket: the outbound half below
                    // sees that.
                    let _ = room.map(|room| room.send(Ok(msg)));
                }
            },
            msg = outbound.next() => match msg {
                Some(msg) => {
                    if !send_within(&mut sink, timing.send_timeout, msg).await {
                        break;
                    }
                }
                None => break,
            },
            () = shutdown.requested() => {
                // Said once and not waited on: the client's own close reply
                // changes nothing, and a peer that cannot take the frame is
                // gone either way.
                let _ = send_within(&mut sink, timing.send_timeout, going_away()).await;
                break;
            }
            beat = heartbeat.due() => match beat {
                Beat::Ping => heartbeat.ping(&mut sink).await,
                Beat::Overdue => break,
            },
        }
    }
}

#[cfg(test)]
#[path = "socket_tests.rs"]
mod tests;
