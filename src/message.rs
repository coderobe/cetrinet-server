use derive_more::From;
use serde::{Serialize, Deserialize};
use async_tungstenite::tungstenite;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Error {
    #[serde(rename = "m")]
    pub message: String,
    #[serde(rename = "c")]
    pub code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Auth {
    #[serde(rename = "n")]
    pub name: String,
    #[serde(rename = "c")]
    pub client: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Motd {
    #[serde(rename = "m")]
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelList {
    #[serde(rename = "c")]
    pub channels: Vec<ChannelUpdate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelUpdate {
    #[serde(rename = "t")]
    pub target: String,
    #[serde(rename = "u")]
    pub users: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Join {
    #[serde(rename = "u")]
    pub user: String,
    #[serde(rename = "t")]
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Part {
    #[serde(rename = "u")]
    pub user: String,
    #[serde(rename = "t")]
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserList {
    #[serde(rename = "t")]
    pub target: String,
    #[serde(rename = "u")]
    pub users: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cmsg {
    #[serde(rename = "m")]
    pub message: String,
    #[serde(rename = "t")]
    pub target: String,
    #[serde(rename = "s")]
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GReadyState {
    #[serde(rename = "r")]
    pub ready: bool,
    #[serde(rename = "t")]
    pub target: String,
    #[serde(rename = "s")]
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GStart {
    #[serde(rename = "t")]
    pub target: String,
    #[serde(rename = "u")]
    pub users: Vec<String>,
}

#[derive(Debug, Clone, From, Serialize, Deserialize)]
#[serde(tag = "t", content = "d")]
pub enum Data {
    #[serde(rename = "error")]
    Error(Error),
    #[serde(rename = "auth")]
    Auth(Auth),
    #[serde(rename = "motd")]
    Motd(Motd),
    #[serde(rename = "channellist")]
    ChannelList(ChannelList),
    #[serde(rename = "channelupdate")]
    ChannelUpdate(ChannelUpdate),
    #[serde(rename = "join")]
    Join(Join),
    #[serde(rename = "part")]
    Part(Part),
    #[serde(rename = "userlist")]
    UserList(UserList),
    #[serde(rename = "cmsg")]
    Cmsg(Cmsg),
    #[serde(rename = "greadystate")]
    GReadyState(GReadyState),
    #[serde(rename = "gstart")]
    GStart(GStart),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    #[serde(rename = "v")]
    pub version: u8,
    #[serde(flatten)]
    pub data: Data,
}

impl<D> From<D> for Message where D: Into<Data> {
    fn from(data: D) -> Self {
        Message {
            version: 1,
            data: data.into(),
        }
    }
}

pub trait IntoWebsocketMessage {
    fn into_ws_message(self) -> Result<tungstenite::Message, corepack::error::Error>;
}
impl<M> IntoWebsocketMessage for M where M: Into<Message> {
    fn into_ws_message(self) -> Result<tungstenite::Message, corepack::error::Error> {
        Ok(tungstenite::Message::binary(corepack::to_bytes(self.into())?))
    }
}
