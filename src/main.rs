#![recursion_limit="1024"]

use std::collections::hash_map::{self, HashMap};
use std::sync::Arc;
use std::pin::Pin;
use std::ops::{Deref, DerefMut};
use parking_lot::{RwLock, RwLockUpgradableReadGuard};
use futures::future::{self, FutureExt, Fuse};
use futures::{stream::{self, StreamExt}, sink::{Sink, SinkExt}};
use futures::channel::{mpsc, oneshot};
use async_std::prelude::*;
use async_std::task;
use async_std::net::{TcpListener, TcpStream};
use async_tungstenite::{tungstenite, WebSocketStream};

mod message;
mod channel;
use message::IntoWebsocketMessage;

pub enum Void{}

pub struct GlobalState {
    pub channels: Arc<RwLock<HashMap<Arc<str>, channel::Channel>>>,
    pub default_channels: Vec<Arc<str>>,
    pub users: Arc<RwLock<HashMap<Arc<str>, Arc<UserHandle>>>>,
}

pub struct UserHandle {
    pub outgoing: mpsc::Sender<message::Message>,
}

pub struct User {
    name: Arc<str>,
    client: String,
    channels: Arc<RwLock<HashMap<Arc<str>, UserChannel>>>,
    global_state: Arc<GlobalState>,
}

impl Drop for User {
    fn drop(&mut self) {
        let mut users_write = self.global_state.users.write();
        users_write.remove(&*self.name);
    }
}

struct UserChannel {
    incoming: channel::IncomingSink,
    parted: oneshot::Sender<Void>,
}

enum State {
    Continue(Box<dyn Future<Output=Result<State, Box<dyn std::error::Error>>> + Send>),
    Finished,
}

async fn handle_common_messages(ws: WebSocketStream<TcpStream>, user: Arc<User>, outgoing_receiver: mpsc::Receiver<message::Message>) -> impl Stream<Item=message::Message> + Unpin {
    let (ws_sink, ws_stream) = ws.split();
    let (mut outgoing_chat_sender, outgoing_chat_receiver) = mpsc::channel::<Arc<tungstenite::Message>>(0);
    task::spawn(async move {
        let outgoing_receiver = outgoing_receiver.map(|message| Ok(message.into_ws_message().unwrap()));
        let outgoing_chat_receiver = outgoing_chat_receiver.map(|message| Ok((&*message).clone()));
        let _ = stream::select(outgoing_receiver, outgoing_chat_receiver).forward(ws_sink).await;
    });
    let ws_stream = {
        let user = user.clone();
        let outgoing_chat_sender = outgoing_chat_sender.clone();
        ws_stream.scan((), move |_, ws_message| {
            let user = user.clone();
            let mut outgoing_chat_sender = outgoing_chat_sender.clone();
            async move {
                let ws_message = ws_message.ok()?;
                let ws_data = ws_message.into_data();
                let message = corepack::from_bytes::<message::Message>(&ws_data).ok()?;
                println!("message received: {:?}", message);

                let target = match &message {
                    message::Message{data: message::Data::Cmsg(message::Cmsg{target, ..}), ..} => target,
                    message::Message{data: message::Data::GReadyState(message::GReadyState{target, ..}), ..} => target,
                    message::Message{data: message::Data::Join(message::Join{target, ..}), ..} => {
                        join_channel(user.global_state.clone(), &mut outgoing_chat_sender, &*user, Arc::from(&**target)).await;
                        return Some(None);
                    },
                    message::Message{data: message::Data::Part(message::Part{target, ..}), ..} => {
                        let mut channels_write = user.channels.write();
                        channels_write.remove(&**target);
                        return Some(None);
                    },
                    _ => {
                        return Some(Some(message));
                    }
                };

                if let Some(mut channel_incoming) = {
                    let channels_read = user.channels.read();
                    let channel_incoming = channels_read.get(&**target).map(|c| c.incoming.clone());
                    channel_incoming
                } {
                    channel_incoming.send(message).await;
                }
                Some(None)
            }.boxed()
        }).filter_map(future::ready)
    };
    join_default_channels(user.global_state.clone(), &mut outgoing_chat_sender, &*user).await;
    ws_stream
}

async fn join_default_channels(global_state: Arc<GlobalState>, ws_sink: &mut mpsc::Sender<Arc<tungstenite::Message>>, user: &User) {
    for channel_name in &global_state.default_channels {
        join_channel(global_state.clone(), ws_sink, user, channel_name.clone()).await;
    }
}

async fn join_channel(global_state: Arc<GlobalState>, ws_sink: &mut mpsc::Sender<Arc<tungstenite::Message>>, user: &User, channel_name: Arc<str>) {
    if {
        let channels_read = user.channels.read();
        channels_read.contains_key(&*channel_name)
    } {
        ws_sink.send(Arc::new(message::Error{message: format!("You're already in {}", channel_name), code: "already_in_channel".to_string()}.into_ws_message().unwrap())).await;
        return;
    }

    loop {
        let mut channel_join_sender = {
            let channels_read = global_state.channels.upgradable_read();
            if let Some(channel_join_sender) = channels_read.get(&*channel_name).map(|s| s.join_sender.clone()) {
                channel_join_sender
            } else {
                let mut channels_write = RwLockUpgradableReadGuard::upgrade(channels_read);
                let channel_join_sender = channels_write.entry(channel_name.clone()).or_insert(channel::channel(global_state.clone(), channel_name.clone())).join_sender.clone();
                channel_join_sender
            }
        };

        let (join_response_sender, join_response_receiver) = oneshot::channel();
        let _ = channel_join_sender.send(channel::JoinRequest {
            name: user.name.clone(),
            response: join_response_sender,
            outgoing: ws_sink.clone(),
        }).await;

        if let Ok(join_response) = join_response_receiver.await {
            let mut channels_write = user.channels.write();
            channels_write.insert(channel_name, UserChannel {
                incoming: join_response.incoming,
                parted: join_response.parted,
            });
            return;
        }
    }
}

async fn unauthed(global_state: Arc<GlobalState>, mut ws: WebSocketStream<TcpStream>) -> Result<State, Box<dyn std::error::Error>> {
    while let Some(ws_message) = ws.next().await {
        let ws_data = &*ws_message?.into_data();
        let message = corepack::from_bytes::<message::Message>(ws_data)?;
        println!("message received: {:?}", message);

        match message.data {
            message::Data::Auth(mut auth) => {
                if let Some(tripcode) = auth.name.rfind('#').map(|idx| auth.name.split_off(idx+1)) {
                    auth.name.truncate(auth.name.len()-1);
                }
                let name: Arc<str> = Arc::from(&*auth.name);

                let (ws_sink, outgoing_receiver) = mpsc::channel::<message::Message>(0);
                let user = {
                    let global_state_clone = global_state.clone();
                    let mut users_write = global_state.users.write();
                    match users_write.entry(name.clone()) {
                        hash_map::Entry::Occupied(_) => None,
                        hash_map::Entry::Vacant(vacant) => {
                            vacant.insert(Arc::new(UserHandle {
                                outgoing: ws_sink.clone(),
                            }));
                            Some(Arc::new(User {
                                name: name.clone(),
                                client: auth.client,
                                channels: Default::default(),
                                global_state: global_state_clone,
                            }))
                        }
                    }
                };
                let user = if let Some(user) = user {
                    user
                } else {
                    let _ = ws.send(message::Error{message: format!("The name {} is unavailable.", name), code: "name_unavailable".to_string()}.into_ws_message()?).await;
                    continue;
                };

                let _ = ws.send_all(&mut stream::iter(vec![
                    message::Motd{message: "hello\nworld!".into()}.into_ws_message().unwrap(),
                    {
                        let channels_read = user.global_state.channels.read();
                        let channels = channels_read.iter().map(|(channel_name, channel)| {
                            message::ChannelUpdate{target: channel_name.to_string(), users: channel.count_users()}
                        }).collect();
                        message::ChannelList{channels}.into_ws_message().unwrap()
                    },
                ]).map(Ok)).await;

                let ws_stream = handle_common_messages(ws, user.clone(), outgoing_receiver).await;

                return Ok(State::Continue(Box::new(idling(ws_sink, ws_stream, user))));
            },
            _ => {
                let _ = ws.send(message::Error{message: "You must authenticate before sending other commands.".to_string(), code: "unauthorized".to_string()}.into_ws_message()?).await;
            },
        }
    }
    Ok(State::Finished)
}

async fn idling<S: Sink<message::Message> + Unpin, ST: Stream<Item=message::Message> + Unpin>(ws_sink: S, mut ws_stream: ST, user: Arc<User>) -> Result<State, Box<dyn std::error::Error>> {
    while let Some(message) = ws_stream.next().await {
        match message.data {
            _ => panic!("unhandled message: {:?}", message)
        }
    }
    Ok(State::Finished)
}

async fn accept_websocket(global_state: Arc<GlobalState>, stream: TcpStream) -> Result<(), Box<dyn std::error::Error>> {
    let ws = async_tungstenite::accept_async(stream).await?;

    let mut state: Fuse<Pin<Box<dyn Future<Output=Result<State, Box<dyn std::error::Error>>> + Send>>> = unauthed(global_state, ws).boxed().fuse();
    while let Ok(State::Continue(new_state)) = (&mut state).await {
        state = Pin::from(new_state).fuse();
    }

    println!("finished");

    Ok(())
}

#[async_std::main]
async fn main() -> std::io::Result<()> {
    let default_channels = vec![Arc::from("#lobby")];

    let global_state = Arc::new(GlobalState {
        channels: Default::default(),
        default_channels,
        users: Default::default(),
    });

    let listener = TcpListener::bind("127.0.0.1:28420").await?;
    while let Some(stream) = listener.incoming().next().await {
        let global_state = global_state.clone();
        let _ = task::spawn(async {
            let stream = stream.expect("a stream");
            if let Err(e) = accept_websocket(global_state, stream).await {
                println!("Error accepting websocket: {}", e);
            }
        });
    }
    Ok(())
}
