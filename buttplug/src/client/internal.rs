// Buttplug Rust Source Code File - See https://buttplug.io for more info.
//
// Copyright 2016-2019 Nonpolynomial Labs LLC. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.

//! Implementation of internal Buttplug Client event loop.

use crate::core::messages::{self, ButtplugMessageUnion};
use super::connector::{ButtplugClientConnector,
                       ButtplugClientConnectionStateShared,
                       ButtplugClientConnectorError};
use core::pin::Pin;
use futures::StreamExt;
use futures_util::future::FutureExt;
use async_std::{sync::{Sender, Receiver},
                future::{select, Future},
                task::{Waker, Context, Poll}};
use std::sync::{Arc, Mutex};

/// Struct used for waiting on replies from the server.
///
/// When a ButtplugMessage is sent to the server, it may take an indeterminate
/// amount of time to get a reply. This struct holds the reply, as well as a
/// [Waker] for the related future. Once the reply_msg is filled, the waker will
/// be called to finish the future polling.
#[derive(Debug, Clone)]
pub struct ButtplugClientFutureState<T> {
    reply_msg: Option<T>,
    waker: Option<Waker>,
}

// For some reason, deriving default above doesn't work, but doing an explicit
// derive here does work.
impl<T> Default for ButtplugClientFutureState<T> {
    fn default() -> Self {
        ButtplugClientFutureState::<T> {
            reply_msg: None,
            waker: None,
        }
    }
}

impl<T> ButtplugClientFutureState<T> {
    /// Sets the reply message in a message state struct, firing the waker.
    ///
    /// When a reply is received from (or in the in-process case, generated by)
    /// a server, this function takes the message, updates the state struct, and
    /// calls [Waker::wake] so that the corresponding future can finish.
    ///
    /// # Parameters
    ///
    /// - `msg`: Message to set as reply, which will be returned by the
    /// corresponding future.
    pub fn set_reply_msg(&mut self, msg: &T)
    where T: Clone {
        self.reply_msg = Some(msg.clone());
        let waker = self.waker.take();
        // TODO This should never happen? If it does we'll just lock because we
        // don't have a future to finish?
        if !waker.is_none() {
            waker.unwrap().wake();
        }
    }
}

/// Shared [ButtplugClientConnectionStatus] type.
///
/// [ButtplugClientConnectionStatus] is made to be shared across futures, and we'll
/// never know if those futures are single or multithreaded. Only needs to
/// unlock for calls to [ButtplugClientConnectionStatus::set_reply_msg].
pub type ButtplugClientFutureStateShared<T> = Arc<Mutex<ButtplugClientFutureState<T>>>;

/// [Future] implementation for [ButtplugMessageUnion] types send to the server.
///
/// A [Future] implementation that we can always expect to return a
/// [ButtplugMessageUnion]. Used to deal with getting server replies after
/// sending [ButtplugMessageUnion] types via the client API.
#[derive(Debug)]
pub struct ButtplugClientFuture<T> {
    /// State that holds the waker for the future, and the [ButtplugMessageUnion] reply (once set).
    ///
    /// ## Notes
    ///
    /// This needs to be an [Arc]<[Mutex]<T>> in order to make it mutable under
    /// pinning when dealing with being a future. There is a chance we could do
    /// this as a [Pin::get_unchecked_mut] borrow, which would be way faster, but
    /// that's dicey and hasn't been proven as needed for speed yet.
    waker_state: ButtplugClientFutureStateShared<T>,
}

impl<T> Default for ButtplugClientFuture<T> {
    fn default() -> Self {
        ButtplugClientFuture::<T> {
            waker_state: ButtplugClientFutureStateShared::<T>::default()
        }
    }
}

impl<T> ButtplugClientFuture<T> {

    /// Returns a clone of the state, used for moving the state across contexts
    /// (tasks/threads/etc...).
    pub fn get_state_clone(&self) -> ButtplugClientFutureStateShared<T> {
        self.waker_state.clone()
    }

    // TODO Should we implement drop on this, so it'll yell if its dropping and
    // the waker didn't fire? otherwise it seems like we could have quiet
    // deadlocks.
}

impl<T> Future for ButtplugClientFuture<T> {
    type Output = T;

    /// Returns when the [ButtplugMessageUnion] reply has been set in the
    /// [ButtplugClientConnectionStatusShared].
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut waker_state = self.waker_state.lock().unwrap();
        if waker_state.reply_msg.is_some() {
            let msg = waker_state.reply_msg.take().unwrap();
            Poll::Ready(msg)
        } else {
            debug!("Waker set.");
            waker_state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

pub type ButtplugClientMessageState = ButtplugClientFutureState<ButtplugMessageUnion>;
pub type ButtplugClientMessageStateShared = ButtplugClientFutureStateShared<ButtplugMessageUnion>;
pub type ButtplugClientMessageFuture = ButtplugClientFuture<ButtplugMessageUnion>;

/// The internal event loop for the [ButtplugClient].
///
/// Created whenever a new [ButtplugClient] is created, the internal loop
/// handles connection and communication with the server, and creation of events
/// received from the server. As [ButtplugClient] is clonable, multiple
/// ButtplugClient instances can exist that all communicate with the same
/// [ButtplugClientInternalLoop].
///
/// Also, if multiple [ButtplugClient] instances are created via new(), multiple
/// [ButtplugClientInternalLoop]s can run in parallel. This allows applications
/// to possibly create connections to multiple [ButtplugServer] instances.
pub struct ButtplugClientInternalLoop {
    /// Connector struct, which handles communication with the server
    connector: Option<Box<dyn ButtplugClientConnector>>,
    /// Receiver for data from clients
    client_receiver: Receiver<ButtplugInternalClientMessage>,
    /// Sender for communicating events to [ButtplugClient]s
    ///
    // When the event_sender is dropped, all clients will only receive None
    // during wait_for_event calls, so we can assume disconnection at that
    // point.
    event_sender: Sender<ButtplugMessageUnion>,
}

/// Make ButtplugClientInternalLoop sendable across threads.
///
/// Since this loop will usually be run in a future somewhere, and we don't know
/// where, this needs to be send.
unsafe impl Send for ButtplugClientInternalLoop {}
unsafe impl Sync for ButtplugClientInternalLoop {}

/// Enum used for communication between the client and the internal loop.
pub enum ButtplugInternalClientMessage {
    /// Client request to connect, via the included connector instance.
    ///
    /// Once connection is finished, use the bundled future to resolve.
    Connect(Box<dyn ButtplugClientConnector>, ButtplugClientConnectionStateShared),
    /// Client request to disconnect, via already sent connector instance.
    Disconnect,
    /// Client request to send a message via the connector.
    ///
    /// Bundled future should have reply set and waker called when this is
    /// finished.
    Message((ButtplugMessageUnion, ButtplugClientMessageStateShared)),
}

enum StreamReturn {
    ConnectorMessage(ButtplugMessageUnion),
    ClientMessage(ButtplugInternalClientMessage),
    Disconnect,
}

impl ButtplugClientInternalLoop {
    /// Returns a new ButtplugClientInternalLoop instance.
    ///
    /// This should really only ever be constructed by a [ButtplugClient], and
    /// there should only be one per new [ButtplugClient].
    ///
    /// # Parameters
    ///
    /// - `event_sender`: Used when sending server updates to clients.
    /// - `client_receiver`: Used when receiving commands from clients to
    /// send to server.
    pub fn new(event_sender: Sender<ButtplugMessageUnion>,
               client_receiver: Receiver<ButtplugInternalClientMessage>) -> Self {
        ButtplugClientInternalLoop {
            connector: None,
            client_receiver,
            event_sender,
        }
    }

    async fn wait_for_connector(&mut self) -> Option<Receiver<ButtplugMessageUnion>> {
        match self.client_receiver.next().await {
            None => {
                debug!("Client disconnected.");
                None
            },
            Some(msg) => {
                match msg {
                    ButtplugInternalClientMessage::Connect(mut connector, state) => {
                        match connector.connect().await {
                            Some(_s) => {
                                error!("Cannot connect to server.");
                                let mut waker_state = state.lock().unwrap();
                                let reply = Some(ButtplugClientConnectorError::new("Cannot connect to server."));
                                waker_state.set_reply_msg(&reply);
                                None
                            },
                            None => {
                                info!("Connected!");
                                let mut waker_state = state.lock().unwrap();
                                waker_state.set_reply_msg(&None);
                                let recv = connector.get_event_receiver();
                                self.connector = Option::Some(connector);
                                return Some(recv);
                            }
                        }
                    },
                    _ => {
                        error!("Received non-connector message before connector message.");
                        None
                    }
                }
            }
        }
    }

    /// The internal event loop for [ButtplugClient] connection and
    /// communication
    ///
    /// The event_loop does a few different things during its lifetime.
    ///
    /// - The first thing it will do is wait for a Connect message from a
    /// client. This message contains a [ButtplugClientConnector] that will be
    /// used to connect and communicate with a [ButtplugServer].
    ///
    /// - After a connection is established, it will listen for events from the
    /// connector, or messages from the client, until either server/client
    /// disconnects.
    ///
    /// - Finally, on disconnect, it will tear down, and cannot be used again.
    /// All clients and devices associated with the loop will be invalidated,
    /// and a new [ButtplugClient] (and corresponding
    /// [ButtplugClientInternalLoop]) must be created.
    pub async fn event_loop(&mut self) {
        info!("Starting client event loop.");
        // Wait for the connect message, then only continue on successful
        // connection.
        let mut connector_receiver;
        match self.wait_for_connector().await {
            None => return,
            Some(recv) => connector_receiver = recv,
        }
        // Once connected, wait for messages from either the client or the
        // connector, and send them the direction they're supposed to go.
        loop {
            let client_future = self.client_receiver
                .next()
                .map(|x| {
                    match x {
                        None => {
                            debug!("Client disconnected.");
                            StreamReturn::Disconnect
                        },
                        Some(msg) => StreamReturn::ClientMessage(msg)
                    }
                });
            let event_future = connector_receiver
                .next()
                .map(|x| {
                    match x {
                        None => {
                            debug!("Connector disconnected.");
                            StreamReturn::Disconnect
                        },
                        Some(msg) => StreamReturn::ConnectorMessage(msg)
                    }
                });
            let stream_ret = select!(event_future, client_future).await;
            match stream_ret {
                StreamReturn::ConnectorMessage(_msg) => {
                    info!("Sending message to clients.");
                    self.event_sender.send(_msg.clone()).await;
                },
                StreamReturn::ClientMessage(_msg) => {
                    debug!("Parsing a client message.");
                    match _msg {
                        ButtplugInternalClientMessage::Message(_msg_fut) => {
                            debug!("Sending message through connector.");
                            if let Some(ref mut connector) = self.connector {
                                connector.send(&_msg_fut.0, &_msg_fut.1).await;
                            }
                        },
                        ButtplugInternalClientMessage::Disconnect => {
                            info!("Client requested disconnect");
                            break;
                        },
                        // TODO Do something other than panic if someone does
                        // something like trying to connect twice..
                        _ => panic!("Message not handled!")
                    }
                },
                StreamReturn::Disconnect => {
                    info!("Disconnected!");
                    break;
                }
            }
        }
        info!("Exiting client event loop");
    }
}
