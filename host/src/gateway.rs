use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use serde::de::DeserializeOwned;

use error::*;
use futures::future::{try_join_all, Future, TryFutureExt};
use generic::{path_label, NetworkTime, PathLabel, TCPathBuf};

use crate::http;
use crate::kernel::Kernel;
use crate::scalar::{Link, LinkHost, LinkProtocol, Value};
use crate::state::State;
use crate::txn::*;

const PATH: PathLabel = path_label(&["host", "gateway"]);

#[async_trait]
pub trait Client {
    async fn fetch<T: DeserializeOwned>(
        &self,
        txn_id: &TxnId,
        link: &Link,
        key: &Value,
    ) -> TCResult<T>;

    async fn get(&self, txn: Txn, link: Link, key: Value, auth: Option<String>) -> TCResult<State>;

    async fn put(
        &self,
        txn_id: Txn,
        link: Link,
        key: Value,
        value: State,
        auth: Option<String>,
    ) -> TCResult<()>;

    async fn post(
        &self,
        txn: Txn,
        link: Link,
        params: State,
        auth: Option<String>,
    ) -> TCResult<State>;

    async fn delete(
        &self,
        txn_id: TxnId,
        link: Link,
        key: Value,
        auth: Option<String>,
    ) -> TCResult<()>;
}

#[async_trait]
pub trait Server {
    type Error: std::error::Error;

    async fn listen(self, addr: SocketAddr) -> Result<(), Self::Error>;
}

pub struct Gateway {
    actor: Actor,
    kernel: Kernel,
    txn_server: TxnServer,
    addr: IpAddr,
    http_port: u16,
    client: http::Client,
}

impl Gateway {
    pub fn time() -> NetworkTime {
        NetworkTime::now()
    }

    pub fn new(kernel: Kernel, txn_server: TxnServer, addr: IpAddr, http_port: u16) -> Arc<Self> {
        let actor_id = Value::from(Link::from(TCPathBuf::from(PATH)));
        let actor = Actor::new(actor_id);

        Arc::new(Self {
            actor,
            kernel,
            addr,
            txn_server,
            http_port,
            client: http::Client::new(),
        })
    }

    fn sign_token(&self, txn: &Txn) -> TCResult<Option<String>> {
        let signed = self
            .actor
            .sign_token(txn.request().token())
            .map_err(TCError::internal)?;
        Ok(Some(signed))
    }

    pub fn root(&self) -> Link {
        let host = LinkHost::from((LinkProtocol::HTTP, self.addr.clone(), Some(self.http_port)));
        host.into()
    }

    pub async fn new_txn(self: &Arc<Self>, txn_id: TxnId, token: Option<String>) -> TCResult<Txn> {
        self.txn_server.new_txn(self.clone(), txn_id, token).await
    }

    pub async fn fetch<T: DeserializeOwned>(
        &self,
        txn_id: &TxnId,
        subject: &Link,
        key: &Value,
    ) -> TCResult<T> {
        self.client.fetch(txn_id, subject, key).await
    }

    pub async fn get(&self, txn: &Txn, subject: Link, key: Value) -> TCResult<State> {
        if subject.host().is_none() {
            self.kernel.get(txn, subject.path(), key).await
        } else {
            let auth = self.sign_token(txn)?;
            self.client.get(txn.clone(), subject, key, auth).await
        }
    }

    pub async fn put(&self, txn: &Txn, subject: Link, key: Value, value: State) -> TCResult<()> {
        if subject.host().is_none() {
            self.kernel.put(txn, subject.path(), key, value).await
        } else {
            let auth = self.sign_token(txn)?;
            self.client
                .put(txn.clone(), subject, key, value, auth)
                .await
        }
    }

    pub async fn post(&self, txn: &Txn, subject: Link, params: State) -> TCResult<State> {
        if subject.host().is_none() {
            self.kernel.post(txn, subject.path(), params).await
        } else {
            let auth = self.sign_token(txn)?;
            self.client.post(txn.clone(), subject, params, auth).await
        }
    }

    pub fn listen(
        self: Arc<Self>,
    ) -> Pin<Box<impl Future<Output = Result<(), Box<dyn std::error::Error>>>>> {
        let servers = vec![self.http_listen()];

        Box::pin(try_join_all(servers).map_ok(|_| ()))
    }

    fn http_listen(
        self: Arc<Self>,
    ) -> std::pin::Pin<Box<impl futures::Future<Output = Result<(), Box<dyn std::error::Error>>>>>
    {
        let port = self.http_port;
        let http_addr = (self.addr, port).into();
        let server = crate::http::HTTPServer::new(self);
        let listener = server.listen(http_addr).map_err(|e| {
            let e: Box<dyn std::error::Error> = Box::new(e);
            e
        });

        Box::pin(listener)
    }
}
