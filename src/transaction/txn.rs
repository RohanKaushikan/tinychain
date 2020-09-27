use std::collections::{HashMap, HashSet};
use std::convert::TryInto;
use std::fmt;
use std::hash::Hash;
use std::iter;
use std::sync::Arc;

use futures::future::{self, TryFutureExt};
use futures::stream::{self, FuturesUnordered, Stream, StreamExt};
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::auth::Auth;
use crate::block::{BlockData, Dir, DirEntry, File};
use crate::chain::ChainInstance;
use crate::class::{Instance, State, TCBoxTryFuture, TCResult, TCStream};
use crate::collection::class::CollectionInstance;
use crate::error;
use crate::gateway::{Gateway, NetworkTime};
use crate::lock::RwLock;
use crate::value::class::ValueInstance;
use crate::value::link::PathSegment;
use crate::value::op::{Method, Op, OpDef, OpRef};
use crate::value::{Number, TCString, Value, ValueId};

use super::Transact;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub struct TxnId {
    timestamp: u128, // nanoseconds since Unix epoch
    nonce: u16,
}

impl TxnId {
    pub fn new(time: NetworkTime) -> TxnId {
        TxnId {
            timestamp: time.as_nanos(),
            nonce: rand::thread_rng().gen(),
        }
    }

    pub fn zero() -> TxnId {
        TxnId {
            timestamp: 0,
            nonce: 0,
        }
    }
}

impl Ord for TxnId {
    fn cmp(&self, other: &TxnId) -> std::cmp::Ordering {
        if self.timestamp == other.timestamp {
            self.nonce.cmp(&other.nonce)
        } else {
            self.timestamp.cmp(&other.timestamp)
        }
    }
}

impl PartialOrd for TxnId {
    fn partial_cmp(&self, other: &TxnId) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Into<PathSegment> for TxnId {
    fn into(self) -> PathSegment {
        self.to_string().parse().unwrap()
    }
}

impl Into<String> for TxnId {
    fn into(self) -> String {
        format!("{}-{}", self.timestamp, self.nonce)
    }
}

impl fmt::Display for TxnId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}-{}", self.timestamp, self.nonce)
    }
}

pub struct Txn {
    id: TxnId,
    dir: Arc<Dir>,
    context: ValueId,
    gateway: Arc<Gateway>,
    mutated: RwLock<Vec<Box<dyn Transact>>>,
}

impl Txn {
    pub async fn new(gateway: Arc<Gateway>, workspace: Arc<Dir>) -> TCResult<Arc<Txn>> {
        let id = TxnId::new(Gateway::time());
        let context: PathSegment = id.clone().try_into()?;
        let dir = workspace.create_dir(&id, &context.clone().into()).await?;

        println!("new Txn: {}", id);

        Ok(Arc::new(Txn {
            id,
            dir,
            context,
            gateway,
            mutated: RwLock::new(vec![]),
        }))
    }

    pub async fn context<T: BlockData>(&self) -> TCResult<Arc<File<T>>>
    where
        Arc<File<T>>: Into<DirEntry>,
    {
        self.dir
            .create_file(self.id.clone(), self.context.clone())
            .await
    }

    pub async fn subcontext(&self, subcontext: ValueId) -> TCResult<Arc<Txn>> {
        let dir = self
            .dir
            .get_or_create_dir(&self.id, &self.context.clone().into())
            .await?;

        Ok(Arc::new(Txn {
            id: self.id.clone(),
            dir,
            context: subcontext,
            gateway: self.gateway.clone(),
            mutated: self.mutated.clone(),
        }))
    }

    pub fn subcontext_tmp<'a>(&'a self) -> TCBoxTryFuture<'a, Arc<Txn>> {
        Box::pin(async move {
            let id = self.dir.unique_id(self.id()).await?;
            self.subcontext(id).await
        })
    }

    pub fn id(&'_ self) -> &'_ TxnId {
        &self.id
    }

    pub async fn execute<S: Stream<Item = (ValueId, Value)> + Unpin>(
        self: Arc<Self>,
        mut parameters: S,
        auth: Auth,
    ) -> TCResult<State> {
        // TODO: use a Graph here and queue every op absolutely as soon as it's ready

        println!("Txn::execute");

        let mut graph = HashMap::new();
        let mut capture = None;
        while let Some((name, value)) = parameters.next().await {
            if let Value::TCString(TCString::Ref(tc_ref)) = value {
                return Err(error::bad_request(
                    &format!("Tried to assign {} to a reference", name),
                    tc_ref,
                ));
            }

            capture = Some(name.clone());
            graph.insert(name, State::Value(value));
        }

        let capture = capture.ok_or(error::unsupported("Cannot execute empty operation"))?;

        let mut pending = FuturesUnordered::new();

        while !is_resolved(graph.get(&capture).ok_or(error::not_found(&capture))?) {
            let mut visited = HashSet::new();
            let mut unvisited = Vec::with_capacity(graph.len());
            unvisited.push(capture.clone());
            while let Some(name) = unvisited.pop() {
                if visited.contains(&name) {
                    println!("Already visited {}", name);
                    continue;
                } else {
                    visited.insert(name.clone());
                }

                println!("Txn::execute {} (#{})", &name, visited.len());

                let state = graph.get(&name).ok_or_else(|| error::not_found(&name))?;
                if let State::Value(Value::Op(op)) = state {
                    if op.is_def() {
                        continue;
                    }

                    println!("Provider: {}", &op);

                    let mut ready = true;
                    for dep in requires(op, &graph)? {
                        let dep_state = graph.get(&dep).ok_or_else(|| error::not_found(&dep))?;

                        if !is_resolved(dep_state) {
                            ready = false;
                            unvisited.push(dep);
                        }
                    }

                    if ready {
                        pending.push(
                            self.clone()
                                .resolve(graph.clone(), *op.clone(), auth.clone())
                                .map_ok(|state| (name, state)),
                        );
                    }
                }
            }

            while let Some(result) = pending.next().await {
                let (name, state) = result?;
                graph.insert(name, state);
            }
        }

        graph.remove(&capture).ok_or(error::not_found(capture))
    }

    pub async fn execute_and_stream<S: Stream<Item = (ValueId, Value)> + Unpin>(
        self: Arc<Self>,
        parameters: S,
        auth: Auth,
    ) -> TCResult<TCStream<Value>> {
        let state = self.clone().execute(parameters, auth).await?;
        match state {
            State::Chain(chain) => chain.to_stream(self).await,
            State::Cluster(_cluster) => {
                // TODO
                let stream: TCStream<Value> = Box::pin(stream::empty());
                TCResult::Ok(stream)
            }
            State::Collection(collection) => {
                let stream: TCStream<Value> = collection.to_stream(self).await?;
                TCResult::Ok(stream)
            }
            State::Value(value) => {
                let stream: TCStream<Value> = Box::pin(stream::once(future::ready(value)));
                TCResult::Ok(stream)
            }
        }
    }

    pub async fn commit(&self) {
        println!("commit!");

        future::join_all(self.mutated.write().await.drain(..).map(|s| async move {
            s.commit(&self.id).await;
        }))
        .await;
    }

    pub async fn rollback(&self) {
        println!("rollback!");

        future::join_all(self.mutated.write().await.drain(..).map(|s| async move {
            s.rollback(&self.id).await;
        }))
        .await;
    }

    pub async fn resolve(
        self: Arc<Self>,
        provided: HashMap<ValueId, State>,
        provider: Op,
        auth: Auth,
    ) -> TCResult<State> {
        println!("Txn::resolve {}", provider);

        match provider {
            Op::Def(op_def) => Err(error::not_implemented(format!("Txn::resolve {}", op_def))),
            Op::Ref(OpRef::If(cond, then, or_else)) => {
                let cond = provided
                    .get(cond.value_id())
                    .ok_or_else(|| error::not_found(cond))?;
                if let State::Value(Value::Number(Number::Bool(cond))) = cond {
                    if cond.into() {
                        Ok(State::Value(then))
                    } else {
                        Ok(State::Value(or_else))
                    }
                } else {
                    Err(error::bad_request(
                        "Expected a boolean condition but found",
                        cond,
                    ))
                }
            }
            Op::Ref(OpRef::Get(link, key)) => {
                let object = resolve_value(&provided, &key)?.clone();
                println!("object {}", object);
                self.gateway
                    .clone()
                    .get(&link, object, auth, Some(self.clone()))
                    .await
            }
            Op::Method(Method::Get(tc_ref, path, key)) => {
                let subject = provided
                    .get(tc_ref.value_id())
                    .ok_or_else(|| error::not_found(tc_ref))?;
                let key = resolve_value(&provided, &key)?;

                println!("Method::Get subject {}: {}", subject, key);

                match subject {
                    State::Value(value) => value.get(path, key.clone()).map(State::Value),
                    State::Chain(chain) => {
                        println!("Txn::resolve Chain {}: {}", path, key);
                        chain.get(self.clone(), &path, key.clone(), auth).await
                    }
                    State::Cluster(cluster) => {
                        println!("Txn::resolve Cluster {}: {}", path, key);
                        cluster
                            .get(
                                self.gateway.clone(),
                                Some(self.clone()),
                                path,
                                key.clone(),
                                auth,
                            )
                            .await
                    }
                    other => {
                        self.mutate(subject.clone()).await;
                        Err(error::not_implemented(format!(
                            "Txn::resolve Method::Get {}",
                            other.class()
                        )))
                    }
                }
            }
            Op::Ref(OpRef::Put(link, key, value)) => {
                let value = resolve_state(&provided, &value)?;
                self.gateway
                    .clone()
                    .put(&link, key, value, &auth, Some(self.clone()))
                    .await?;

                Ok(().into())
            }
            Op::Method(Method::Put(tc_ref, path, key, value)) => {
                let subject = provided
                    .get(&tc_ref.clone().into())
                    .ok_or_else(|| error::not_found(tc_ref))?;
                let value = resolve_state(&provided, &value)?;

                println!(
                    "Txn::resolve Method::Put {}{}: {} <- {}",
                    subject, path, key, value
                );

                match subject {
                    State::Value(_) => Err(error::unsupported(
                        "Value is immutable (doesn't support PUT)",
                    )),
                    State::Chain(chain) => {
                        self.mutate(chain.clone().into()).await;

                        chain
                            .put(self.clone(), path, key, value)
                            .map_ok(State::from)
                            .await
                    }
                    other => Err(error::not_implemented(format!(
                        "Txn::resolve Method::Put for {}",
                        other
                    ))),
                }
            }
            Op::Ref(OpRef::Post(link, data)) => {
                let data = stream::iter(data).map(move |(name, value)| {
                    // TODO: just allow sending an error as a value
                    resolve_value(&provided, &value)
                        .map(|value| (name, value.clone()))
                        .unwrap()
                });

                self.gateway
                    .post(&link, data, auth)
                    .map_ok(State::from)
                    .await
            }
            Op::Method(Method::Post(_subject, _path, _data)) => {
                Err(error::not_implemented("Txn::resolve Method::Post"))
            }
        }
    }

    pub async fn mutate(self: &Arc<Self>, state: State) {
        let state: Box<dyn Transact> = match state {
            State::Chain(chain) => Box::new(chain),
            State::Cluster(cluster) => Box::new(cluster),
            State::Collection(collection) => Box::new(collection),
            State::Value(_) => panic!("Value does not support transaction-specific mutations!"),
        };

        self.mutated.write().await.push(state)
    }
}

fn resolve_state(provided: &HashMap<ValueId, State>, object: &Value) -> TCResult<State> {
    match object {
        Value::TCString(TCString::Ref(object)) => match provided.get(object.value_id()) {
            Some(state) => Ok(state.clone()),
            None => Err(error::not_found(object)),
        },
        other => Ok(State::Value(other.clone())),
    }
}

fn resolve_value<'a>(
    provided: &'a HashMap<ValueId, State>,
    object: &'a Value,
) -> TCResult<&'a Value> {
    match object {
        Value::TCString(TCString::Ref(object)) => match provided.get(object.value_id()) {
            Some(State::Value(object)) => Ok(object),
            Some(other) => Err(error::bad_request("Expected Value but found", other)),
            None => Err(error::not_found(object)),
        },
        other => Ok(other),
    }
}

fn is_resolved(state: &State) -> bool {
    match state {
        State::Value(value) => is_resolved_value(value),
        _ => true,
    }
}

fn is_resolved_value(value: &Value) -> bool {
    match value {
        Value::Op(op) => match **op {
            Op::Ref(_) => false,
            _ => true,
        },
        Value::TCString(TCString::Ref(_)) => false,
        Value::Tuple(values) => values.iter().all(is_resolved_value),
        _ => true,
    }
}

fn requires(op: &Op, txn_state: &HashMap<ValueId, State>) -> TCResult<HashSet<ValueId>> {
    let mut deps = HashSet::new();

    match op {
        Op::Def(OpDef::Get((tc_ref, _))) => {
            deps.insert(tc_ref.clone());
        }
        Op::Def(OpDef::Put((tc_ref, _, _))) => {
            deps.insert(tc_ref.clone());
        }
        Op::Def(OpDef::Post(_)) => {}
        Op::Method(method) => match method {
            Method::Get(subject, _path, key) => {
                deps.insert(subject.value_id().clone());
                deps.extend(value_requires(key, txn_state)?);
            }
            Method::Put(subject, _path, key, value) => {
                deps.insert(subject.value_id().clone());
                deps.extend(value_requires(key, txn_state)?);
                deps.extend(value_requires(value, txn_state)?);
            }
            Method::Post(_subject, _path, _data) => {}
        },
        Op::Ref(op_ref) => match op_ref {
            OpRef::If(cond, then, or_else) => {
                let cond_state = txn_state
                    .get(cond.value_id())
                    .ok_or_else(|| error::not_found(cond))?;
                if let State::Value(Value::Op(cond_op)) = cond_state {
                    deps.extend(requires(cond_op, txn_state)?);
                } else {
                    deps.extend(value_requires(then, txn_state)?);
                    deps.extend(value_requires(or_else, txn_state)?);
                }
            }
            OpRef::Get(_path, key) => {
                deps.extend(value_requires(key, txn_state)?);
            }
            OpRef::Put(_path, key, value) => {
                deps.extend(value_requires(key, txn_state)?);
                deps.extend(value_requires(value, txn_state)?);
            }
            OpRef::Post(_path, data) => {
                for (_id, provider) in data {
                    deps.extend(value_requires(provider, txn_state)?);
                }
            }
        },
    }

    Ok(deps)
}

fn value_requires(
    value: &Value,
    txn_state: &HashMap<ValueId, State>,
) -> TCResult<HashSet<ValueId>> {
    match value {
        Value::Op(op) => requires(op, txn_state),
        Value::TCString(TCString::Ref(tc_ref)) => {
            Ok(iter::once(tc_ref.value_id().clone()).collect())
        }
        _ => Ok(HashSet::new()),
    }
}
