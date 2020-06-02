use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::TryInto;
use std::fmt;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use futures::future::{self, join_all, try_join_all, Future, FutureExt};
use futures::lock::Mutex;
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::auth::Token;
use crate::error;
use crate::host::{Host, NetworkTime};
use crate::internal::Dir;
use crate::state::State;
use crate::value::link::*;
use crate::value::op::*;
use crate::value::*;

#[async_trait]
pub trait Transact: Send + Sync {
    async fn commit(&self, txn_id: &TxnId);

    async fn rollback(&self, txn_id: &TxnId);
}

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
}

impl PartialOrd for TxnId {
    fn partial_cmp(&self, other: &TxnId) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
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

#[derive(Default)]
struct TxnState<'a> {
    known: HashSet<TCRef>,
    queue: VecDeque<(ValueId, Request, &'a Option<Token>)>,
    resolved: HashMap<ValueId, State>,
}

impl<'a> TxnState<'a> {
    fn new() -> TxnState<'a> {
        TxnState {
            known: HashSet::new(),
            queue: VecDeque::new(),
            resolved: HashMap::new(),
        }
    }

    fn extend<I: Iterator<Item = (ValueId, Value)>>(
        &mut self,
        values: I,
        auth: &'a Option<Token>,
    ) -> TCResult<()> {
        for item in values {
            self.push(item, auth)?
        }

        Ok(())
    }

    fn push(&mut self, value: (ValueId, Value), auth: &'a Option<Token>) -> TCResult<()> {
        if self.resolved.contains_key(&value.0) {
            return Err(error::bad_request("Duplicate value provided for", value.0));
        }
        self.known.insert(value.0.clone().into());

        match value.1 {
            Value::Request(request) => {
                let required = request.deps();
                let unknown: Vec<&TCRef> = required.difference(&self.known).collect();
                if !unknown.is_empty() {
                    let unknown: Value = unknown.into_iter().cloned().collect();
                    Err(error::bad_request(
                        "Some required values were not provided",
                        unknown,
                    ))
                } else {
                    if required.is_empty() {
                        self.queue.push_front((value.0, *request, auth));
                    } else {
                        self.queue.push_back((value.0, *request, auth));
                    }

                    Ok(())
                }
            }
            _ => {
                self.resolved.insert(value.0, value.1.into());
                Ok(())
            }
        }
    }

    async fn resolve(
        &mut self,
        txn: Arc<Txn<'a>>,
        capture: Vec<ValueId>,
    ) -> TCResult<HashMap<ValueId, State>> {
        // TODO: Don't resolve any GET op unless it's required by a captured value

        let mut resolved: HashMap<ValueId, State> = self.resolved.drain().collect();
        while !self.queue.is_empty() {
            let known: HashSet<TCRef> = resolved.keys().cloned().map(|id| id.into()).collect();
            let mut ready = vec![];
            let mut value_ids = vec![];
            while let Some((value_id, op, auth)) = self.queue.pop_front() {
                if op.deps().is_subset(&known) {
                    ready.push(txn.resolve_value(&resolved, value_id.clone(), op, auth));
                    println!("ready: {}", value_id);
                    value_ids.push(value_id);
                } else {
                    self.queue.push_front((value_id, op, auth));
                    break;
                }
            }

            let values = try_join_all(ready).await?.into_iter().map(|s| {
                println!("resolved {}", value_ids[0]);
                (value_ids.remove(0), s)
            });
            resolved.extend(values);
            println!("{} remaining to resolve", self.queue.len());
        }

        let resolved = resolved
            .drain()
            .filter(|(id, _)| capture.contains(id))
            .collect();

        Ok(resolved)
    }
}

pub struct Txn<'a> {
    id: TxnId,
    context: Arc<Dir>,
    host: Arc<Host>,
    mutated: Arc<RwLock<Vec<Arc<dyn Transact>>>>, // TODO: this should be a Set of some kind
    state: Mutex<TxnState<'a>>,
}

impl<'a> Txn<'a> {
    pub async fn new(host: Arc<Host>, root: Arc<Dir>) -> TCResult<Arc<Txn<'a>>> {
        let id = TxnId::new(host.time());
        let context: PathSegment = id.clone().try_into()?;
        let context = root.create_dir(&id, context.into()).await?;
        let state = Mutex::new(TxnState::new());

        Ok(Arc::new(Txn {
            id,
            context,
            host,
            mutated: Arc::new(RwLock::new(vec![])),
            state,
        }))
    }

    pub fn context(self: &Arc<Self>) -> Arc<Dir> {
        self.context.clone()
    }

    pub async fn subcontext(self: &Arc<Self>, subcontext: ValueId) -> TCResult<Arc<Txn<'a>>> {
        let subcontext: Arc<Dir> = self.context.create_dir(&self.id, subcontext.into()).await?;

        Ok(Arc::new(Txn {
            id: self.id.clone(),
            context: subcontext,
            host: self.host.clone(),
            mutated: self.mutated.clone(),
            state: Mutex::new(TxnState::default()),
        }))
    }

    pub fn id(self: &Arc<Self>) -> TxnId {
        self.id.clone()
    }

    pub async fn extend<I: Iterator<Item = (ValueId, Value)>>(
        &self,
        iter: I,
        auth: &'a Option<Token>,
    ) -> TCResult<()> {
        self.state.lock().await.extend(iter, auth)
    }

    pub async fn push(&self, item: (ValueId, Value), auth: &'a Option<Token>) -> TCResult<()> {
        self.state.lock().await.push(item, auth)
    }

    pub fn commit(&'a self) -> impl Future<Output = ()> + 'a {
        println!("commit!");
        join_all(self.mutated.write().unwrap().drain(..).map(|s| async move {
            s.commit(&self.id).await;
        }))
        .then(|_| future::ready(()))
    }

    pub fn rollback(&'a self) -> impl Future<Output = ()> + 'a {
        println!("rollback!");
        join_all(self.mutated.write().unwrap().drain(..).map(|s| async move {
            s.rollback(&self.id).await;
        }))
        .then(|_| future::ready(()))
    }

    pub fn mutate(self: &Arc<Self>, state: Arc<dyn Transact>) {
        self.mutated.write().unwrap().push(state)
    }

    pub async fn resolve(
        self: &Arc<Self>,
        capture: Vec<ValueId>,
    ) -> TCResult<HashMap<ValueId, State>> {
        self.state.lock().await.resolve(self.clone(), capture).await
    }

    async fn resolve_value(
        self: &Arc<Self>,
        resolved: &HashMap<ValueId, State>,
        value_id: ValueId,
        request: Request,
        auth: &Option<Token>,
    ) -> TCResult<State> {
        let extension = self.subcontext(value_id).await?;
        let subject = request.subject();

        match request.op().clone() {
            Op::Get(GetOp { key }) => match subject {
                Subject::Link(l) => extension.get(l.clone(), key, auth).await,
                Subject::Ref(r) => match resolved.get(&r.value_id()) {
                    Some(s) => s.get(&extension, key, auth).await,
                    None => Err(error::bad_request(
                        "Required value not provided",
                        r.value_id(),
                    )),
                },
            },
            Op::Put(PutOp { key, value }) => match subject {
                Subject::Link(l) => {
                    extension
                        .put(l.clone(), key, resolve_val(resolved, value)?, auth)
                        .await
                }
                Subject::Ref(r) => {
                    let subject = resolve_id(resolved, &r.value_id())?;
                    let key = resolve_val(resolved, key)?;
                    let value = resolve_val(resolved, value)?;
                    println!("{}.put({}, {})", subject, key, value);
                    subject
                        .put(&extension, key.try_into()?, value.try_into()?, auth)
                        .await
                }
            },
            Op::Post(PostOp { action, requires }) => match subject {
                Subject::Ref(r) => {
                    let mut deps: Vec<(ValueId, Value)> = Vec::with_capacity(requires.len());
                    for (dest_id, id) in requires {
                        let dep = resolve_val(resolved, id)?;
                        deps.push((dest_id, dep.try_into()?));
                    }

                    let subject = resolve_id(resolved, &r.value_id())?;
                    subject
                        .post(extension, &action.try_into()?, deps, auth)
                        .await
                }
                Subject::Link(l) => Err(error::method_not_allowed(l)),
            },
        }
    }

    pub fn time(&self) -> NetworkTime {
        NetworkTime::from_nanos(self.id.timestamp)
    }

    pub async fn get(
        self: &Arc<Self>,
        link: Link,
        key: Value,
        auth: &Option<Token>,
    ) -> TCResult<State> {
        println!("txn::get {} {}", link, key);
        self.host.get(self, &link, key, auth).await
    }

    pub async fn put(
        self: &Arc<Self>,
        dest: Link,
        key: Value,
        state: State,
        auth: &Option<Token>,
    ) -> TCResult<State> {
        println!("txn::put {} {}", dest, key);
        self.host.put(self, dest, key, state, auth).await
    }
}

fn resolve_id(resolved: &HashMap<ValueId, State>, id: &ValueId) -> TCResult<State> {
    match resolved.get(id) {
        Some(s) => Ok(s.clone()),
        None => Err(error::bad_request("Required value not provided", id)),
    }
}

fn resolve_val(resolved: &HashMap<ValueId, State>, value: Value) -> TCResult<State> {
    match value {
        Value::Ref(r) => resolve_id(resolved, &r.value_id()),
        Value::Vector(mut v) => {
            let mut val: Vec<Value> = vec![];
            for item in v.drain(..) {
                match resolve_val(resolved, item)? {
                    State::Value(i) => val.push(i),
                    other => {
                        return Err(error::bad_request(
                            "State {} cannot be serialized into a Value",
                            other,
                        ))
                    }
                }
            }

            Ok(Value::Vector(val).into())
        }
        _ => Ok(value.into()),
    }
}
