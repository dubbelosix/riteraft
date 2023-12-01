#[macro_use]
extern crate slog;
extern crate slog_async;
extern crate slog_term;

use async_trait::async_trait;
use bincode::{deserialize, serialize};
use log::info;
use riteraft::{Mailbox, Raft, Result as RaftResult, Store};
use serde::{Deserialize, Serialize};
use slog::Drain;
use structopt::StructOpt;
use warp::{reply, Filter};

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use borsh::{BorshSerialize, BorshDeserialize};

#[derive(Debug, StructOpt)]
struct Options {
    #[structopt(long)]
    raft_addr: String,
    #[structopt(long)]
    peer_addr: Option<String>,
    #[structopt(long)]
    web_server: Option<String>,
}

#[derive(BorshSerialize, BorshDeserialize)]
struct PutRequest {
    k: String,
    v: Vec<u8>,
}

#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum Message {
    Insert { key: String, value: Vec<u8> },
}

#[derive(Clone)]
struct HashStore(Arc<RwLock<HashMap<String, Vec<u8>>>>);

impl HashStore {
    fn new() -> Self {
        Self(Arc::new(RwLock::new(HashMap::new())))
    }
    fn get(&self, key: String) -> Option<Vec<u8>> {
        self.0.read().unwrap().get(&key).cloned()
    }
}

#[async_trait]
impl Store for HashStore {
    async fn apply(&mut self, message: &[u8]) -> RaftResult<Vec<u8>> {
        let message: Message =  BorshDeserialize::try_from_slice(message).unwrap();
        let message: Vec<u8> = match message {
            Message::Insert { key, value } => {
                let mut db = self.0.write().unwrap();
                db.insert(key.clone(), value.clone());
                value
            }
        };
        Ok(message)
    }

    async fn snapshot(&self) -> RaftResult<Vec<u8>> {
        Ok(serialize(&self.0.read().unwrap().clone())?)
    }

    async fn restore(&mut self, snapshot: &[u8]) -> RaftResult<()> {
        let new: HashMap<String, Vec<u8>> = deserialize(snapshot).unwrap();
        let mut db = self.0.write().unwrap();
        let _ = std::mem::replace(&mut *db, new);
        Ok(())
    }
}

fn with_mailbox(
    mailbox: Arc<Mailbox>,
) -> impl Filter<Extract = (Arc<Mailbox>,), Error = Infallible> + Clone {
    warp::any().map(move || mailbox.clone())
}

fn with_store(store: HashStore) -> impl Filter<Extract = (HashStore,), Error = Infallible> + Clone {
    warp::any().map(move || store.clone())
}

async fn put(
    mailbox: Arc<Mailbox>,
    body: bytes::Bytes,
) -> Result<impl warp::Reply, Infallible> {
    let request: PutRequest = BorshDeserialize::try_from_slice(&body).unwrap();
    let message = Message::Insert { key: request.k, value: request.v };
    let message = BorshSerialize::try_to_vec(&message).unwrap();
    let _ = mailbox.send(message).await.unwrap();
    Ok(reply::json(&"OK".to_string()))
}

async fn get(store: HashStore, key: String) -> Result<impl warp::Reply, Infallible> {
    let response = store.get(key);
    Ok(reply::json(&response))
}

async fn leave(mailbox: Arc<Mailbox>) -> Result<impl warp::Reply, Infallible> {
    mailbox.leave().await.unwrap();
    Ok(reply::json(&"OK".to_string()))
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    let logger = slog::Logger::root(drain, slog_o!("version" => env!("CARGO_PKG_VERSION")));

    // converts log to slog
    let _log_guard = slog_stdlog::init().unwrap();

    let options = Options::from_args();
    let store = HashStore::new();

    let raft = Raft::new(options.raft_addr, store.clone(), logger.clone());
    let mailbox = Arc::new(raft.mailbox());
    let (raft_handle, mailbox) = match options.peer_addr {
        Some(addr) => {
            info!("running in follower mode");
            let handle = tokio::spawn(raft.join(addr));
            (handle, mailbox)
        }
        None => {
            info!("running in leader mode");
            let handle = tokio::spawn(raft.lead());
            (handle, mailbox)
        }
    };

    let put_kv = warp::post()
        .and(warp::path("put"))
        .and(with_mailbox(mailbox.clone()))
        .and(warp::body::bytes())
        .and_then(put);

    let get_kv = warp::get()
        .and(warp::path!("get" / String))
        .and(with_store(store.clone()))
        .and_then(|key, store: HashStore| get(store, key));

    let leave_kv = warp::get()
        .and(warp::path!("leave"))
        .and(with_mailbox(mailbox.clone()))
        .and_then(leave);

    let routes = put_kv.or(get_kv).or(leave_kv);

    if let Some(addr) = options.web_server {
        let _server = tokio::spawn(async move {
            warp::serve(routes)
                .run(SocketAddr::from_str(&addr).unwrap())
                .await;
        });
    }

    let result = tokio::try_join!(raft_handle)?;
    result.0?;
    Ok(())
}
