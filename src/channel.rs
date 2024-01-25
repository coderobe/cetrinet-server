use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::pin::Pin;
use futures::Sink;
use futures::channel::{mpsc, oneshot};
use futures::select;
use futures::future::FutureExt;
use futures::stream::{StreamExt, futures_unordered::FuturesUnordered};
use futures::sink::{self, SinkExt};
//use async_std::prelude::*;
use async_std::task;
use async_tungstenite::tungstenite;
use crate::message::{self, IntoWebsocketMessage as _};
use crate::{Void, GlobalState};

#[derive(Clone)]
struct User {
    outgoing: mpsc::Sender<Arc<tungstenite::Message>>,
    ready: bool,
}

pub struct Channel {
    pub join_sender: mpsc::Sender<JoinRequest>,
    count_users: Arc<AtomicUsize>,
}

impl Channel {
    pub fn count_users(&self) -> usize {
        self.count_users.load(Ordering::Acquire)
    }
}

pub struct JoinRequest {
    pub name: Arc<str>,
    pub response: oneshot::Sender<JoinResponse>,
    pub outgoing: mpsc::Sender<Arc<tungstenite::Message>>,
}

pub struct JoinResponse {
    pub incoming: IncomingSink,
    pub parted: oneshot::Sender<Void>,
}

#[derive(Clone)]
pub struct IncomingSink {
    name: Arc<str>,
    sender: mpsc::Sender<(Arc<str>, message::Message)>,
}

impl Sink<message::Message> for IncomingSink {
    type Error = mpsc::SendError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut task::Context) -> task::Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.sender).poll_ready(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: message::Message) -> Result<(), Self::Error> {
        let mut self_ref = &mut *self;
        Pin::new(&mut self_ref.sender).start_send((self_ref.name.clone(), item))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut task::Context) -> task::Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.sender).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut task::Context) -> task::Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.sender).poll_close(cx)
    }
}

pub fn channel(global_state: Arc<GlobalState>, name: Arc<str>) -> Channel {
    let mut users = HashMap::new();
    let count_users = Arc::new(AtomicUsize::new(0));
    let (join_sender, mut join_receiver) = mpsc::channel::<JoinRequest>(0);
    let (incoming_sender, mut incoming_receiver) = mpsc::channel(0);
    let mut parted_name_receiver = FuturesUnordered::new();


    task::spawn({
        let count_users = count_users.clone();
        async move {
            loop {
                select! {
                    join_request = join_receiver.next() => {
                        if let Some(join_request) = join_request {
                            let (parted_sender, parted_receiver) = oneshot::channel();

                            if join_request.response.send(JoinResponse {
                                incoming: IncomingSink {
                                    name: join_request.name.clone(),
                                    sender: incoming_sender.clone(),
                                },
                                parted: parted_sender,
                            }).is_err() {
                                continue;
                            };

                            // TODO: this should be an Rc<RefCell<User>>, or msg shouldn't need mut
                            let mut user = User {
                                outgoing: join_request.outgoing,
                                ready: false,
                            };

                            users.insert(join_request.name.clone(), user.clone());
                            {
                                let user_name = join_request.name.clone();
                                parted_name_receiver.push(parted_receiver.map(move |_| user_name));
                            }
                            let count_users_fetched = count_users.fetch_add(1, Ordering::AcqRel)+1;
                            msg(users.values_mut(), message::Join{user: join_request.name.to_string(), target: name.to_string()});
                            msg_all(&*global_state, message::ChannelUpdate{target: name.to_string(), users: count_users_fetched});
                            msg(std::iter::once(&mut user.clone()), message::UserList{target: name.to_string(), users: users.keys().map(ToString::to_string).collect()});
                            for ready_user_name in users.iter().filter(|(_, v)| v.ready).map(|(k, _)| k.to_string()) {
                                msg(std::iter::once(&mut user.clone()), message::GReadyState{target: name.to_string(), source: ready_user_name.to_string(), ready: true});
                            }
                        } else {
                            unimplemented!("channel deletion");
                        }
                    },
                    (source, message) = incoming_receiver.select_next_some() => {
                        match message {
                            message::Message{data: message::Data::Cmsg(mut cmsg), ..} => {
                                cmsg.source = source.to_string();
                                msg(users.values_mut(), cmsg);
                            },
                            message::Message{data: message::Data::GReadyState(mut greadystate), ..} => {
                                if let Some(user) = users.get_mut(&*source) {
                                    user.ready = greadystate.ready;
                                    greadystate.source = source.to_string();
                                    msg(users.values_mut(), greadystate);
                                }
                            },
                            message::Message{data: message::GStart(mut gstart), ..} => {
                                if let Some(user) = users.get_mut(&*source) {
                                    if !user.ready {
                                        msg(std::iter::once(&mut user), message::Error{message: "You must be ready to start a game".to_string(), code: "unauthorized"});
                                        continue;
                                    }

                                    if current_game.is_some() {
                                        msg(std::iter::once(&mut user), message::Error{message: "A game is already running".to_string(), code: "already_running".to_string()});
                                        continue;
                                    }
                                } else {
                                    continue;
                                }
                                current_game = Some(game(users.iter().filter(|(_, v)| v.ready).map(|(k, v)| (k.clone(), v.game_outgoing.clone()))));
                            },
                            _ => unimplemented!("unknown message sent to chat: {:?}", message),
                        }
                    },
                    user_name = parted_name_receiver.select_next_some() => {
                        msg(users.values_mut(), message::Part{user: user_name.to_string(), target: name.to_string()});
                        users.remove(&*user_name);
                        let count_users_fetched = count_users.fetch_sub(1, Ordering::AcqRel)-1;
                        msg_all(&*global_state, message::ChannelUpdate{target: name.to_string(), users: count_users_fetched});
                        if users.is_empty() {
                            // TODO: if someone deletes the channel, and we receive a part before we receive the delete notification, we're deleting the wrong thing
                            let mut channels_write = global_state.channels.write();
                            channels_write.remove(&*name);
                            return;
                        }
                    },
                }
            }
        }
    });

    Channel {
        join_sender,
        count_users,
    }
}

// TODO: Channel updates can be implemented as, per channel, an Arc<RwLock<(Version, Message)>>, and per channel per user, a Waker
fn msg_all(global_state: &GlobalState, message: impl Into<message::Message>) {
    let message = message.into();
    let users_read = global_state.users.read();
    for user in users_read.values() {
        let mut outgoing = user.outgoing.clone();
        let message = message.clone();
        task::spawn(async move {
            let _ = outgoing.send(message.clone()).await;
        });
    }
}

fn msg<'a, I: IntoIterator<Item=&'a mut User>, M: Into<message::Message>>(users: I, message: M) {
    let message = Arc::new(message.into().into_ws_message().unwrap());
    for user in users {
        if let Err(_) = user.outgoing.try_send(message.clone()) {
            let _ = user.outgoing.clone().try_send(Arc::new(message::Error{message: "Kicked due to full buffers.".to_string(), code: "kicked".to_string()}.into_ws_message().unwrap()));
            // TODO: Delete the user - this will, eventually, trigger them to part
        }
    }
}
